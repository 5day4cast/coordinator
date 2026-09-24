use crate::{
    api::admin_auth::{
        admin_login, admin_login_page, operator_response_headers, require_operator, AdminAccess,
    },
    api::nip98_replay::Nip98ReplayGuard,
    api::routes::{
        add_event_entry, admin_competition_fragment, admin_create_competition_handler,
        admin_delete_competition_handler, admin_fee_estimates_fragment, admin_page_handler,
        admin_send_bitcoin_handler, admin_settle_test_invoice_handler,
        admin_wallet_address_fragment, admin_wallet_balance_fragment, admin_wallet_fragment,
        admin_wallet_outputs_fragment, change_password, claim_ticket_payout, competitions_fragment,
        competitions_rows_fragment, create_competition, entries_fragment, entry_detail_fragment,
        entry_form_fragment, forgot_password_challenge, forgot_password_reset,
        get_aggregate_nonces, get_balance, get_competition, get_competitions,
        get_contract_parameters, get_entries, get_estimated_fee_rates, get_next_address,
        get_outputs, get_ticket_refund, get_ticket_status, health, leaderboard_fragment,
        leaderboard_rows_fragment, login, login_username, payouts_fragment, public_page_handler,
        register, register_username, request_competition_ticket, send_to_address,
        set_lightning_address, submit_final_signatures, submit_public_nonces, submit_ticket_payout,
    },
    config::Settings,
    domain::{
        leaderboard::Leaderboards, CompetitionRunners, CompetitionStore, CompetitionWakes,
        Coordinator, InvoiceSubscriber, InvoiceWatcher, PaymentSubscriber, PayoutWatcher, UserInfo,
        UserStore,
    },
    infra::{
        bitcoin::{Bitcoin, BitcoinClient, BitcoinSyncWatcher},
        db::{DBConnection, DatabasePoolConfig, DatabaseType},
        file_utils::create_folder,
        keymeld::create_keymeld_service,
        lightning::{Ln, LnClient},
        lnurl::{HttpsLnurlPay, LnurlPay},
        oracle::{Oracle, OracleClient},
    },
};

