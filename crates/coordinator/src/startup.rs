use crate::{
    api::routes::{
        add_event_entry, admin_competition_fragment, admin_create_competition_handler,
        admin_fee_estimates_fragment, admin_page_handler, admin_send_bitcoin_handler,
        admin_wallet_address_fragment, admin_wallet_balance_fragment, admin_wallet_fragment,
        admin_wallet_outputs_fragment, change_password, competitions_fragment,
        competitions_rows_fragment, create_competition, entries_fragment, entry_detail_fragment,
        entry_form_fragment, forgot_password_challenge, forgot_password_reset,
        get_aggregate_nonces, get_balance, get_competitions, get_contract_parameters, get_entries,
        get_estimated_fee_rates, get_next_address, get_outputs, get_ticket_status, health,
        leaderboard_fragment, leaderboard_rows_fragment, login, login_username, payouts_fragment,
        public_page_handler, register, register_username, request_competition_ticket,
        send_to_address, submit_final_signatures, submit_public_nonces, submit_ticket_payout,
    },
    config::Settings,
    domain::{
        CompetitionStore, CompetitionWatcher, Coordinator, InvoiceWatcher, PayoutWatcher, UserInfo,
        UserStore,
    },
    infra::{
        bitcoin::{Bitcoin, BitcoinClient, BitcoinSyncWatcher},
        db::{DBConnection, DatabasePoolConfig, DatabaseType},
        file_utils::create_folder,
        keymeld::create_keymeld_service,
        lightning::{Ln, LnClient},
        oracle::{Oracle, OracleClient},
    },
};

// Mock implementations only available with e2e-testing feature or debug builds
#[cfg(any(feature = "e2e-testing", debug_assertions))]
use crate::infra::{
    bitcoin_mock::MockBitcoinClient, lightning_mock::MockLnClient, oracle_mock::MockOracle,
};
use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{connect_info::IntoMakeServiceWithConnectInfo, ConnectInfo, Path, Request, State},
    http::{header, Extensions, HeaderValue, StatusCode},
    middleware::{self, AddExtension, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    serve::Serve,
    Router,
};
use dlctix::secp::Scalar;
use hyper::{
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
    Method,
};
use log::{error, info, warn};
use reqwest_middleware::{
    reqwest::{self, Client, Url},
    ClientBuilder, ClientWithMiddleware, Middleware,
};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use std::{collections::HashMap, net::SocketAddr, str::FromStr};
use std::{sync::Arc, time::Duration};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::RwLock;
use tokio::{net::TcpListener, select, task::JoinHandle};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower_http::cors::{AllowOrigin, CorsLayer};
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
        let (app_state, background_tasks, cancellation_token) = build_app(config.clone()).await?;
        let server = build_server(listener, app_state, config.api_settings.origins).await?;
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
    pub private_url: String,
    pub remote_url: String,
    pub oracle_url: String,
    pub esplora_url: String,
    pub bitcoin: Arc<dyn Bitcoin>,
    pub coordinator: Arc<Coordinator>,
    pub users_info: Arc<UserInfo>,
    pub background_threads: Arc<HashMap<String, JoinHandle<()>>>,
    pub forgot_password_challenges: Arc<RwLock<HashMap<String, (String, std::time::Instant)>>>,
}

