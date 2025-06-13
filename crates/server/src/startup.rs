use crate::{
    add_event_entry, admin_index_handler, create_event, create_folder,
    domain::{CompetitionStore, CompetitionWatcher, DBConnection, InvoiceWatcher, UserInfo},
    get_aggregate_nonces, get_balance, get_competitions, get_contract_parameters, get_entries,
    get_estimated_fee_rates, get_next_address, get_outputs, get_ticket_status, health,
    index_handler, login, register, request_competition_ticket, send_to_address,
    submit_final_signatures, submit_public_nonces, BitcoinClient, BitcoinSyncWatcher, Coordinator,
    Ln, LnClient, OracleClient, Settings, UserStore,
};
use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{connect_info::IntoMakeServiceWithConnectInfo, ConnectInfo, Request},
    http::Extensions,
    middleware::{self, AddExtension, Next},
    response::IntoResponse,
    routing::{get, post},
    serve::Serve,
    Router,
};
use hyper::{
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
    Method,
};
use log::{error, info, warn};
use reqwest_middleware::{
    reqwest::{self, Client, Response, Url},
    ClientBuilder, ClientWithMiddleware, Middleware,
};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use std::{collections::HashMap, net::SocketAddr, str::FromStr};
use std::{sync::Arc, time::Duration};
use tokio::signal::unix::{signal, SignalKind};
use tokio::{net::TcpListener, select, task::JoinHandle};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower_http::{
    cors::{Any, CorsLayer},
    services::{ServeDir, ServeFile},
    set_status::SetStatus,
};
pub struct Application {
    server: Serve<
        TcpListener,
        IntoMakeServiceWithConnectInfo<Router, SocketAddr>,
        AddExtension<Router, ConnectInfo<SocketAddr>>,
    >,
    cancellation_token: CancellationToken,
    background_tasks: TaskTracker,
}

impl Application {
    pub async fn build(config: Settings) -> Result<Self, anyhow::Error> {
        let address = format!(
            "{}:{}",
            config.api_settings.domain, config.api_settings.port
        );
        let listener = SocketAddr::from_str(&address)?;
        let (app_state, serve_dir, serve_admin_dir, background_tasks, cancellation_token) =
            build_app(config).await?;
        let server = build_server(listener, app_state, serve_dir, serve_admin_dir).await?;
        Ok(Self {
            server,
            cancellation_token,
            background_tasks,
        })
    }