// Mock implementations only available with e2e-testing feature or debug builds
use crate::api::nip98_origins::Nip98Origins;
use crate::config::{APISettings, RateLimitSettings};
#[cfg(any(feature = "e2e-testing", debug_assertions))]
use crate::infra::{
    bitcoin_mock::MockBitcoinClient, lightning_mock::MockLnClient, lnurl_mock::MockLnurlPay,
    oracle_mock::MockOracle,
};
use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{connect_info::IntoMakeServiceWithConnectInfo, ConnectInfo, Path, Request, State},
    http::{header, Extensions, HeaderValue, StatusCode, Uri},
    middleware::{self, AddExtension, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    serve::Serve,
    Extension, Router,
};
use bitcoin::Network;
use dlctix::secp::Scalar;
use futures::FutureExt;
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
use tokio::{
    net::TcpListener,
    select,
    task::{AbortHandle, JoinHandle},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower_governor::{
    governor::GovernorConfigBuilder, key_extractor::PeerIpKeyExtractor, GovernorLayer,
};
use tower_http::cors::{AllowOrigin, CorsLayer};
type HttpServer = Serve<
    TcpListener,
    IntoMakeServiceWithConnectInfo<Router, SocketAddr>,
    AddExtension<Router, ConnectInfo<SocketAddr>>,
>;

/// Owns both HTTP listeners, the background producers, and the databases.
///
/// The public listener serves participants. The admin listener serves operator routes
/// behind `api::admin_auth`; it never shares a socket with the public router.
pub struct Application {
    server: HttpServer,
    admin_server: HttpServer,
    cancellation_token: CancellationToken,
    background_tasks: TaskTracker,
    background_abort_handles: Vec<AbortHandle>,
    db_connections: Vec<DBConnection>,
}

impl Application {
    pub async fn build(config: Settings) -> Result<Self, anyhow::Error> {
        config.validate()?;
        let address = format!(
            "{}:{}",
            config.api_settings.domain, config.api_settings.port
        );
        let listener = SocketAddr::from_str(&address)?;
        let admin_listener = config.admin_settings.listen_addr;
        let network = config.bitcoin_settings.network;
        // Fail on a missing operator token before connecting to LND or opening databases.
        let admin_access = Arc::new(AdminAccess::from_settings(&config.admin_settings)?);
        let (app_state, background_tasks, cancellation_token, db_connections) =
            build_app(config.clone()).await?;
        let background_abort_handles: Vec<_> = app_state
            .background_threads
            .values()
            .map(JoinHandle::abort_handle)
            .collect();
        let app_state = Arc::new(app_state);
        let servers = async {
            let server =
                build_server(listener, app(app_state.clone(), &config.api_settings)).await?;
            let admin_server = build_server(
                admin_listener,
                admin_app(app_state.clone(), admin_access, network),
            )
            .await?;
            Ok::<_, anyhow::Error>((server, admin_server))
        };
        let (server, admin_server) = match servers.await {
            Ok(servers) => servers,
            Err(error) => {
                cancellation_token.cancel();
                for handle in background_abort_handles {
                    handle.abort();
                }
                let _ = tokio::time::timeout(Duration::from_secs(5), background_tasks.wait()).await;
                for database in db_connections {
                    if let Err(close_error) = database.close().await {
                        error!("Database cleanup after HTTP bind failure failed: {close_error}");
                    }
                }
                return Err(error);
            }
        };
        Ok(Self {
            server,
            admin_server,
            cancellation_token,
            background_tasks,
            background_abort_handles,
            db_connections,
        })
    }

    pub async fn run_until_stopped(self) -> Result<(), anyhow::Error> {
        info!("Starting server...");
        let Application {
            server,
            admin_server,
            cancellation_token,
            background_tasks,
            background_abort_handles,
            db_connections,
        } = self;
        let stop_http = CancellationToken::new();
        let mut http = spawn_http(server, stop_http.clone());
        let mut admin_http = spawn_http(admin_server, stop_http.clone());
        let mut http_finished = false;
        let mut admin_http_finished = false;
        let mut shutdown_error = None;
        let writer_stopped = async {
            let waiters: Vec<_> = db_connections
                .iter()
                .map(|database| Box::pin(database.writer_stopped()))
                .collect();
            if waiters.is_empty() {
                std::future::pending::<()>().await;
            }
            futures::future::select_all(waiters).await;
        };
        select! {
            result = &mut http => {
                http_finished = true;
                shutdown_error = Some(anyhow!("HTTP server stopped unexpectedly: {result:?}"));
            }
            result = &mut admin_http => {
                admin_http_finished = true;
                shutdown_error = Some(anyhow!("Admin HTTP server stopped unexpectedly: {result:?}"));
            }
            () = shutdown_signal() => {}
            () = writer_stopped => {
                shutdown_error = Some(anyhow!("Database writer stopped unexpectedly"));
            }
            () = cancellation_token.cancelled() => {
                shutdown_error = Some(anyhow!("A background worker stopped unexpectedly"));
            }
        }
        stop_http.cancel();
        let (public_drain, admin_drain) = tokio::join!(
            drain_http("HTTP server", http, http_finished),
            drain_http("Admin HTTP server", admin_http, admin_http_finished),
        );
        for drain_error in [public_drain, admin_drain].into_iter().flatten() {
            shutdown_error.get_or_insert(drain_error);
        }
        cancellation_token.cancel();
        if tokio::time::timeout(Duration::from_secs(10), background_tasks.wait())
            .await
            .is_err()
        {
            shutdown_error
                .get_or_insert_with(|| anyhow!("Background tasks timed out during shutdown"));
            for handle in background_abort_handles {
                handle.abort();
            }
            if tokio::time::timeout(Duration::from_secs(5), background_tasks.wait())
                .await
                .is_err()
            {
                error!("Aborted background tasks did not finish before timeout");
            }
        }

        // Close admission, drain accepted writes, then close SQLite. A local
        // commit does not guarantee that Litestream has replicated it remotely.
        for db in db_connections {
            if let Err(error) = db.close().await {
                error!("Database shutdown failed: {error}");
                shutdown_error.get_or_insert(error);
            }
        }

        match shutdown_error {
            Some(error) => Err(error),
            None => {
                info!("Shutdown complete");
                Ok(())
            }
        }
    }
}

type HttpTask = JoinHandle<Result<(), std::io::Error>>;

fn spawn_http(server: HttpServer, stop: CancellationToken) -> HttpTask {
    tokio::spawn(async move { server.with_graceful_shutdown(stop.cancelled_owned()).await })
}

/// Wait up to 10 seconds for a listener to drain after `stop_http`; abort it afterwards.
async fn drain_http(name: &str, mut task: HttpTask, finished: bool) -> Option<anyhow::Error> {
    if finished {
        return None;
    }
    match tokio::time::timeout(Duration::from_secs(10), &mut task).await {
        Ok(Ok(Ok(()))) => None,
        Ok(result) => Some(anyhow!("{name} shutdown failed: {result:?}")),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Some(anyhow!("{name} drain timed out"))
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub ui_dir: String,
    pub private_url: String,
    pub remote_url: String,
    pub oracle_url: String,
    pub explorer_url: String,
    pub network: String,
    pub bitcoin: Arc<dyn Bitcoin>,
    pub coordinator: Arc<Coordinator>,
    pub users_info: Arc<UserInfo>,
    /// Leaderboards and the oracle weather pages show, served from a background cache.
    pub leaderboards: Arc<Leaderboards>,
    pub lnurl: Arc<dyn LnurlPay>,
    pub background_threads: Arc<HashMap<String, JoinHandle<()>>>,
    pub forgot_password_challenges: Arc<RwLock<HashMap<String, (String, std::time::Instant)>>>,
}

pub async fn build_app(
    config: Settings,
) -> Result<(AppState, TaskTracker, CancellationToken, Vec<DBConnection>), anyhow::Error> {
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
        let client = BitcoinClient::new(&config.bitcoin_settings, &config.ln_settings)
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
        let client = BitcoinClient::new(&config.bitcoin_settings, &config.ln_settings)
            .await
            .map(Arc::new)?;
        info!("Bitcoin service configured");
        client
    };

    let http_client = Client::new();
    let reqwest_client = build_reqwest_client(http_client.clone());

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
            build_oracle_reqwest_client(http_client.clone()),
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
            build_oracle_reqwest_client(http_client.clone()),
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

    let competition_db_clone = competition_db.clone();
    let competition_store = CompetitionStore::new(competition_db);

    let users_db = DBConnection::new(
        &config.db_settings.data_folder,
        "users",
        pool_config.clone(),
        DatabaseType::Users,
    )
    .await
    .map_err(|e| anyhow!("Error setting up users db: {}", e))?;

    let users_db_clone = users_db.clone();
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
        competition_db_clone.clone(),
    )
    .map_err(|e| anyhow!("Failed to create keymeld service: {}", e))?;

    if config.keymeld_settings.enabled {
        info!("Keymeld service configured (enabled)");
        if config.keymeld_settings.dangerous_trust_unattested_enclaves {
            log::warn!(
                "Keymeld attestation is disabled: trusting unattested simulated enclaves. Never use this on mainnet."
            );
        }
    } else {
        info!("Keymeld service configured (disabled - using local MuSig2)");
    }

    let keymeld_gateway_url = if config.keymeld_settings.enabled {
        Some(config.keymeld_settings.gateway_url.clone())
    } else {
        None
    };

    // Lightning Address resolution follows the Lightning client: mocked
    // together, real together.
    let lnurl = lnurl_resolver(
        config.ln_settings.mock_enabled,
        config.bitcoin_settings.network,
    );

    let lease_holder = config.coordinator_settings.lease_holder();
    let pacing = config.coordinator_settings.pacing();
    let coordinator = Coordinator::new(
        oracle_client,
        competition_store,
        bitcoin_client.clone(),
        ln.clone(),
        lnurl.clone(),
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
    .await?
    .with_automatic_payouts(
        config.keymeld_settings.automatic_payouts,
        config.keymeld_settings.automatic_payout_max_fee_rate_sat_vb,
    )?
    .with_ark(arkade(&config.ark_settings).await?)?;
    let (wakes, wake_requests) = CompetitionWakes::new();
    let coordinator = Arc::new(
        coordinator
            .with_wakes(wakes.clone())
            .with_lease_holder(lease_holder.clone(), pacing.lease_ttl),
    );

    if config.coordinator_settings.escrow_enabled {
        info!("Escrow transactions enabled");
    } else {
        info!("Escrow transactions disabled (using HODL invoices only)");
    }

    info!("Coordinator service configured");

    let tracker = TaskTracker::new();
    let mut threads = HashMap::new();
    let cancel_token = CancellationToken::new();
    let runners = CompetitionRunners::new(
        coordinator.clone(),
        coordinator.competition_store.clone(),
        wakes,
        lease_holder,
        pacing,
        tracker.clone(),
        cancel_token.clone(),
    );
    let competition_watcher_task = spawn_supervised(
        &tracker,
        "competition runners",
        cancel_token.clone(),
        runners.supervise(wake_requests),
    );

    let bitcoin_watcher = BitcoinSyncWatcher::new(
        bitcoin_client.clone(),
        cancel_token.clone(),
        Duration::from_secs(config.bitcoin_settings.refresh_blocks_secs),
    );

    let bitcoin_watcher_task = spawn_supervised(
        &tracker,
        "Bitcoin sync watcher",
        cancel_token.clone(),
        async move { bitcoin_watcher.watch().await },
    );

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

    let invoice_watcher_handle = spawn_supervised(
        &tracker,
        "invoice watcher",
        cancel_token.clone(),
        async move { invoice_watcher.watch().await },
    );

    threads.insert("invoice_watcher".to_string(), invoice_watcher_handle);

    let payout_watcher = PayoutWatcher::new(
        coordinator.clone(),
        ln.clone(),
        cancel_token.clone(),
        Duration::from_secs(config.ln_settings.payout_watch_interval),
    );

    let payout_watcher_handle = spawn_supervised(
        &tracker,
        "payout watcher",
        cancel_token.clone(),
        async move { payout_watcher.watch().await },
    );

    threads.insert("payout_watcher".to_string(), payout_watcher_handle);

    let automatic_coordinator = coordinator.clone();
    let automatic_cancel = cancel_token.clone();
    let automatic_handle = spawn_supervised(
        &tracker,
        "automatic payouts",
        cancel_token.clone(),
        async move {
            loop {
                tokio::select! {
                    _ = automatic_cancel.cancelled() => break,
                    result = automatic_coordinator
                        .worker_leases()
                        .tick("automatic-payouts", automatic_coordinator.automatic_payout_tick()) => {
                        if let Some(Err(error)) = result { error!("Automatic payout worker: {}", error); }
                    }
                }
                tokio::select! {
                    _ = automatic_cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
            automatic_coordinator
                .worker_leases()
                .release("automatic-payouts")
                .await;
            Ok(())
        },
    );
    threads.insert("automatic_payouts".to_string(), automatic_handle);

    if coordinator.ark().is_some() {
        let ark_coordinator = coordinator.clone();
        let ark_cancel = cancel_token.clone();
        let ark_handle = spawn_supervised(
            &tracker,
            "escrow swaps",
            cancel_token.clone(),
            async move {
                loop {
                    tokio::select! {
                        _ = ark_cancel.cancelled() => break,
                        result = ark_coordinator
                            .worker_leases()
                            .tick("escrow-swaps", ark_coordinator.check_ark_swaps()) => {
                            if let Some(Err(error)) = result { error!("Escrow swap worker: {}", error); }
                        }
                    }
                    tokio::select! {
                        _ = ark_cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
                ark_coordinator
                    .worker_leases()
                    .release("escrow-swaps")
                    .await;
                Ok(())
            },
        );
        threads.insert("escrow_swaps".to_string(), ark_handle);
    }

    // Subscription-based watchers for faster payment detection
    // These run alongside the polling watchers as the primary mechanism,
    // with polling serving as a fallback
    let invoice_subscriber =
        InvoiceSubscriber::new(coordinator.clone(), ln.clone(), cancel_token.clone());

    let invoice_subscriber_handle = spawn_supervised(
        &tracker,
        "invoice subscriber",
        cancel_token.clone(),
        async move { invoice_subscriber.subscribe().await },
    );

    threads.insert("invoice_subscriber".to_string(), invoice_subscriber_handle);

    let payment_subscriber =
        PaymentSubscriber::new(coordinator.clone(), ln.clone(), cancel_token.clone());

    let payment_subscriber_handle = spawn_supervised(
        &tracker,
        "payment subscriber",
        cancel_token.clone(),
        async move { payment_subscriber.subscribe().await },
    );

    threads.insert("payment_subscriber".to_string(), payment_subscriber_handle);

    // Pages read the oracle's weather from a cache this keeps fresh, never from the oracle.
    let users_info = Arc::new(UserInfo::new(users_store));
    let leaderboards = Arc::new(Leaderboards::new(
        coordinator.clone(),
        users_info.clone(),
        &config.coordinator_settings.oracle_url,
    )?);
    leaderboards.spawn_refresher(&tracker, cancel_token.clone());
    tracker.close();

    let app_state = AppState {
        ui_dir: config.ui_settings.ui_dir,
        private_url: config.ui_settings.private_url,
        remote_url: config.ui_settings.remote_url,
        explorer_url: config
            .bitcoin_settings
            .explorer_url
            .clone()
            .unwrap_or_default(),
        oracle_url: config.coordinator_settings.oracle_url,
        network: config.bitcoin_settings.network.to_string(),
        coordinator,
        users_info,
        leaderboards,
        lnurl,
        bitcoin: bitcoin_client,
        background_threads: Arc::new(threads),
        forgot_password_challenges: Arc::new(RwLock::new(HashMap::new())),
    };
    Ok((
        app_state,
        tracker,
        cancel_token,
        vec![competition_db_clone, users_db_clone],
    ))
}

pub async fn build_server(
    socket_addr: SocketAddr,
    router: Router,
) -> Result<HttpServer, anyhow::Error> {
    let listener = TcpListener::bind(socket_addr).await?;
    let local_addr = listener.local_addr()?;
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    );
    info!("Service running @: http://{local_addr}");
    Ok(server)
}

/// Public listener: participant API, public pages, and static assets. It must never
/// route an operator path; `startup_tests` proves this for every admin route.
pub fn app(app_state: Arc<AppState>, api: &APISettings) -> Router {
    let origins: Vec<HeaderValue> = api
        .origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();

    // NIP-98 events must name the URL clients actually use: the browser
    // origins plus the UI's own public and private URLs. Validated at startup.
    let nip98_origins = Nip98Origins::new(api.origins.iter().map(String::as_str).chain([
        app_state.remote_url.as_str(),
        app_state.private_url.as_str(),
    ]))
    .expect("api_settings.origins and ui_settings urls are validated at startup");

    // Release expired replay entries during idle periods as well as admission.
    let replay = Arc::new(Nip98ReplayGuard::new(api.replay_capacity));
    {
        let guard = Arc::downgrade(&replay);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let Some(guard) = guard.upgrade() else { break };
                guard.prune(time::OffsetDateTime::now_utc().unix_timestamp());
            }
        });
    }

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([ACCEPT, CONTENT_TYPE, AUTHORIZATION])
        .allow_origin(AllowOrigin::list(origins))
        .allow_credentials(true);

    let users_endpoints = Router::new()
        .route("/login", post(login))
        .route("/register", post(register))
        .route("/username/register", post(register_username))
        .route("/lightning-address", post(set_lightning_address))
        .route("/username/login", post(login_username))
        .route("/username/change-password", post(change_password))
        .route("/username/forgot-password", post(forgot_password_challenge))
        .route("/username/reset-password", post(forgot_password_reset));
    let users_endpoints = limited(
        users_endpoints,
        &api.rate_limit,
        api.rate_limit.auth_per_second,
        api.rate_limit.auth_burst,
    );

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

    let api_routes = Router::new()
        .route("/", get(public_page_handler))
        .merge(htmx_routes)
        .fallback(public_fallback)
        .route("/api/v1/health_check", get(health))
        .route("/api/v1/competitions", get(get_competitions))
        .route(
            "/api/v1/competitions/{competition_id}",
            get(get_competition),
        )
        .route(
            "/api/v1/competitions/{competition_id}/payout-terms",
            get(crate::api::routes::get_payout_terms),
        )
        .route(
            "/api/v1/competitions/{competition_id}/ticket",
            post(request_competition_ticket),
        )
        .route(
            "/api/v1/competitions/{competition_id}/tickets/{ticket_id}/status",
            get(get_ticket_status),
        )
        .route(
            "/api/v1/competitions/{competition_id}/tickets/{ticket_id}/refund",
            get(get_ticket_refund),
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
        .route(
            "/api/v1/competitions/{competitionId}/entries/{entryId}/claim",
            post(claim_ticket_payout),
        )
        .route(
            "/api/v1/competitions/{competitionId}/entries/{entryId}/payout-authorization",
            get(crate::api::routes::get_payout_authorization)
                .post(crate::api::routes::submit_invoice_fallback),
        )
        .route("/api/v1/entries", post(add_event_entry))
        .route("/api/v1/entries", get(get_entries))
        .nest("/api/v1/users", users_endpoints);
    let api_routes = limited(
        api_routes,
        &api.rate_limit,
        api.rate_limit.per_second,
        api.rate_limit.burst,
    );

    Router::new()
        .merge(api_routes)
        .route("/ui/{*path}", get(serve_static_file))
        .layer(Extension(replay))
        .layer(Extension(Arc::new(nip98_origins)))
        .layer(middleware::from_fn(log_request))
        .with_state(app_state)
        .layer(cors)
}

/// Per-client limit on every route of `router`, unless limiting is off.
/// `per_second` is the sustained rate, `burst` the allowance above it.
fn limited<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    settings: &RateLimitSettings,
    per_second: u32,
    burst: u32,
) -> Router<S> {
    if !settings.enabled {
        return router;
    }
    let config = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_nanos(
                1_000_000_000_u64.div_ceil(u64::from(per_second.max(1))),
            ))
            .burst_size(burst.max(1))
            .key_extractor(PeerIpKeyExtractor)
            .finish()
            .expect("rate limit settings are valid"),
    );
    // The limiter only forgets idle clients when told to.
    let limiter = Arc::downgrade(config.limiter());
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let Some(limiter) = limiter.upgrade() else {
                break;
            };
            limiter.retain_recent();
        }
    });
    router.route_layer(GovernorLayer::new(config))
}