pub async fn build_app(
    config: Settings,
) -> Result<(AppState, TaskTracker, CancellationToken), anyhow::Error> {
    info!(
        "Static UI assets configured at {}",
        config.ui_settings.ui_dir
    );

    // Create Bitcoin client (real or mock based on config)
    #[cfg(any(feature = "e2e-testing", debug_assertions))]
    let bitcoin_client: Arc<dyn Bitcoin> = if config.bitcoin_settings.mock_enabled {
        info!("Mock Bitcoin client configured");
        Arc::new(MockBitcoinClient::new(config.bitcoin_settings.network))
    } else {
        let client = BitcoinClient::new(&config.bitcoin_settings)
            .await
            .map(Arc::new)?;
        info!("Bitcoin service configured");
        client
    };

    #[cfg(not(any(feature = "e2e-testing", debug_assertions)))]
    let bitcoin_client: Arc<dyn Bitcoin> = {
        if config.bitcoin_settings.mock_enabled {
            return Err(anyhow!(
                "Mock Bitcoin client requires e2e-testing feature or debug build"
            ));
        }
        let client = BitcoinClient::new(&config.bitcoin_settings)
            .await
            .map(Arc::new)?;
        info!("Bitcoin service configured");
        client
    };

    let reqwest_client = build_reqwest_client();

    // Create LN client (real or mock based on config)
    #[cfg(any(feature = "e2e-testing", debug_assertions))]
    let ln: Arc<dyn Ln> = if config.ln_settings.mock_enabled {
        let mock_ln = if let Some(auto_accept_secs) = config.ln_settings.mock_auto_accept_secs {
            MockLnClient::with_auto_accept(Duration::from_secs(auto_accept_secs))
        } else {
            MockLnClient::new()
        };
        mock_ln.ping().await?;
        info!(
            "Mock LN client configured (auto_accept: {:?})",
            config.ln_settings.mock_auto_accept_secs
        );
        Arc::new(mock_ln)
    } else {
        let ln_client = LnClient::new(reqwest_client.clone(), config.ln_settings.clone())
            .await
            .map(Arc::new)?;
        ln_client.ping().await?;
        info!("LND client configured");
        ln_client
    };

    #[cfg(not(any(feature = "e2e-testing", debug_assertions)))]
    let ln: Arc<dyn Ln> = {
        if config.ln_settings.mock_enabled {
            return Err(anyhow!(
                "Mock LN client requires e2e-testing feature or debug build"
            ));
        }
        let ln_client = LnClient::new(reqwest_client.clone(), config.ln_settings.clone())
            .await
            .map(Arc::new)?;
        ln_client.ping().await?;
        info!("LND client configured");
        ln_client
    };

    // Create Oracle client (real or mock based on config)
    #[cfg(any(feature = "e2e-testing", debug_assertions))]
    let oracle_client: Arc<dyn Oracle> = if config.coordinator_settings.mock_oracle {
        info!("Mock Oracle configured");
        Arc::new(MockOracle::new([0u8; 32]))
    } else {
        let oracle_url = Url::parse(&config.coordinator_settings.oracle_url)
            .map_err(|e| anyhow!("Failed to parse oracle url: {}", e))?;
        let real_oracle = OracleClient::new(
            reqwest_client,
            &oracle_url,
            &config.coordinator_settings.private_key_file,
        )?;
        info!("Oracle client configured");
        Arc::new(real_oracle)
    };

    #[cfg(not(any(feature = "e2e-testing", debug_assertions)))]
    let oracle_client: Arc<dyn Oracle> = {
        if config.coordinator_settings.mock_oracle {
            return Err(anyhow!(
                "Mock Oracle requires e2e-testing feature or debug build"
            ));
        }
        let oracle_url = Url::parse(&config.coordinator_settings.oracle_url)
            .map_err(|e| anyhow!("Failed to parse oracle url: {}", e))?;
        let real_oracle = OracleClient::new(
            reqwest_client,
            &oracle_url,
            &config.coordinator_settings.private_key_file,
        )?;
        info!("Oracle client configured");
        Arc::new(real_oracle)
    };
    create_folder(&config.db_settings.data_folder.clone());

    let pool_config: DatabasePoolConfig = config.db_settings.clone().into();

    let competition_db = DBConnection::new(
        &config.db_settings.data_folder,
        "competitions",
        pool_config.clone(),
        DatabaseType::Competitions,
    )
    .await
    .map_err(|e| anyhow!("Error setting up competition db: {}", e))?;

    let competition_store = CompetitionStore::new(competition_db);

    let users_db = DBConnection::new(
        &config.db_settings.data_folder,
        "users",
        pool_config.clone(),
        DatabaseType::Users,
    )
    .await
    .map_err(|e| anyhow!("Error setting up users db: {}", e))?;

    let users_store = UserStore::new(users_db);

    // Create Keymeld service
    // Get the coordinator's private key for keymeld credentials
    let coordinator_private_key: Scalar = bitcoin_client.get_derived_private_key().await?;
    let private_key_bytes: [u8; 32] = coordinator_private_key.serialize();

    // Generate a UUID v7 for the coordinator user ID
    // UUID v7 is time-based, which ensures consistent ordering with ticket IDs (also UUID v7).
    // This is critical because participant ordering must be deterministic - tickets are created
    // after the coordinator starts, so the coordinator's UUID v7 will always sort before tickets.
    let coordinator_user_id = uuid::Uuid::now_v7();

    let keymeld_service = create_keymeld_service(
        config.keymeld_settings.clone(),
        coordinator_user_id,
        &private_key_bytes,
    )
    .map_err(|e| anyhow!("Failed to create keymeld service: {}", e))?;

    if config.keymeld_settings.enabled {
        info!("Keymeld service configured (enabled)");
    } else {
        info!("Keymeld service configured (disabled - using local MuSig2)");
    }

    let keymeld_gateway_url = if config.keymeld_settings.enabled {
        Some(config.keymeld_settings.gateway_url.clone())
    } else {
        None
    };

    let coordinator = Coordinator::new(
        oracle_client,
        competition_store,
        bitcoin_client.clone(),
        ln.clone(),
        keymeld_service,
        keymeld_gateway_url,
        config
            .coordinator_settings
            .relative_locktime_block_delta
            .into(),
        config.coordinator_settings.required_confirmations,
        config.coordinator_settings.name,
        config.coordinator_settings.escrow_enabled,
        config.coordinator_settings.invoice_settlement_confirmations,
    )
    .await
    .map(Arc::new)?;

    if config.coordinator_settings.escrow_enabled {
        info!("Escrow transactions enabled");
    } else {
        info!("Escrow transactions disabled (using HODL invoices only)");
    }

    info!("Coordinator service configured");

    let tracker = TaskTracker::new();
    let mut threads = HashMap::new();
    let cancel_token = CancellationToken::new();
    let competition_watcher = CompetitionWatcher::new(
        coordinator.clone(),
        cancel_token.clone(),
        Duration::from_secs(config.coordinator_settings.sync_interval_secs),
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

    let payout_watcher = PayoutWatcher::new(
        coordinator.clone(),
        ln.clone(),
        cancel_token.clone(),
        Duration::from_secs(config.ln_settings.payout_watch_interval),
    );

    let payout_watcher_handle = tokio::spawn(async move {
        if let Err(e) = payout_watcher.watch().await {
            error!("Payout watcher error: {}", e);
        }
    });

    threads.insert("payout_watcher".to_string(), payout_watcher_handle);

    let app_state = AppState {
        ui_dir: config.ui_settings.ui_dir,
        private_url: config.ui_settings.private_url,
        remote_url: config.ui_settings.remote_url,
        esplora_url: config.bitcoin_settings.esplora_url,
        oracle_url: config.coordinator_settings.oracle_url,
        coordinator,
        users_info: Arc::new(UserInfo::new(users_store)),
        bitcoin: bitcoin_client,
        background_threads: Arc::new(threads),
        forgot_password_challenges: Arc::new(RwLock::new(HashMap::new())),
    };
    Ok((app_state, tracker, cancel_token))
}

pub async fn build_server(
    socket_addr: SocketAddr,
    app_state: AppState,
    origins: Vec<String>,
) -> Result<
    Serve<
        TcpListener,
        IntoMakeServiceWithConnectInfo<Router, SocketAddr>,
        AddExtension<Router, ConnectInfo<SocketAddr>>,
    >,
    anyhow::Error,
> {
    let listener = TcpListener::bind(socket_addr).await?;

    info!("Setting up service");
    let app = app(app_state, origins);
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

pub fn app(app_state: AppState, origins: Vec<String>) -> Router {
    let origins: Vec<HeaderValue> = origins
        .into_iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([ACCEPT, CONTENT_TYPE, AUTHORIZATION])
        .allow_origin(AllowOrigin::list(origins))
        .allow_credentials(true);

    let wallet_endpoints = Router::new()
        .route("/balance", get(get_balance))
        .route("/address", get(get_next_address))
        .route("/outputs", get(get_outputs))
        .route("/send", post(send_to_address))
        .route("/estimated_fees", get(get_estimated_fee_rates));

    let users_endpoints = Router::new()
        .route("/login", post(login))
        .route("/register", post(register))
        .route("/username/register", post(register_username))
        .route("/username/login", post(login_username))
        .route("/username/change-password", post(change_password))
        .route("/username/forgot-password", post(forgot_password_challenge))
        .route("/username/reset-password", post(forgot_password_reset));

    // HTMX admin routes (pure server-side rendering, no WASM)
    let admin_htmx_routes = Router::new()
        .route("/", get(admin_page_handler))
        .route("/competition", get(admin_competition_fragment))
        .route("/wallet", get(admin_wallet_fragment))
        .route("/wallet/balance", get(admin_wallet_balance_fragment))
        .route("/wallet/address", get(admin_wallet_address_fragment))
        .route("/wallet/fees", get(admin_fee_estimates_fragment))
        .route("/wallet/outputs", get(admin_wallet_outputs_fragment))
        .route("/wallet/send", post(admin_send_bitcoin_handler))
        .route("/api/competitions", post(admin_create_competition_handler));

    // HTMX public routes (some require JS bridge for auth)
    let htmx_routes = Router::new()
        .route("/competitions", get(competitions_fragment))
        .route("/competitions/rows", get(competitions_rows_fragment))
        .route(
            "/competitions/{competition_id}/entry-form",
            get(entry_form_fragment),
        )
        .route(
            "/competitions/{competition_id}/leaderboard",
            get(leaderboard_fragment),
        )
        .route(
            "/competitions/{competition_id}/leaderboard/rows",
            get(leaderboard_rows_fragment),
        )
        .route("/entries", get(entries_fragment))
        .route("/entries/{entry_id}/detail", get(entry_detail_fragment))
        .route("/payouts", get(payouts_fragment));

    Router::new()
        .route("/", get(public_page_handler))
        .nest("/admin", admin_htmx_routes)
        .merge(htmx_routes)
        .fallback(public_page_handler)
        .route("/api/v1/health_check", get(health))
        .route("/api/v1/competitions", post(create_competition))
        .route("/api/v1/competitions", get(get_competitions))
        .route(
            "/api/v1/competitions/{competition_id}/ticket",
            post(request_competition_ticket),
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
        .route(
            "/api/v1/competitions/{competitionId}/entries/{entryId}/payout",
            post(submit_ticket_payout),
        )
        .route("/api/v1/entries", post(add_event_entry))
        .route("/api/v1/entries", get(get_entries))
        .nest("/api/v1/wallet", wallet_endpoints)
        .nest("/api/v1/users", users_endpoints)
        .route("/ui/{*path}", get(serve_static_file))
        .layer(middleware::from_fn(log_request))
        .with_state(Arc::new(app_state))
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

async fn serve_static_file(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    // Prevent directory traversal attacks
    if path.contains("..") {
        return (StatusCode::BAD_REQUEST, "Bad request").into_response();
    }

    let file_path = std::path::Path::new(&state.ui_dir).join(&path);

    let content = match tokio::fs::read(&file_path).await {
        Ok(c) => c,
        Err(_) => return (StatusCode::NOT_FOUND, "Not found").into_response(),
    };

    let mime_type = get_mime_type(&path);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_type)
        .body(Body::from(content))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Server error").into_response())
}

fn get_mime_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        // JavaScript
        "js" | "mjs" => "application/javascript; charset=utf-8",
        // CSS
        "css" => "text/css; charset=utf-8",
        // HTML
        "html" | "htm" => "text/html; charset=utf-8",
        // JSON
        "json" | "map" => "application/json",
        // Images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        // Fonts
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        // Other
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "wasm" => "application/wasm",
        // Default
        _ => "application/octet-stream",
    }
}

pub fn build_reqwest_client() -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    ClientBuilder::new(Client::new())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .with(LoggingMiddleware)
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
    ) -> reqwest_middleware::Result<reqwest::Response> {
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