    pub async fn run_until_stopped(self) -> Result<(), anyhow::Error> {
        info!("Starting server...");
        match self.server.with_graceful_shutdown(shutdown_signal()).await {
            Ok(_) => {
                info!("Server shutdown initiated");
                self.cancellation_token.cancel();

                let timeout = tokio::time::sleep(std::time::Duration::from_secs(10));
                select! {
                    _ = self.background_tasks.wait() => {
                        info!("Background tasks completed gracefully");
                    }
                    _ = timeout => {
                        warn!("Background tasks timed out during shutdown");
                    }
                }

                info!("Shutdown complete");
                Ok(())
            }
            Err(e) => {
                error!("Server shutdown error: {}", e);
                self.cancellation_token.cancel();

                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    self.background_tasks.wait(),
                )
                .await;

                Err(anyhow!("Error during server shutdown: {}", e))
            }
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub ui_dir: String,
    pub admin_ui_dir: String,
    pub private_url: String,
    pub remote_url: String,
    pub oracle_url: String,
    pub esplora_url: String,
    pub bitcoin: Arc<BitcoinClient>,
    pub coordinator: Arc<Coordinator>,
    pub users_info: Arc<UserInfo>,
    pub background_threads: Arc<HashMap<String, JoinHandle<()>>>,
}

pub async fn build_app(
    config: Settings,
) -> Result<
    (
        AppState,
        ServeDir<ServeFile>,
        ServeDir<SetStatus<ServeFile>>,
        TaskTracker,
        CancellationToken,
    ),
    anyhow::Error,
> {
    // The ui folder needs to be generated and have this relative path from where the binary is being run
    let serve_dir = ServeDir::new(config.ui_settings.ui_dir.clone())
        .not_found_service(ServeFile::new(format!(
            "{}/index.html",
            config.ui_settings.ui_dir
        )))
        .fallback(ServeFile::new(format!(
            "{}/index.html",
            config.ui_settings.ui_dir
        )));
    info!("Public UI configured");

    // The admin_ui folder needs to be generated and have this relative path from where the binary is being run
    let serve_admin_dir = ServeDir::new(config.ui_settings.admin_ui_dir.clone())
        .not_found_service(ServeFile::new(config.ui_settings.admin_ui_dir.clone()));
    info!("Admin UI configured");

    let bitcoin_client = BitcoinClient::new(&config.bitcoin_settings)
        .await
        .map(Arc::new)?;
    info!("Bitcoin service configured");

    let reqwest_client = build_reqwest_client();
    let ln = LnClient::new(reqwest_client.clone(), config.ln_settings.clone())
        .await
        .map(Arc::new)?;

    // TODO: add background thread to monitor how lnd node is doing, alert if there is an issue
    ln.ping().await?;
    info!("Lnd client configured");

    let orcale_url = Url::parse(&config.coordinator_settings.oracle_url)
        .map_err(|e| anyhow!("Failed to parse oracle url: {}", e))?;
    let oracle_client = OracleClient::new(
        reqwest_client,
        &orcale_url,
        &config.coordinator_settings.private_key_file,
    )?;
    info!("Oracle client configured");
    create_folder(&config.db_settings.data_folder.clone());
    let competition_db = DBConnection::new(&config.db_settings.data_folder, "competitions")
        .map_err(|e| anyhow!("Error setting up competition db: {}", e))?;
    let competition_store = CompetitionStore::new(competition_db)
        .map_err(|e| anyhow!("Error setting up competition store: {}", e))?;
    let users_db = DBConnection::new(&config.db_settings.data_folder, "users")
        .map_err(|e| anyhow!("Error setting up users db: {}", e))?;
    let users_store =
        UserStore::new(users_db).map_err(|e| anyhow!("Error setting up user store: {}", e))?;

    let coordinator = Coordinator::new(
        Arc::new(oracle_client),
        competition_store,
        bitcoin_client.clone(),
        ln.clone(),
        config
            .coordinator_settings
            .relative_locktime_block_delta
            .into(),
        config.coordinator_settings.required_confirmations,
    )
    .await
    .map(Arc::new)?;

    info!("Coordinator service configured");

    let tracker = TaskTracker::new();
    let mut threads = HashMap::new();
    let cancel_token = CancellationToken::new();
    let competition_watcher = CompetitionWatcher::new(
        coordinator.clone(),
        cancel_token.clone(),
        Duration::from_secs(30),
    );
    let competition_watcher_task = tracker.spawn(async move {
        match competition_watcher.watch().await {
            Ok(_) => {
                info!("Successfully shutdown competition watcher")
            }
            Err(e) => {
                error!("Error in competition watcher: {}", e)
            }
        }
    });

    let bitcoin_watcher = BitcoinSyncWatcher::new(
        bitcoin_client.clone(),
        cancel_token.clone(),
        Duration::from_secs(config.bitcoin_settings.refresh_blocks_secs),
    );

    let bitcoin_watcher_task = tracker.spawn(async move {
        match bitcoin_watcher.watch().await {
            Ok(_) => {
                info!("Successfully shutdown Bitcoin sync watcher")
            }
            Err(e) => {
                error!("Error in Bitcoin sync watcher: {}", e)
            }
        }
    });

    tracker.close();
    threads.insert(
        String::from("competition_watcher"),
        competition_watcher_task,
    );
    threads.insert(String::from("bitcoin_sync_watcher"), bitcoin_watcher_task);

    let invoice_watcher = InvoiceWatcher::new(
        coordinator.clone(),
        ln.clone(),
        cancel_token.clone(),
        Duration::from_secs(config.ln_settings.invoice_watch_interval),
    );

    let invoice_watcher_handle = tokio::spawn(async move {
        if let Err(e) = invoice_watcher.watch().await {
            error!("Invoice watcher error: {}", e);
        }
    });

    threads.insert("invoice_watcher".to_string(), invoice_watcher_handle);

    let app_state = AppState {
        ui_dir: config.ui_settings.ui_dir,
        private_url: config.ui_settings.private_url,
        remote_url: config.ui_settings.remote_url,
        admin_ui_dir: config.ui_settings.admin_ui_dir,
        esplora_url: config.bitcoin_settings.esplora_url,
        oracle_url: config.coordinator_settings.oracle_url,
        coordinator,
        users_info: Arc::new(UserInfo::new(users_store)),
        bitcoin: bitcoin_client,
        background_threads: Arc::new(threads),
    };
    Ok((app_state, serve_dir, serve_admin_dir, tracker, cancel_token))
}

pub async fn build_server(
    socket_addr: SocketAddr,
    app_state: AppState,
    serve_dir: ServeDir<ServeFile>,
    serve_admin_dir: ServeDir<SetStatus<ServeFile>>,
) -> Result<
    Serve<
        TcpListener,
        IntoMakeServiceWithConnectInfo<Router, SocketAddr>,
        AddExtension<Router, ConnectInfo<SocketAddr>>,
    >,
    anyhow::Error,
> {
    let std_listener = std::net::TcpListener::bind(socket_addr)?;
    let listener = TcpListener::from_std(std_listener)?;

    info!("Setting up service");
    let app = app(app_state, serve_dir, serve_admin_dir);
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    info!(
        "Service running @: http://{}:{}",
        socket_addr.ip(),
        socket_addr.port()
    );
    Ok(server)
}

pub fn app(
    app_state: AppState,
    serve_dir: ServeDir<ServeFile>,
    serve_admin_dir: ServeDir<SetStatus<ServeFile>>,
) -> Router {
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([ACCEPT, CONTENT_TYPE, AUTHORIZATION])
        .allow_origin(Any);

    let wallet_endpoints = Router::new()
        .route("/balance", get(get_balance))
        .route("/address", get(get_next_address))
        .route("/outputs", get(get_outputs))
        .route("/send", post(send_to_address))
        .route("/estimated_fees", get(get_estimated_fee_rates));

    let users_endpoints = Router::new()
        .route("/login", post(login))
        .route("/register", post(register));

    Router::new()
        .route("/", get(index_handler))
        .route("/admin", get(admin_index_handler))
        .fallback(index_handler)
        .route("/api/v1/health_check", get(health))
        .route("/api/v1/competitions", post(create_event))
        .route("/api/v1/competitions", get(get_competitions))
        .route(
            "/api/v1/competitions/{competition_id}/ticket",
            get(request_competition_ticket),
        )
        .route(
            "/api/v1/competitions/{competition_id}/tickets/{ticket_id}/status",
            get(get_ticket_status),
        )
        .route(
            "/api/v1/competitions/{id}/contract",
            get(get_contract_parameters),
        )
        .route(
            "/api/v1/competitions/{competition_id}/entries/{entry_id}/public_nonces",
            post(submit_public_nonces),
        )
        .route(
            "/api/v1/competitions/{id}/aggregate_nonces",
            get(get_aggregate_nonces),
        )
        .route(
            "/api/v1/competitions/{competition_id}/entries/{entry_id}/final_signatures",
            post(submit_final_signatures),
        )
        .route("/api/v1/entries", post(add_event_entry))
        .route("/api/v1/entries", get(get_entries))
        .nest("/api/v1/wallet", wallet_endpoints)
        .nest("/api/v1/users", users_endpoints)
        .layer(middleware::from_fn(log_request))
        .with_state(Arc::new(app_state))
        .nest_service("/ui", serve_dir.clone())
        .nest_service("/admin_ui", serve_admin_dir.clone())
        .layer(cors)
}

async fn log_request(request: Request<Body>, next: Next) -> impl IntoResponse {
    let now = time::OffsetDateTime::now_utc();
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_default();
    info!(target: "http_request","new request, {} {}", request.method().as_str(), path);

    let response = next.run(request).await;
    let response_time = time::OffsetDateTime::now_utc() - now;
    info!(target: "http_response", "response, code: {}, time: {}", response.status().as_str(), response_time);

    response
}

pub fn build_reqwest_client() -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    ClientBuilder::new(Client::new())
        .with(LoggingMiddleware)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}

struct LoggingMiddleware;

#[async_trait::async_trait]
impl Middleware for LoggingMiddleware {
    async fn handle(
        &self,
        req: reqwest::Request,
        extensions: &mut Extensions,
        next: reqwest_middleware::Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let method = req.method().clone();
        let url = req.url().clone();

        info!("Making {} request to: {}", method, url);

        let result = next.run(req, extensions).await;

        match &result {
            Ok(response) => {
                info!("{} {} -> Status: {}", method, url, response.status());
            }
            Err(error) => {
                warn!("{} {} -> Error: {:?}", method, url, error);
            }
        }

        result
    }
}

async fn shutdown_signal() {
    let mut sigint = signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");

    select! {
        _ = sigint.recv() => info!("Received SIGINT signal"),
        _ = sigterm.recv() => info!("Received SIGTERM signal"),
    }
}