/// Admin listener: operator pages, the LND wallet API, and competition creation.
///
/// Everything except the sign-in form and static assets sits behind `require_operator`.
/// No CORS layer: operator pages call only their own origin. The test-settle route,
/// which marks tickets paid without a payment, is never registered on mainnet.
pub fn admin_app(app_state: Arc<AppState>, access: Arc<AdminAccess>, network: Network) -> Router {
    let mut admin_htmx_routes = Router::new()
        .route("/", get(admin_page_handler))
        .route("/competition", get(admin_competition_fragment))
        .route("/wallet", get(admin_wallet_fragment))
        .route("/wallet/balance", get(admin_wallet_balance_fragment))
        .route("/wallet/address", get(admin_wallet_address_fragment))
        .route("/wallet/fees", get(admin_fee_estimates_fragment))
        .route("/wallet/outputs", get(admin_wallet_outputs_fragment))
        .route("/wallet/send", post(admin_send_bitcoin_handler))
        .route("/api/competitions", post(admin_create_competition_handler))
        .route(
            "/api/competitions/delete",
            post(admin_delete_competition_handler),
        );
    if network != Network::Bitcoin {
        admin_htmx_routes = admin_htmx_routes.route(
            "/api/test/settle-invoice/{ticket_id}",
            post(admin_settle_test_invoice_handler),
        );
    }

    let wallet_endpoints = Router::new()
        .route("/balance", get(get_balance))
        .route("/address", get(get_next_address))
        .route("/outputs", get(get_outputs))
        .route("/send", post(send_to_address))
        .route("/estimated_fees", get(get_estimated_fee_rates));

    let operator_routes = Router::new()
        .nest("/admin", admin_htmx_routes)
        .nest("/api/v1/wallet", wallet_endpoints)
        .route("/api/v1/competitions", post(create_competition))
        .route_layer(middleware::from_fn_with_state(
            access.clone(),
            require_operator,
        ))
        .with_state(app_state.clone());

    let sign_in = Router::new()
        .route("/admin/login", get(admin_login_page).post(admin_login))
        .with_state(access);

    Router::new()
        .merge(operator_routes)
        .merge(sign_in)
        .route("/ui/{*path}", get(serve_static_file))
        .with_state(app_state)
        .layer(middleware::from_fn(operator_response_headers))
        .layer(middleware::from_fn(log_request))
}

/// Serve the competitions page for client-side paths, but never for paths reserved for
/// the API or the operator listener, so a missing operator route cannot look present.
async fn public_fallback(State(state): State<Arc<AppState>>, uri: Uri) -> Response {
    let path = uri.path();
    let reserved = ["/admin", "/api"]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")));
    if reserved {
        return StatusCode::NOT_FOUND.into_response();
    }
    public_page_handler(State(state)).await.into_response()
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
    static_file_response(&state.ui_dir, &path).await
}

async fn static_file_response(ui_dir: &str, path: &str) -> Response {
    // Axum percent-decodes the wildcard before extraction. An encoded leading
    // slash would make Path::join discard ui_dir, even without any '..'.
    if path.is_empty()
        || !std::path::Path::new(path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return (StatusCode::BAD_REQUEST, "Bad request").into_response();
    }

    let file_path = std::path::Path::new(ui_dir).join(path);

    let content = match tokio::fs::read(&file_path).await {
        Ok(c) => c,
        Err(_) => return (StatusCode::NOT_FOUND, "Not found").into_response(),
    };

    let mime_type = get_mime_type(path);

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

#[cfg(any(feature = "e2e-testing", debug_assertions))]
fn lnurl_resolver(mock: bool, network: Network) -> Arc<dyn LnurlPay> {
    if mock {
        Arc::new(MockLnurlPay::new(network))
    } else {
        Arc::new(HttpsLnurlPay::new(network))
    }
}

/// Release builds refuse mocked Lightning before this is reached.
#[cfg(not(any(feature = "e2e-testing", debug_assertions)))]
fn lnurl_resolver(_mock: bool, network: Network) -> Arc<dyn LnurlPay> {
    Arc::new(HttpsLnurlPay::new(network))
}

/// Run a background worker. A worker that stops for any reason other than
/// shutdown takes the whole service down with it: the watchers settle
/// invoices, payouts and contracts, so running without one is worse than
/// restarting.
fn spawn_supervised(
    tracker: &TaskTracker,
    name: &'static str,
    cancel: CancellationToken,
    work: impl std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'static,
) -> JoinHandle<()> {
    // Also propagate unexpected task abortion, including before its first poll.
    let cancel_on_drop = cancel.clone().drop_guard();
    tracker.spawn(async move {
        let _cancel_on_drop = cancel_on_drop;
        match std::panic::AssertUnwindSafe(work).catch_unwind().await {
            Ok(Ok(())) if cancel.is_cancelled() => info!("{name} stopped"),
            Ok(Ok(())) => {
                error!("{name} stopped unexpectedly; shutting down");
                cancel.cancel();
            }
            Ok(Err(e)) => {
                error!("{name} failed: {e}; shutting down");
                cancel.cancel();
            }
            Err(_) => {
                error!("{name} panicked; shutting down");
                cancel.cancel();
            }
        }
    })
}

pub fn build_reqwest_client(client: Client) -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .with(LoggingMiddleware)
        .build()
}

/// Build a reqwest client for the oracle with more forgiving retry policy.
/// The oracle may be temporarily unavailable during blue/green deployments,
/// so we retry for up to 10 minutes with exponential backoff (5s to 60s).
pub fn build_oracle_reqwest_client(client: Client) -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder()
        .retry_bounds(Duration::from_secs(5), Duration::from_secs(60))
        .build_with_total_retry_duration(Duration::from_secs(10 * 60));
    ClientBuilder::new(client)
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

#[cfg(test)]
mod startup_tests {
    use super::*;
    use crate::api::admin_auth::{AdminCredentials, CSRF_HEADER, SESSION_COOKIE};
    use axum::{body::to_bytes, http::HeaderMap};
    use std::io::Write;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const SETTLE_PATH: &str = "/admin/api/test/settle-invoice/0190b7a4-0000-7000-8000-000000000000";

    /// Every operator route, including the sign-in form, as (method, path).
    const OPERATOR_ROUTES: &[(&str, &str)] = &[
        ("GET", "/admin"),
        ("GET", "/admin/competition"),
        ("GET", "/admin/wallet"),
        ("GET", "/admin/wallet/balance"),
        ("GET", "/admin/wallet/address"),
        ("GET", "/admin/wallet/fees"),
        ("GET", "/admin/wallet/outputs"),
        ("POST", "/admin/wallet/send"),
        ("POST", "/admin/api/competitions"),
        ("POST", "/admin/api/competitions/delete"),
        ("POST", SETTLE_PATH),
        ("GET", "/admin/login"),
        ("POST", "/admin/login"),
        ("GET", "/api/v1/wallet/balance"),
        ("GET", "/api/v1/wallet/address"),
        ("GET", "/api/v1/wallet/outputs"),
        ("GET", "/api/v1/wallet/estimated_fees"),
        ("POST", "/api/v1/wallet/send"),
        ("POST", "/api/v1/competitions"),
    ];

    fn protected_routes() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
        OPERATOR_ROUTES
            .iter()
            .filter(|(_, path)| *path != "/admin/login")
    }

    /// Real application state over mock Bitcoin, LND, and oracle clients.
    struct TestState {
        state: Arc<AppState>,
        tasks: TaskTracker,
        cancel: CancellationToken,
        databases: Vec<DBConnection>,
        _data: tempfile::TempDir,
    }

    impl TestState {
        async fn start() -> Self {
            let data = tempfile::tempdir().unwrap();
            let mut settings = Settings::default();
            settings.db_settings.data_folder = data.path().display().to_string();
            settings.bitcoin_settings.mock_enabled = true;
            settings.ln_settings.mock_enabled = true;
            settings.coordinator_settings.mock_oracle = true;
            settings.coordinator_settings.oracle_url = String::from("mock://oracle");
            let (state, tasks, cancel, databases) = build_app(settings).await.unwrap();
            Self {
                state: Arc::new(state),
                tasks,
                cancel,
                databases,
                _data: data,
            }
        }

        fn admin(&self, access: AdminAccess, network: Network) -> Router {
            admin_app(self.state.clone(), Arc::new(access), network)
        }

        fn public(&self) -> Router {
            app(
                self.state.clone(),
                &APISettings {
                    rate_limit: RateLimitSettings::disabled(),
                    ..APISettings::default()
                },
            )
        }

        async fn stop(self) {
            self.cancel.cancel();
            for handle in self.state.background_threads.values() {
                handle.abort();
            }
            self.tasks.wait().await;
            for database in self.databases {
                database.close().await.unwrap();
            }
        }
    }

    fn token_access() -> AdminAccess {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "{TOKEN}").unwrap();
        AdminAccess::Token(AdminCredentials::load(file.path()).unwrap())
    }

    fn request(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::from(body.to_owned())).unwrap()
    }

    async fn send(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    const BEARER: &str = "Bearer 0123456789abcdef0123456789abcdef";
    const FORM: &str = "application/x-www-form-urlencoded";

    #[tokio::test]
    async fn public_router_serves_no_operator_route() {
        let test = TestState::start().await;
        let public = test.public();
        for (method, path) in OPERATOR_ROUTES {
            for headers in [&[][..], &[("authorization", BEARER)][..]] {
                let (status, _, _) = send(&public, request(method, path, headers, "")).await;
                assert!(
                    status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
                    "public {method} {path} returned {status}"
                );
            }
        }
        let (status, _, _) = send(&public, request("GET", "/api/v1/health_check", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&public, request("GET", "/api/v1/competitions", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        test.stop().await;
    }

    #[tokio::test]
    async fn admin_router_requires_a_valid_token_on_every_operator_route() {
        let test = TestState::start().await;
        let admin = test.admin(token_access(), Network::Regtest);
        let wrong = [
            vec![],
            vec![("authorization", "Bearer 0123456789abcdef0123456789abcdeX")],
            vec![("authorization", "Bearer ")],
            vec![("authorization", TOKEN)],
            vec![("cookie", "coordinator_admin_session=4102444800.00")],
        ];
        for (method, path) in protected_routes() {
            for headers in &wrong {
                let (status, response_headers, _) =
                    send(&admin, request(method, path, headers, "")).await;
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} with {headers:?}"
                );
                assert_eq!(response_headers["www-authenticate"], "Bearer");
                assert_eq!(response_headers["x-frame-options"], "DENY");
            }
            // The handler runs: any status but an authentication failure or missing route.
            let (status, _, body) = send(
                &admin,
                request(method, path, &[("authorization", BEARER)], ""),
            )
            .await;
            assert!(
                ![
                    StatusCode::UNAUTHORIZED,
                    StatusCode::FORBIDDEN,
                    StatusCode::METHOD_NOT_ALLOWED
                ]
                .contains(&status)
                    && (status != StatusCode::NOT_FOUND || path == &SETTLE_PATH),
                "{method} {path} with bearer returned {status}: {body}"
            );
        }
        let (status, _, body) = send(
            &admin,
            request(
                "GET",
                "/api/v1/wallet/balance",
                &[("authorization", BEARER)],
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("confirmed"));
        test.stop().await;
    }

    #[tokio::test]
    async fn browser_session_needs_the_csrf_token_for_state_changes() {
        let test = TestState::start().await;
        let admin = test.admin(token_access(), Network::Regtest);

        let (status, _, _) = send(
            &admin,
            request(
                "POST",
                "/admin/login",
                &[("content-type", FORM)],
                "token=wrong",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, headers, _) = send(
            &admin,
            request(
                "POST",
                "/admin/login",
                &[("content-type", FORM)],
                &format!("token={TOKEN}"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers["location"], "/admin");
        let set_cookie = headers["set-cookie"].to_str().unwrap();
        for flag in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/admin"] {
            assert!(set_cookie.contains(flag), "{set_cookie} lacks {flag}");
        }
        let cookie = set_cookie.split(';').next().unwrap().to_owned();
        assert!(cookie.starts_with(SESSION_COOKIE));

        let (status, _, page) = send(
            &admin,
            request("GET", "/admin/wallet", &[("cookie", &cookie)], ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let marker = format!("{CSRF_HEADER}&quot;:&quot;");
        let csrf = page
            .split_once(&marker)
            .and_then(|(_, rest)| rest.split_once("&quot;"))
            .map(|(token, _)| token.to_owned())
            .expect("admin page renders the CSRF token for HTMX");

        let delete = "competition_id=not-a-uuid";
        let without_csrf = [("cookie", cookie.as_str()), ("content-type", FORM)];
        let (status, _, _) = send(
            &admin,
            request(
                "POST",
                "/admin/api/competitions/delete",
                &without_csrf,
                delete,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let with_csrf = [
            ("cookie", cookie.as_str()),
            ("content-type", FORM),
            (CSRF_HEADER, csrf.as_str()),
        ];
        let (status, _, body) = send(
            &admin,
            request("POST", "/admin/api/competitions/delete", &with_csrf, delete),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Invalid competition ID"), "{body}");
        test.stop().await;
    }

    #[tokio::test]
    async fn test_settlement_route_does_not_exist_on_mainnet() {
        let test = TestState::start().await;
        let bearer = [("authorization", BEARER)];
        let regtest = test.admin(token_access(), Network::Regtest);
        let (_, _, body) = send(&regtest, request("POST", SETTLE_PATH, &bearer, "")).await;
        assert!(body.contains("Ticket not found"), "{body}");

        let mainnet = test.admin(token_access(), Network::Bitcoin);
        let (status, _, body) = send(&mainnet, request("POST", SETTLE_PATH, &bearer, "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.is_empty(), "{body}");
        test.stop().await;
    }

    #[tokio::test]
    async fn unauthenticated_development_access_admits_operator_routes() {
        let test = TestState::start().await;
        let admin = test.admin(AdminAccess::Unauthenticated, Network::Regtest);
        let (status, _, _) = send(&admin, request("GET", "/api/v1/wallet/balance", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&admin, request("GET", "/admin/wallet/fees", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        test.stop().await;
    }
}

#[cfg(test)]
mod static_file_tests {
    use super::*;

    #[tokio::test]
    async fn static_routes_reject_percent_encoded_absolute_paths_and_parent_components() {
        let directory = tempfile::tempdir().unwrap();
        let ui_dir = directory.path().join("ui");
        std::fs::create_dir_all(ui_dir.join("pkg")).unwrap();
        std::fs::write(ui_dir.join("pkg/app.js"), "browser code").unwrap();
        let secret_path = directory.path().join("secret.txt");
        std::fs::write(&secret_path, "private data").unwrap();

        let router = Router::new()
            .route(
                "/ui/{*path}",
                get(
                    |State(ui_dir): State<String>, Path(path): Path<String>| async move {
                        static_file_response(&ui_dir, &path).await
                    },
                ),
            )
            .with_state(ui_dir.to_str().unwrap().to_owned());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();

        let valid = client
            .get(format!("http://{address}/ui/pkg/app.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(valid.status(), StatusCode::OK);
        assert_eq!(valid.text().await.unwrap(), "browser code");

        for path in [
            format!(
                "%2F{}",
                secret_path.to_str().unwrap().trim_start_matches('/')
            ),
            "pkg%2F..%2F..%2Fsecret.txt".to_owned(),
        ] {
            let response = client
                .get(format!("http://{address}/ui/{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert!(!response.text().await.unwrap().contains("private data"));
        }
        server.abort();
    }
}

#[cfg(test)]
#[path = "startup_hardening_tests.rs"]
mod startup_hardening_tests;

/// The Arkade server and swap service, when Arkade funding is enabled.
async fn arkade(
    settings: &crate::config::ArkSettings,
) -> Result<Option<crate::domain::Arkade>, anyhow::Error> {
    if !settings.enabled {
        return Ok(None);
    }
    let server = coordinator_ark::ArkServer::connect(settings.server_url.clone()).await?;
    let swaps =
        crate::infra::ark_swap::SwapClient::new(&settings.swap_url, settings.swap_token()?)?;
    info!(
        "Arkade funding enabled: server {} (signer {}), swaps at {}",
        settings.server_url,
        server.rules().signer,
        settings.swap_url
    );
    Ok(Some(crate::domain::Arkade {
        transport: Arc::new(server.client().clone()),
        server,
        swaps: Arc::new(swaps),
        refund_after_start_secs: settings.refund_after_start_secs,
        max_refund_fee_sats: settings.max_refund_fee_sats,
    }))
}
