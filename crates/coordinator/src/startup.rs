use crate::{
    api::admin_auth::{
        admin_login, admin_login_page, operator_response_headers, require_operator, AdminAccess,
    },
    api::nip98_replay::Nip98ReplayGuard,
    api::routes::{
        add_event_entry, admin_approve_owed_winner_sweep_handler, admin_competition_fragment,
        admin_competition_map, admin_create_competition_handler, admin_delete_competition_handler,
        admin_fee_estimates_fragment, admin_page_handler, admin_send_bitcoin_handler,
        admin_settle_owed_winner_handler, admin_settle_test_invoice_handler,
        admin_wallet_address_fragment, admin_wallet_balance_fragment, admin_wallet_fragment,
        admin_wallet_outputs_fragment, change_password, claim_ticket_payout, competitions_fragment,
        create_competition, create_queued_competition, entries_fragment, entry_detail_fragment,
        entry_forecasts_fragment, entry_form_fragment, entry_paid_fragment, entry_payout_fragment,
        entry_unpaid_fragment, forgot_password_challenge, forgot_password_reset,
        get_aggregate_nonces, get_balance, get_competition, get_competitions,
        get_contract_parameters, get_entries, get_estimated_fee_rates, get_next_address,
        get_outputs, get_paid_tickets, get_ticket_refund, get_ticket_status, get_unpaid_tickets,
        health, leaderboard_fragment, leaderboard_rows_fragment, login, login_username, not_found,
        operator_approve_owed_winner_sweep, operator_competition, operator_competitions,
        operator_delete_competition, operator_owed_winners, operator_payout_holds,
        operator_release_payout_hold, operator_settle_owed_winner, operator_write_off_refunds,
        payouts_fragment, public_page_handler, register, register_ticket, register_username,
        request_competition_ticket, send_to_address, set_lightning_address, signup_pow_challenge,
        submit_final_signatures, submit_public_nonces, submit_ticket_payout,
        ticket_status_fragment,
    },
    config::Settings,
    domain::{
        leaderboard::Leaderboards,
        recovery::{Recovery, RecoveryPublisher},
        CompetitionRunners, CompetitionStore, CompetitionWakes, Coordinator, InvoiceSubscriber,
        InvoiceWatcher, PaymentSubscriber, PayoutWatcher, SignupPow, SubscriptionHealth, UserInfo,
        UserStore, ARK_SWAP_BOARDS_EVERY, TICKET_PREIMAGE_BATCH,
    },
    infra::{
        bitcoin::{Bitcoin, BitcoinClient, BitcoinSyncWatcher, ElectrumHeaders},
        db::{DBConnection, DatabasePoolConfig, DatabaseType},
        file_utils::create_folder,
        keymeld::create_keymeld_service,
        lightning::{Ln, LnClient},
        lnurl::{HttpsLnurlPay, LnurlPay},
        oracle::{Oracle, OracleClient},
    },
    metrics::{metrics_app, Metrics, LN_INVOICE_SUBSCRIPTION_UP, LN_PAYMENT_SUBSCRIPTION_UP},
};

// Mock implementations only available with e2e-testing feature or debug builds
use crate::api::nip98_origins::Nip98Origins;
use crate::api::public_headers::{public_response_headers, PublicHeaders};
use crate::api::request_context::{
    request_context_middleware, HttpContext, ParentRequestIdMiddleware,
};
use crate::config::{APISettings, RateLimitSettings};
#[cfg(any(feature = "e2e-testing", debug_assertions))]
use crate::infra::{
    bitcoin_mock::MockBitcoinClient, lightning_mock::MockLnClient, lnurl_mock::MockLnurlPay,
    oracle_mock::MockOracle,
};
use anyhow::anyhow;
#[cfg(test)]
use axum::{body::Body, extract::Request};
use axum::{
    extract::{connect_info::IntoMakeServiceWithConnectInfo, ConnectInfo, DefaultBodyLimit, State},
    http::{header, Extensions, HeaderValue, StatusCode, Uri},
    middleware::{self, AddExtension},
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
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowOrigin, CorsLayer},
};
type HttpServer = Serve<
    TcpListener,
    IntoMakeServiceWithConnectInfo<Router, SocketAddr>,
    AddExtension<Router, ConnectInfo<SocketAddr>>,
>;

/// Owns the HTTP listeners, the background producers, and the databases.
///
/// The public listener serves participants. The admin listener serves operator routes
/// behind `api::admin_auth`; it never shares a socket with the public router. The
/// optional metrics listener serves only `GET /metrics`.
pub struct Application {
    server: HttpServer,
    admin_server: HttpServer,
    metrics_server: Option<HttpServer>,
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
        let metrics_listener = config.metrics_settings.listen_addr;
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
                build_server(listener, app(app_state.clone(), &config.api_settings)?).await?;
            let admin_server = build_server(
                admin_listener,
                admin_app(app_state.clone(), admin_access, network),
            )
            .await?;
            let metrics_server = match metrics_listener {
                Some(address) => {
                    let metrics = Metrics::new(
                        app_state.coordinator.competition_store.clone(),
                        app_state.background_threads.clone(),
                    )?
                    .with_arkade_health(app_state.coordinator.arkade_health())
                    .with_settled_outputs(app_state.coordinator.clone());
                    Some(build_server(address, metrics_app(Arc::new(metrics))).await?)
                }
                None => None,
            };
            Ok::<_, anyhow::Error>((server, admin_server, metrics_server))
        };
        let (server, admin_server, metrics_server) = match servers.await {
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
            metrics_server,
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
            metrics_server,
            cancellation_token,
            background_tasks,
            background_abort_handles,
            db_connections,
        } = self;
        let stop_http = CancellationToken::new();
        let mut http = spawn_http(server, stop_http.clone());
        let mut admin_http = spawn_http(admin_server, stop_http.clone());
        let mut metrics_http = metrics_server.map(|server| spawn_http(server, stop_http.clone()));
        let mut http_finished = false;
        let mut admin_http_finished = false;
        let mut metrics_http_finished = false;
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
            Some(result) = async {
                match metrics_http.as_mut() {
                    Some(task) => Some(task.await),
                    None => None,
                }
            } => {
                metrics_http_finished = true;
                shutdown_error = Some(anyhow!("Metrics HTTP server stopped unexpectedly: {result:?}"));
            }
            result = shutdown_signal() => {
                if let Err(error) = result { shutdown_error = Some(error.into()); }
            }
            () = writer_stopped => {
                shutdown_error = Some(anyhow!("Database writer stopped unexpectedly"));
            }
            () = cancellation_token.cancelled() => {
                shutdown_error = Some(anyhow!("A background worker stopped unexpectedly"));
            }
        }
        stop_http.cancel();
        let (public_drain, admin_drain, metrics_drain) = tokio::join!(
            drain_http("HTTP server", http, http_finished),
            drain_http("Admin HTTP server", admin_http, admin_http_finished),
            async {
                match metrics_http {
                    Some(task) => {
                        drain_http("Metrics HTTP server", task, metrics_http_finished).await
                    }
                    None => None,
                }
            },
        );
        for drain_error in [public_drain, admin_drain, metrics_drain]
            .into_iter()
            .flatten()
        {
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
    pub admin_monitoring: Arc<crate::infra::admin_monitoring::AdminMonitoring>,
    pub admin_weather: Arc<crate::infra::admin_weather::WeatherDiscovery>,
    pub ui_dir: String,
    /// Hash of the WASM package in `ui_dir`; pages request it by this version.
    pub wasm_version: String,
    pub private_url: String,
    pub remote_url: String,
    pub oracle_url: String,
    /// Gateway the browser calls to verify its assigned signing enclave.
    pub keymeld_public_url: Option<String>,
    pub explorer_url: String,
    /// Arkade explorer for VTXOs and Arkade transactions; empty when none is configured.
    pub ark_explorer_url: String,
    /// Origin of the Satchel test-network wallet pages offer, when one is configured.
    pub satchel_url: Option<String>,
    pub network: String,
    pub bitcoin: Arc<dyn Bitcoin>,
    pub coordinator: Arc<Coordinator>,
    /// One serialized operator inventory, refreshed without decoding contract blobs.
    pub operator_inventory: Arc<crate::infra::refresh_cache::RefreshCache<(), axum::body::Bytes>>,
    pub users_info: Arc<UserInfo>,
    /// Leaderboards and the oracle weather pages show, served from a background cache.
    pub leaderboards: Arc<Leaderboards>,
    pub lnurl: Arc<dyn LnurlPay>,
    pub background_threads: Arc<HashMap<String, JoinHandle<()>>>,
    /// Consumed reset challenges mapped to their owner and consumption time.
    pub forgot_password_challenges: Arc<RwLock<HashMap<String, (String, std::time::Instant)>>>,
    /// Recovery records and the recovery file, when enabled.
    pub recovery: Option<Arc<Recovery>>,
    /// Issues and checks the proofs of work new accounts carry.
    pub signup_pow: Arc<SignupPow>,
    /// Which proxies may vouch for client addresses and request ids.
    pub http_context: Arc<HttpContext>,
    /// Per-session and global caps on browser telemetry events.
    pub telemetry_caps: Arc<crate::api::telemetry::TelemetryCaps>,
    /// The feedback form's messages, proof of work and limits.
    pub feedback: Arc<crate::domain::feedback::Feedback>,
    pub mainnet_signup: Arc<crate::domain::mainnet_signup::MainnetSignup>,
    /// Visitor logs for the operator's Visitors page.
    pub visitor_logs: Arc<crate::infra::visitor_logs::VisitorLogs>,
}

async fn create_bitcoin_client(config: &Settings) -> Result<Arc<dyn Bitcoin>, anyhow::Error> {
    #[cfg(any(feature = "e2e-testing", debug_assertions))]
    if config.bitcoin_settings.mock_enabled {
        info!("Mock Bitcoin client configured");
        return Ok(Arc::new(MockBitcoinClient::new(
            config.bitcoin_settings.network,
        )));
    }
    #[cfg(not(any(feature = "e2e-testing", debug_assertions)))]
    if config.bitcoin_settings.mock_enabled {
        return Err(anyhow!(
            "Mock Bitcoin client requires e2e-testing feature or debug build"
        ));
    }
    let client = BitcoinClient::new(&config.bitcoin_settings, &config.ln_settings).await?;
    info!("Bitcoin service configured");
    Ok(Arc::new(client))
}

async fn create_lightning_client(
    config: &Settings,
    client: ClientWithMiddleware,
) -> Result<Arc<dyn Ln>, anyhow::Error> {
    #[cfg(any(feature = "e2e-testing", debug_assertions))]
    if config.ln_settings.mock_enabled {
        let mock = match config.ln_settings.mock_auto_accept_secs {
            Some(seconds) => MockLnClient::with_auto_accept(Duration::from_secs(seconds)),
            None => MockLnClient::new(),
        };
        mock.ping().await?;
        info!(
            "Mock LN client configured (auto_accept: {:?})",
            config.ln_settings.mock_auto_accept_secs
        );
        return Ok(Arc::new(mock));
    }
    #[cfg(not(any(feature = "e2e-testing", debug_assertions)))]
    if config.ln_settings.mock_enabled {
        return Err(anyhow!(
            "Mock LN client requires e2e-testing feature or debug build"
        ));
    }
    let ln = LnClient::new(client, config.ln_settings.clone()).await?;
    ln.ping().await?;
    info!("LND client configured");
    Ok(Arc::new(ln))
}

fn create_oracle_client(
    config: &Settings,
    client: Client,
) -> Result<Arc<dyn Oracle>, anyhow::Error> {
    #[cfg(any(feature = "e2e-testing", debug_assertions))]
    if config.coordinator_settings.mock_oracle {
        info!("Mock Oracle configured");
        return Ok(Arc::new(MockOracle::new([0u8; 32])));
    }
    #[cfg(not(any(feature = "e2e-testing", debug_assertions)))]
    if config.coordinator_settings.mock_oracle {
        return Err(anyhow!(
            "Mock Oracle requires e2e-testing feature or debug build"
        ));
    }
    let url = Url::parse(&config.coordinator_settings.oracle_url)
        .map_err(|error| anyhow!("Failed to parse oracle url: {error}"))?;
    let oracle = OracleClient::new(
        build_oracle_reqwest_client(client),
        &url,
        &config.coordinator_settings.private_key_file,
    )?;
    info!("Oracle client configured");
    Ok(Arc::new(oracle))
}

pub async fn build_app(
    config: Settings,
) -> Result<(AppState, TaskTracker, CancellationToken, Vec<DBConnection>), anyhow::Error> {
    let admin_weather = Arc::new(crate::infra::admin_weather::WeatherDiscovery::new(
        &config.coordinator_settings.oracle_url,
    )?);
    let admin_monitoring = Arc::new(
        crate::infra::admin_monitoring::AdminMonitoring::from_settings(
            config.admin_settings.monitoring.as_ref(),
        ),
    );
    info!(
        "Static UI assets configured at {}",
        config.ui_settings.ui_dir
    );

    let bitcoin_client = create_bitcoin_client(&config).await?;
    let http_client = Client::new();
    let reqwest_client = build_reqwest_client(http_client.clone());
    let ln = create_lightning_client(&config, reqwest_client.clone()).await?;
    let oracle_client = create_oracle_client(&config, oracle_http_client()?)?;
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

    // Recovery records read the oracle's key for the contract events they publish.
    let recovery_oracle = oracle_client.clone();
    let recovery = if config.recovery_settings.enabled {
        let recovery = Recovery::load(
            &config.recovery_settings.key_file,
            config.bitcoin_settings.network,
            config.recovery_settings.relays.clone(),
            config.ark_settings.server_url.clone(),
        )?;
        info!(
            "Recovery records enabled, key {}",
            recovery.public_key().to_hex()
        );
        Some(Arc::new(recovery))
    } else {
        None
    };

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
    .with_ark(arkade(&config.ark_settings).await?)?
    .with_arkade_outage_secs(config.ark_settings.arkade_outage_secs)
    .with_network_fee(config.network_fee_settings.clone())?
    .with_kickoff_check(config.kickoff_check_settings.clone())?
    .with_cpfp(config.cpfp_settings.clone())?
    .with_settle_only(
        config.coordinator_settings.settle_only,
        config.coordinator_settings.settle_only_unstarted,
    )
    .with_max_winning_places(config.coordinator_settings.max_winning_places);
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
    // Integrity scans grow with the database. Keep them off the readiness probe.
    let integrity_coordinator = coordinator.clone();
    let integrity_cancel = cancel_token.clone();
    let integrity_task = spawn_supervised(
        &tracker,
        "database integrity",
        cancel_token.clone(),
        async move {
            loop {
                integrity_coordinator.quick_check().await?;
                tokio::select! {
                    () = integrity_cancel.cancelled() => return Ok(()),
                    () = tokio::time::sleep(Duration::from_secs(15 * 60)) => {}
                }
            }
        },
    );
    threads.insert("database integrity".to_string(), integrity_task);
    // Compare the payouts the database knows with what LND sent, once, before anything is paid:
    // a database restored from a backup may be behind it (`restore_reconcile.rs`). It retries
    // until LND answers, and payouts wait for it. It is not a supervised thread, since it ends.
    let reconcile_coordinator = coordinator.clone();
    let reconcile_cancel = cancel_token.clone();
    tracker.spawn(async move {
        loop {
            match reconcile_coordinator.reconcile_after_restore().await {
                Ok(_) => {
                    reconcile_coordinator.reconciled().finish();
                    return;
                }
                Err(e) => error!("Restore reconciliation failed; payouts wait for it: {e:#}"),
            }
            tokio::select! {
                () = reconcile_cancel.cancelled() => return,
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
    });
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

    let mut bitcoin_watcher = BitcoinSyncWatcher::new(
        bitcoin_client.clone(),
        cancel_token.clone(),
        Duration::from_secs(config.bitcoin_settings.refresh_blocks_secs),
    );
    if !config.bitcoin_settings.mock_enabled {
        bitcoin_watcher = bitcoin_watcher.with_headers(
            Arc::new(ElectrumHeaders::new(
                config.bitcoin_settings.electrum_url.clone(),
            )),
            Duration::from_secs(config.bitcoin_settings.refresh_blocks_secs_subscribed),
        );
    }

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

    // Each watcher sweeps slowly while the matching subscription below is connected.
    let invoice_subscription =
        Arc::new(SubscriptionHealth::new("Invoice").with_gauge(LN_INVOICE_SUBSCRIPTION_UP.clone()));
    let payment_subscription =
        Arc::new(SubscriptionHealth::new("Payment").with_gauge(LN_PAYMENT_SUBSCRIPTION_UP.clone()));

    let invoice_watcher = InvoiceWatcher::new(
        coordinator.clone(),
        ln.clone(),
        cancel_token.clone(),
        Duration::from_secs(config.ln_settings.invoice_watch_interval),
    )
    .with_subscription(
        invoice_subscription.clone(),
        Duration::from_secs(config.ln_settings.invoice_watch_interval_subscribed),
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
    )
    .with_subscription(
        payment_subscription.clone(),
        Duration::from_secs(config.ln_settings.payout_watch_interval_subscribed),
    );

    let payout_reconciled = coordinator.reconciled();
    let payout_cancel = cancel_token.clone();
    let payout_watcher_handle = spawn_supervised(
        &tracker,
        "payout watcher",
        cancel_token.clone(),
        async move {
            tokio::select! {
                () = payout_reconciled.wait() => {}
                () = payout_cancel.cancelled() => return Ok(()),
            }
            payout_watcher.watch().await
        },
    );

    threads.insert("payout_watcher".to_string(), payout_watcher_handle);

    let automatic_coordinator = coordinator.clone();
    let automatic_cancel = cancel_token.clone();
    let automatic_handle = spawn_supervised(
        &tracker,
        "automatic payouts",
        cancel_token.clone(),
        async move {
            tokio::select! {
                () = automatic_coordinator.reconciled().wait() => {}
                () = automatic_cancel.cancelled() => return Ok(()),
            }
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

    // Seal the preimages of tickets an older release stored only in plaintext. During a
    // blue/green deploy the older release keeps writing such tickets, so this repeats.
    let preimage_coordinator = coordinator.clone();
    let preimage_cancel = cancel_token.clone();
    let preimage_handle = spawn_supervised(
        &tracker,
        "ticket preimages",
        cancel_token.clone(),
        async move {
            loop {
                tokio::select! {
                    _ = preimage_cancel.cancelled() => break,
                    result = preimage_coordinator.worker_leases().tick(
                        "ticket-preimages",
                        preimage_coordinator
                            .competition_store
                            .backfill_ticket_preimages(TICKET_PREIMAGE_BATCH),
                    ) => match result {
                        Some(Ok(sealed)) if sealed > 0 => info!("Sealed {sealed} ticket preimages"),
                        Some(Err(error)) => error!("Ticket preimage worker: {}", error),
                        _ => {}
                    }
                }
                tokio::select! {
                    _ = preimage_cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
            }
            preimage_coordinator
                .worker_leases()
                .release("ticket-preimages")
                .await;
            Ok(())
        },
    );
    threads.insert("ticket_preimages".to_string(), preimage_handle);

    if coordinator.ark().is_some() {
        let ark_coordinator = coordinator.clone();
        let ark_cancel = cancel_token.clone();
        let ark_handle = spawn_supervised(
            &tracker,
            "escrow swaps",
            cancel_token.clone(),
            async move {
                let mut boards_read_at: Option<std::time::Instant> = None;
                loop {
                    tokio::select! {
                        _ = ark_cancel.cancelled() => break,
                        result = ark_coordinator
                            .worker_leases()
                            .tick("escrow-swaps", ark_coordinator.check_ark_swaps()) => {
                            if let Some(Err(error)) = result { error!("Escrow swap worker: {}", error); }
                        }
                    }
                    // Every instance reads ark-swapd's boards, lease or not: each one decides
                    // for itself whether its ticket route pauses Arkade entries.
                    if boards_read_at.is_none_or(|read| read.elapsed() >= ARK_SWAP_BOARDS_EVERY) {
                        boards_read_at = Some(std::time::Instant::now());
                        tokio::select! {
                            _ = ark_cancel.cancelled() => break,
                            _ = ark_coordinator.read_ark_swap_boards() => {}
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

        // The same process watches the escrows on Arkade, so a payment settles its ticket as
        // soon as the server reports it; the swaps' check lists what this misses.
        let watch_coordinator = coordinator.clone();
        let watch_cancel = cancel_token.clone();
        let watch_handle = spawn_supervised(
            &tracker,
            "escrow subscription",
            cancel_token.clone(),
            async move { watch_coordinator.watch_ark_escrows(watch_cancel).await },
        );
        threads.insert("escrow_subscription".to_string(), watch_handle);
    }

    // Subscription-based watchers for faster payment detection
    // These run alongside the polling watchers as the primary mechanism,
    // with polling serving as a fallback
    let invoice_subscriber = InvoiceSubscriber::new(
        coordinator.clone(),
        ln.clone(),
        cancel_token.clone(),
        invoice_subscription,
    );

    let invoice_subscriber_handle = spawn_supervised(
        &tracker,
        "invoice subscriber",
        cancel_token.clone(),
        async move { invoice_subscriber.subscribe().await },
    );

    threads.insert("invoice_subscriber".to_string(), invoice_subscriber_handle);

    let payment_subscriber = PaymentSubscriber::new(
        coordinator.clone(),
        ln.clone(),
        cancel_token.clone(),
        payment_subscription,
    );

    let payment_subscriber_handle = spawn_supervised(
        &tracker,
        "payment subscriber",
        cancel_token.clone(),
        async move { payment_subscriber.subscribe().await },
    );

    threads.insert("payment_subscriber".to_string(), payment_subscriber_handle);

    // Pages read the oracle's weather from a cache this keeps fresh, not from the oracle.
    let users_info = Arc::new(UserInfo::new(users_store));
    let leaderboards = Arc::new(Leaderboards::new(
        coordinator.clone(),
        users_info.clone(),
        &config.coordinator_settings.oracle_url,
    )?);
    leaderboards.spawn_refresher(&tracker, cancel_token.clone());
    admin_weather.spawn_refresher(&tracker, cancel_token.clone());
    admin_monitoring.spawn_refresher(&tracker, cancel_token.clone(), coordinator.clone());
    if let Some(recovery) = recovery.clone() {
        let publisher = RecoveryPublisher::new(
            recovery,
            coordinator.competition_store.clone(),
            users_info.clone(),
            recovery_oracle,
            coordinator.worker_leases().clone(),
            config.recovery_settings.retention(),
            cancel_token.clone(),
        );
        let recovery_handle = spawn_supervised(
            &tracker,
            "recovery records",
            cancel_token.clone(),
            publisher.run(),
        );
        threads.insert("recovery_records".to_string(), recovery_handle);
    }
    // Feedback is kept in the users database; its alerts go out from one coordinator.
    let feedback_store = crate::domain::feedback::FeedbackStore::new(users_db_clone.clone());
    if config.feedback_settings.enabled {
        match crate::infra::feedback_alerts::FeedbackAlerts::from_settings(
            &config.feedback_settings,
        ) {
            Ok(Some(alerts)) => Arc::new(alerts).spawn(
                feedback_store.clone(),
                coordinator.worker_leases().clone(),
                &tracker,
                cancel_token.clone(),
            ),
            Ok(None) => info!("Feedback is on without alerts (no ntfy URL)"),
            Err(error) => warn!("Feedback alerts are unavailable: {error:#}"),
        }
    }
    tracker.close();

    let wasm_version = crate::api::ui_files::package_version(&config.ui_settings.ui_dir);
    if wasm_version.is_empty() {
        warn!(
            "No WASM package in {}/pkg; browsers cannot log in",
            config.ui_settings.ui_dir
        );
    }
    let satchel_url = config.ui_settings.satchel_origin();
    crate::api::telemetry::set_enabled(config.telemetry.enabled);
    let app_state = AppState {
        admin_monitoring,
        admin_weather,
        ui_dir: config.ui_settings.ui_dir,
        wasm_version,
        private_url: config.ui_settings.private_url,
        remote_url: config.ui_settings.remote_url,
        explorer_url: config
            .bitcoin_settings
            .explorer_url
            .clone()
            .unwrap_or_default(),
        ark_explorer_url: config.ark_settings.explorer_url.clone().unwrap_or_default(),
        satchel_url,
        oracle_url: config.coordinator_settings.oracle_url,
        keymeld_public_url: config
            .keymeld_settings
            .enabled
            .then(|| config.keymeld_settings.browser_gateway_url().to_owned()),
        network: config.bitcoin_settings.network.to_string(),
        coordinator,
        operator_inventory: Arc::new(crate::infra::refresh_cache::RefreshCache::new()),
        users_info,
        leaderboards,
        lnurl,
        bitcoin: bitcoin_client,
        background_threads: Arc::new(threads),
        forgot_password_challenges: Arc::new(RwLock::new(HashMap::new())),
        recovery,
        signup_pow: Arc::new(SignupPow::new(config.pow_settings)),
        http_context: Arc::new(HttpContext::from_settings(&config.http_context)?),
        telemetry_caps: Arc::default(),
        mainnet_signup: Arc::new(crate::domain::mainnet_signup::MainnetSignup::new(
            config.mainnet_signup_settings.enabled,
            users_db_clone.clone(),
        )),
        feedback: Arc::new(crate::domain::feedback::Feedback::new(
            config.feedback_settings.enabled,
            feedback_store,
        )),
        visitor_logs: Arc::new(crate::infra::visitor_logs::VisitorLogs::from_settings(
            &config.admin_settings.logs,
        )),
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
pub fn app(app_state: Arc<AppState>, api: &APISettings) -> Result<Router, anyhow::Error> {
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
    .map_err(|error| anyhow!("Invalid authentication origins: {error}"))?;

    let replay = Arc::new(Nip98ReplayGuard::with_database(
        api.replay_capacity,
        app_state.users_info.auth_database(),
    ));

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
        .route("/username/reset-password", post(forgot_password_reset))
        .route("/pow", post(signup_pow_challenge));
    let users_endpoints = limited(
        users_endpoints,
        &api.rate_limit,
        api.rate_limit.auth_per_second,
        api.rate_limit.auth_burst,
    )?;

    // HTMX public routes (some require JS bridge for auth)
    let htmx_routes = Router::new()
        .route("/competitions", get(competitions_fragment))
        .route(
            "/competitions/entry-counts",
            get(crate::api::routes::player_entry_counts),
        )
        .route(
            "/competitions/{competition_id}/entry-form",
            get(entry_form_fragment),
        )
        .route(
            "/competitions/{competition_id}/entry-forecasts",
            get(entry_forecasts_fragment),
        )
        .route(
            "/competitions/{competition_id}/entry-form/payout",
            get(entry_payout_fragment),
        )
        .route(
            "/competitions/{competition_id}/entry-form/unpaid",
            get(entry_unpaid_fragment),
        )
        .route(
            "/competitions/{competition_id}/entry-form/paid",
            get(entry_paid_fragment),
        )
        .route(
            "/competitions/{competition_id}/tickets/{ticket_id}/status",
            get(ticket_status_fragment),
        )
        .route(
            "/competitions/{competition_id}/leaderboard",
            get(leaderboard_fragment),
        )
        .route(
            "/competitions/{competition_id}/leaderboard/rows",
            get(leaderboard_rows_fragment),
        )
        .route(
            "/competitions/{competition_id}/pools",
            get(crate::api::routes::queue_pools_fragment),
        )
        .route("/entries", get(entries_fragment))
        .route("/entries/{entry_id}/detail", get(entry_detail_fragment))
        .route(
            "/entries/{entry_id}/detail/mine",
            get(crate::api::routes::own_entry_detail_fragment),
        )
        .route("/payouts", get(payouts_fragment))
        .route("/help", get(crate::api::routes::help_fragment));

    let api_routes = Router::new()
        .route("/", get(public_page_handler))
        .merge(htmx_routes)
        .fallback(public_fallback)
        .route("/recover", get(crate::api::routes::recover_page_handler))
        .route("/api/v1/health_check", get(health))
        .route("/api/v1/competitions", get(get_competitions))
        .route(
            "/api/v1/competitions/{competition_id}",
            get(get_competition),
        )
        .route(
            "/api/v1/network-fee",
            get(crate::api::routes::get_network_fee),
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
            "/api/v1/competitions/{competition_id}/tickets/unpaid",
            get(get_unpaid_tickets),
        )
        .route(
            "/api/v1/competitions/{competition_id}/tickets/paid",
            get(get_paid_tickets),
        )
        .route(
            "/api/v1/competitions/{competition_id}/tickets/{ticket_id}/status",
            get(get_ticket_status),
        )
        .route(
            "/api/v1/competitions/{competition_id}/tickets/{ticket_id}/registration",
            post(register_ticket),
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
        .route(
            "/api/v1/recovery/info",
            get(crate::api::routes::get_recovery_info),
        )
        .route(
            "/api/v1/recovery/kit",
            get(crate::api::routes::get_recovery_kit),
        )
        .nest("/api/v1/users", users_endpoints);
    let api_routes = limited(
        api_routes,
        &api.rate_limit,
        api.rate_limit.per_second,
        api.rate_limit.burst,
    )?;

    // Browser telemetry has its own caps, so beacons never use up a client's request limit.
    let telemetry = Router::new()
        .route(
            "/api/v1/telemetry",
            post(crate::api::telemetry::post_telemetry),
        )
        .layer(DefaultBodyLimit::max(crate::api::telemetry::MAX_BODY_BYTES));

    // The feedback form has its own limits (domain::feedback), so it never uses up a
    // client's request limit either.
    let feedback = Router::new()
        .route(
            "/mainnet-signup",
            get(crate::api::routes::mainnet_signup_page_handler)
                .post(crate::api::routes::post_mainnet_signup),
        )
        .route(
            "/api/v1/mainnet-signup",
            post(crate::api::routes::post_mainnet_signup),
        )
        .route(
            "/api/v1/mainnet-signup/challenge",
            get(crate::api::routes::mainnet_signup_challenge),
        )
        .route(
            "/feedback",
            get(crate::api::routes::feedback_page_handler).post(crate::api::routes::post_feedback),
        )
        .route(
            "/api/v1/feedback/challenge",
            get(crate::api::routes::feedback_challenge),
        )
        .route("/api/v1/feedback", post(crate::api::routes::post_feedback))
        .layer(DefaultBodyLimit::max(
            crate::api::routes::FEEDBACK_MAX_BODY_BYTES,
        ));

    // The wallet also fetches the assigned enclave's attestation from Keymeld. Pages look up
    // the player's Satchel address and post the Satchel sign-in form, when it is configured.
    let satchel = app_state.satchel_url.as_deref().unwrap_or_default();
    let public_headers = Arc::new(PublicHeaders::new(
        &[
            app_state.remote_url.as_str(),
            app_state.oracle_url.as_str(),
            app_state.keymeld_public_url.as_deref().unwrap_or_default(),
            satchel,
        ],
        &[satchel],
    ));

    let http_context = app_state.http_context.clone();
    Ok(Router::new()
        .merge(api_routes)
        .merge(telemetry)
        .merge(feedback)
        .merge(static_files(&app_state))
        .layer(compression())
        .layer(Extension(replay))
        .layer(Extension(Arc::new(nip98_origins)))
        .layer(middleware::from_fn_with_state(
            public_headers,
            public_response_headers,
        ))
        .layer(middleware::from_fn_with_state(
            http_context,
            request_context_middleware,
        ))
        .with_state(app_state)
        .layer(cors))
}

/// Per-client limit on every route of `router`, unless limiting is off.
/// `per_second` is the sustained rate, `burst` the allowance above it.
fn limited<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    settings: &RateLimitSettings,
    per_second: u32,
    burst: u32,
) -> Result<Router<S>, anyhow::Error> {
    if !settings.enabled {
        return Ok(router);
    }
    let config = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_nanos(
                1_000_000_000_u64.div_ceil(u64::from(per_second.max(1))),
            ))
            .burst_size(burst.max(1))
            .key_extractor(PeerIpKeyExtractor)
            .finish()
            .ok_or_else(|| anyhow!("Invalid request rate limit configuration"))?,
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
    Ok(router.route_layer(GovernorLayer::new(config)))
}

/// Admin listener: operator pages, the LND wallet API, competition creation, viewing and
/// deletion, escrow refund write-offs, and republishing recovery records, for scripts and
/// `coordinator admin`.
///
/// Everything except the sign-in form and static assets sits behind `require_operator`.
/// No CORS layer: operator pages call only their own origin. The test-settle route,
/// which marks tickets paid without a payment, is never registered on mainnet.
pub fn admin_app(app_state: Arc<AppState>, access: Arc<AdminAccess>, network: Network) -> Router {
    let mut admin_htmx_routes = Router::new()
        .route(
            "/",
            get(|| async { axum::response::Redirect::to("/admin/operations") }),
        )
        .route("/competitions", get(admin_page_handler))
        .route("/competition", get(admin_competition_fragment))
        .route("/competition/map", get(admin_competition_map))
        .route("/operations", get(crate::api::routes::operations_page))
        .route("/keymeld", get(crate::api::routes::keymeld_page))
        .route("/services", get(crate::api::routes::services_page))
        .route(
            "/mainnet-signups",
            get(crate::api::routes::admin_mainnet_signups),
        )
        .route(
            "/mainnet-signups.csv",
            get(crate::api::routes::admin_mainnet_signups_csv),
        )
        .route("/feedback", get(crate::api::routes::admin_feedback_list))
        .route(
            "/feedback/unread",
            get(crate::api::routes::admin_feedback_unread),
        )
        .route(
            "/feedback/{id}",
            get(crate::api::routes::admin_feedback_detail)
                .post(crate::api::routes::admin_feedback_update),
        )
        .route("/visitors", get(crate::api::routes::visitors_page_handler))
        .route(
            "/visitors/results",
            get(crate::api::routes::visitors_results_handler),
        )
        .route("/funds", get(crate::api::routes::funds_page))
        .route("/funds/tickets/{id}", get(crate::api::routes::funds_ticket))
        .route("/funds/chain/{id}", get(crate::api::routes::funds_chain))
        .route(
            "/operations/{id}",
            get(crate::api::routes::operation_detail),
        )
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
        )
        .route(
            "/api/owed-winners/approve-sweep",
            post(admin_approve_owed_winner_sweep_handler),
        )
        .route(
            "/api/owed-winners/settle",
            post(admin_settle_owed_winner_handler),
        )
        .route(
            "/api/recovery/republish",
            post(crate::api::routes::admin_republish_recovery_handler),
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
        .route(
            "/api/v1/competitions/queued",
            post(create_queued_competition),
        )
        .route("/api/v1/admin/competitions", get(operator_competitions))
        .route(
            "/api/v1/admin/competitions/{competition_id}",
            get(operator_competition).delete(operator_delete_competition),
        )
        .route(
            "/api/v1/admin/refunds/write-off",
            post(operator_write_off_refunds),
        )
        .route("/api/v1/admin/payout-holds", get(operator_payout_holds))
        .route(
            "/api/v1/admin/payout-holds/{entry_id}/release",
            post(operator_release_payout_hold),
        )
        .route("/api/v1/admin/owed-winners", get(operator_owed_winners))
        .route(
            "/api/v1/admin/owed-winners/{entry_id}/approve-sweep",
            post(operator_approve_owed_winner_sweep),
        )
        .route(
            "/api/v1/admin/owed-winners/{entry_id}/settle",
            post(operator_settle_owed_winner),
        )
        .route(
            "/api/v1/admin/recovery",
            get(crate::api::routes::operator_recovery_status),
        )
        .route(
            "/api/v1/admin/recovery/republish",
            post(crate::api::routes::operator_republish_recovery),
        )
        .route_layer(middleware::from_fn_with_state(
            access.clone(),
            require_operator,
        ))
        .with_state(app_state.clone());

    let sign_in = Router::new()
        .route("/admin/login", get(admin_login_page).post(admin_login))
        .with_state(access);

    let http_context = app_state.http_context.clone();
    Router::new()
        .merge(operator_routes)
        .merge(sign_in)
        .merge(static_files(&app_state))
        .with_state(app_state)
        .layer(middleware::from_fn(operator_response_headers))
        .layer(compression())
        .layer(middleware::from_fn_with_state(
            http_context,
            request_context_middleware,
        ))
}

/// A page for any path the public site does not serve, with status 404. Paths
/// reserved for the API or the operator listener get a bare 404, so a missing
/// operator route cannot look present.
async fn public_fallback(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    headers: header::HeaderMap,
) -> Response {
    let path = uri.path();
    let reserved = ["/admin", "/api"]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")));
    if reserved {
        return StatusCode::NOT_FOUND.into_response();
    }
    not_found(&headers, &state, "Page")
}

/// The WASM package (`/ui`) and the embedded scripts and styles (`/assets`).
fn static_files(state: &AppState) -> Router<Arc<AppState>> {
    crate::api::ui_files::router(&state.ui_dir, state.wasm_version.clone())
        .route("/assets/{file}", get(crate::templates::assets::serve_asset))
}

/// Gzip for browsers that accept it. The public listener compresses its static
/// files; the operator listener compresses every response, pages included. Byte
/// ranges, event streams and responses that already carry an encoding pass as they are.
fn compression() -> CompressionLayer {
    CompressionLayer::new()
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

/// The LND client. Its calls carry no `X-Parent-Request-Id`; see [`build_oracle_reqwest_client`].
pub fn build_reqwest_client(client: Client) -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .with(LoggingMiddleware)
        .build()
}

/// The longest one request to the oracle may take, from connecting to the end of its body.
/// Without a limit, a connection the oracle, or the network on the way to it, accepted and then
/// never answered held a competition step, and one of the few step slots, indefinitely. A
/// request that times out is retried within the bound of [`build_oracle_reqwest_client`].
pub const ORACLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest connecting to the oracle may take.
pub const ORACLE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The HTTP client the coordinator reaches the oracle with: every request is bounded by
/// [`ORACLE_CONNECT_TIMEOUT`] and [`ORACLE_REQUEST_TIMEOUT`].
pub fn oracle_http_client() -> Result<Client, anyhow::Error> {
    oracle_http_client_with(ORACLE_CONNECT_TIMEOUT, ORACLE_REQUEST_TIMEOUT)
}

fn oracle_http_client_with(connect: Duration, request: Duration) -> Result<Client, anyhow::Error> {
    Client::builder()
        .connect_timeout(connect)
        .timeout(request)
        .build()
        .map_err(|error| anyhow!("Failed to build the oracle HTTP client: {error}"))
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
        .with(ParentRequestIdMiddleware)
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

async fn shutdown_signal() -> std::io::Result<()> {
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    select! {
        _ = sigint.recv() => info!("Received SIGINT signal"),
        _ = sigterm.recv() => info!("Received SIGTERM signal"),
    }
    Ok(())
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use crate::api::admin_auth::{AdminCredentials, CSRF_HEADER, SESSION_COOKIE};
    use crate::domain::Competition;
    use axum::{body::to_bytes, http::HeaderMap};
    use std::io::Write;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const SETTLE_PATH: &str = "/admin/api/test/settle-invoice/0190b7a4-0000-7000-8000-000000000000";
    const COMPETITION_PATH: &str =
        "/api/v1/admin/competitions/0190b7a4-0000-7000-8000-000000000000";
    const PAYOUT_HOLD_RELEASE_PATH: &str =
        "/api/v1/admin/payout-holds/0190b7a4-0000-7000-8000-000000000000/release";
    const OWED_WINNER_APPROVE_PATH: &str =
        "/api/v1/admin/owed-winners/0190b7a4-0000-7000-8000-000000000000/approve-sweep";
    const OWED_WINNER_SETTLE_PATH: &str =
        "/api/v1/admin/owed-winners/0190b7a4-0000-7000-8000-000000000000/settle";
    const FEEDBACK_PATH: &str = "/admin/feedback/0190b7a4-0000-7000-8000-000000000000";

    /// Every operator route, including the sign-in form, as (method, path).
    const OPERATOR_ROUTES: &[(&str, &str)] = &[
        ("GET", "/admin/funds"),
        (
            "GET",
            "/admin/funds/tickets/00000000-0000-0000-0000-000000000001",
        ),
        (
            "GET",
            "/admin/funds/chain/00000000-0000-0000-0000-000000000001",
        ),
        ("GET", "/admin"),
        ("GET", "/admin/competition"),
        ("GET", "/admin/competition/map"),
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
        ("POST", "/api/v1/competitions/queued"),
        ("GET", "/api/v1/admin/competitions"),
        ("GET", COMPETITION_PATH),
        ("DELETE", COMPETITION_PATH),
        ("POST", "/api/v1/admin/refunds/write-off"),
        ("GET", "/api/v1/admin/payout-holds"),
        ("POST", PAYOUT_HOLD_RELEASE_PATH),
        ("POST", "/admin/api/owed-winners/approve-sweep"),
        ("POST", "/admin/api/owed-winners/settle"),
        ("GET", "/api/v1/admin/owed-winners"),
        ("POST", OWED_WINNER_APPROVE_PATH),
        ("POST", OWED_WINNER_SETTLE_PATH),
        ("POST", "/admin/api/recovery/republish"),
        ("GET", "/api/v1/admin/recovery"),
        ("POST", "/api/v1/admin/recovery/republish"),
        ("GET", "/admin/mainnet-signups"),
        ("GET", "/admin/mainnet-signups.csv"),
        ("GET", "/admin/feedback"),
        ("GET", "/admin/feedback/unread"),
        ("GET", FEEDBACK_PATH),
        ("POST", FEEDBACK_PATH),
        ("GET", "/admin/visitors"),
        ("GET", "/admin/visitors/results"),
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
            Self::start_with(|_| {}).await
        }

        async fn start_with(configure: impl FnOnce(&mut Settings)) -> Self {
            let data = tempfile::tempdir().unwrap();
            let mut settings = Settings::default();
            settings.db_settings.data_folder = data.path().display().to_string();
            settings.bitcoin_settings.mock_enabled = true;
            settings.ln_settings.mock_enabled = true;
            settings.coordinator_settings.mock_oracle = true;
            settings.coordinator_settings.oracle_url = String::from("mock://oracle");
            settings.recovery_settings.key_file =
                data.path().join("recovery_key.pem").display().to_string();
            configure(&mut settings);
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
            .unwrap()
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
    async fn the_recovery_page_may_reach_relays_and_esplora_but_runs_only_its_own_script() {
        let test = TestState::start().await;
        let public = test.public();
        let (status, headers, body) = send(&public, request("GET", "/recover", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        let policy = headers["content-security-policy"].to_str().unwrap();
        assert!(
            policy.contains("connect-src 'self' https: wss:;"),
            "{policy}"
        );
        assert!(policy.contains("script-src 'self' 'wasm-unsafe-eval';"));
        assert!(body.contains("id=\"recover-form\""));
        assert!(body.contains("/ui/pkg/coordinator_wasm.js"));
        // Every other page keeps its narrow policy.
        let (_, headers, _) = send(&public, request("GET", "/help", &[], "")).await;
        let policy = headers["content-security-policy"].to_str().unwrap();
        assert!(!policy.contains("wss:"), "{policy}");
    }

    #[tokio::test]
    async fn public_pages_carry_the_content_security_policy() {
        let mut test = TestState::start().await;
        Arc::get_mut(&mut test.state).unwrap().keymeld_public_url =
            Some("https://keymeld.example.net/enclaves".into());
        let public = test.public();
        for path in ["/", "/competitions", "/entries", "/payouts", "/help"] {
            for (kind, headers) in [
                ("page", &[][..]),
                ("fragment", &[("hx-request", "true")][..]),
                ("history", &[("hx-history-restore-request", "true")][..]),
            ] {
                let (status, response_headers, body) =
                    send(&public, request("GET", path, headers, "")).await;
                let account = path == "/entries" || path == "/payouts";
                assert_eq!(
                    status,
                    if account && kind == "fragment" {
                        StatusCode::UNAUTHORIZED
                    } else {
                        StatusCode::OK
                    }
                );
                let policy = response_headers["content-security-policy"]
                    .to_str()
                    .unwrap();
                assert!(
                    policy.contains("script-src 'self' 'wasm-unsafe-eval';"),
                    "{path}: {policy}"
                );
                assert!(
                    policy.contains("require-trusted-types-for 'script'"),
                    "{path}"
                );
                assert!(policy.contains("trusted-types htmx"), "{path}");
                assert!(policy.contains("frame-ancestors 'none'"), "{path}");
                assert!(
                    policy.contains("https://keymeld.example.net"),
                    "the wallet needs its configured attestation gateway"
                );
                assert_eq!(response_headers["x-content-type-options"], "nosniff");
                assert!(
                    response_headers["cache-control"]
                        .to_str()
                        .unwrap()
                        .contains("no-transform"),
                    "{path}"
                );
                assert!(!body.contains(" onclick="), "{path}");
                assert_eq!(
                    body.contains("<!DOCTYPE html>"),
                    kind != "fragment",
                    "{kind} {path}"
                );
                if account {
                    assert_eq!(
                        response_headers["cache-control"],
                        "private, no-store, no-transform"
                    );
                    assert!(body.contains("sign-in-required"));
                }
            }
        }
        test.stop().await;
    }

    /// The Live and Finished tabs answer as pages and as htmx swaps, searched or not, and
    /// a search that finds nothing says so.
    #[tokio::test]
    async fn the_competition_tabs_are_served_and_searched() {
        let test = TestState::start().await;
        let public = test.public();
        for path in [
            "/competitions?show=live",
            "/competitions?show=finished&page=3&cancelled=1",
            "/competitions?show=finished&q=no+such+competition",
            "/?show=live&q=KPWM",
        ] {
            for headers in [&[][..], &[("hx-request", "true")][..]] {
                let (status, _, body) = send(&public, request("GET", path, headers, "")).await;
                assert_eq!(status, StatusCode::OK, "{path}");
                assert!(body.contains(r#"class="competition-search""#), "{path}");
                assert!(body.contains(r#"aria-current="page""#), "{path}");
            }
        }
        let (_, _, body) = send(
            &public,
            request(
                "GET",
                "/competitions?show=finished&q=no+such+competition",
                &[],
                "",
            ),
        )
        .await;
        assert!(body.contains("Nothing matches “no such competition”."));
        test.stop().await;
    }

    #[tokio::test]
    async fn the_recovery_file_needs_a_signature_and_holds_only_its_players_records() {
        use crate::domain::{recovery::RecoveryKit, RecoveryOutboxEvent};
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

        let test = TestState::start().await;
        let public = test.public();
        let (status, _, _) = send(&public, request("GET", "/api/v1/recovery/info", &[], "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        test.stop().await;

        let test = TestState::start_with(|settings| {
            settings.recovery_settings.enabled = true;
        })
        .await;
        let public = test.public();
        let recovery = test.state.recovery.clone().unwrap();
        let (status, _, body) =
            send(&public, request("GET", "/api/v1/recovery/info", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(info["coordinator_pubkey"], recovery.public_key().to_hex());
        assert_eq!(info["network"], test.state.network);
        let (status, _, _) = send(&public, request("GET", "/api/v1/recovery/kit", &[], "")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let alice = nostr::Keys::generate();
        let bob = nostr::Keys::generate();
        let mut events = Vec::new();
        for keys in [&alice, &bob] {
            let user = keys.public_key();
            let competition_id = uuid::Uuid::now_v7();
            let d_tag = format!("{}:entry:{competition_id}", recovery.blind(&user));
            let content = recovery.encrypt(&user, &user.to_hex()).unwrap();
            let event = nostr::EventBuilder::new(nostr::Kind::ApplicationSpecificData, content)
                .sign_with_keys(keys)
                .unwrap();
            events.push(RecoveryOutboxEvent {
                d_tag,
                kind: "entry",
                user_pubkey: Some(user.to_hex()),
                competition_id: Some(competition_id),
                content_sha256: String::new(),
                event_json: serde_json::to_string(&event).unwrap(),
                created_at: 0,
            });
        }
        test.state
            .coordinator
            .competition_store
            .put_recovery_events(events, 0, Some(0))
            .await
            .unwrap();

        let url = format!("{}/api/v1/recovery/kit", test.state.remote_url);
        let auth = crate::api::extractors::create_auth_event("GET", &url, None, &alice)
            .await
            .unwrap();
        let header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&auth).unwrap())
        );
        let (status, headers, body) = send(
            &public,
            request(
                "GET",
                "/api/v1/recovery/kit",
                &[("authorization", &header)],
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"coordinator-recovery-npub1"));
        assert!(headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("no-store"));
        let kit: RecoveryKit = serde_json::from_str(&body).unwrap();
        assert_eq!(kit.user_pubkey, alice.public_key().to_hex());
        // Alice has no account here, so no wallet backup.
        assert_eq!(kit.wallet, None);
        assert_eq!(kit.entries.len(), 1);
        let plaintext = nostr::nips::nip44::decrypt(
            alice.secret_key(),
            &recovery.public_key(),
            &kit.entries[0],
        )
        .unwrap();
        assert_eq!(plaintext, alice.public_key().to_hex());
        assert!(!body.contains(&bob.public_key().to_hex()));
        test.stop().await;
    }

    #[tokio::test]
    async fn operators_see_what_each_relay_lacks_and_republish_to_one() {
        use crate::domain::{
            recovery::{RecoveryStatus, RepublishReport},
            RecoveryOutboxEvent,
        };
        const ONE: &str = "wss://relay.one.example";
        const TWO: &str = "wss://relay.two.example";
        let test = TestState::start_with(|settings| {
            settings.recovery_settings.enabled = true;
            settings.recovery_settings.relays = vec![ONE.into(), TWO.into()];
        })
        .await;
        let admin = test.admin(token_access(), Network::Regtest);
        let recovery = test.state.recovery.clone().unwrap();
        let mut events = Vec::new();
        for _ in 0..2 {
            let keys = nostr::Keys::generate();
            let user = keys.public_key();
            let event = nostr::EventBuilder::new(nostr::Kind::ApplicationSpecificData, "x")
                .sign_with_keys(&keys)
                .unwrap();
            events.push(RecoveryOutboxEvent {
                d_tag: recovery.wallet_d_tag(&user),
                kind: "wallet",
                user_pubkey: Some(user.to_hex()),
                competition_id: None,
                content_sha256: String::new(),
                event_json: serde_json::to_string(&event).unwrap(),
                created_at: 0,
            });
        }
        test.state
            .coordinator
            .competition_store
            .put_recovery_events(events, 0, Some(0))
            .await
            .unwrap();
        let bearer = [("authorization", BEARER)];
        let json = [
            ("authorization", BEARER),
            ("content-type", "application/json"),
        ];

        let (status, _, body) = send(
            &admin,
            request("GET", "/api/v1/admin/recovery", &bearer, ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let status: RecoveryStatus = serde_json::from_str(&body).unwrap();
        assert!(status.enabled);
        assert_eq!(status.coordinator_pubkey, recovery.public_key().to_hex());
        assert_eq!(status.relays.len(), 2);
        assert_eq!(status.relays[1].url, TWO);
        assert_eq!(status.relays[1].missing, 2);

        let (status, _, body) = send(
            &admin,
            request(
                "POST",
                "/api/v1/admin/recovery/republish",
                &json,
                r#"{"relays":["wss://elsewhere.example"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("not one of [recovery].relays"), "{body}");

        let (status, _, body) = send(
            &admin,
            request(
                "POST",
                "/api/v1/admin/recovery/republish",
                &json,
                &format!(r#"{{"relays":["{TWO}"]}}"#),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let report: RepublishReport = serde_json::from_str(&body).unwrap();
        assert_eq!(report.queued, 2);
        assert_eq!(report.relays, [TWO]);

        let (status, _, body) = send(
            &admin,
            request(
                "POST",
                "/admin/api/recovery/republish",
                &[("authorization", BEARER), ("content-type", FORM)],
                "relay=wss%3A%2F%2Frelay.two.example",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("Queued 2 records for wss://relay.two.example"),
            "{body}"
        );

        let (status, _, body) = send(&admin, request("GET", "/admin/services", &bearer, "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Recovery records"), "{body}");
        assert!(body.contains("value=\"wss://relay.two.example\""), "{body}");
        test.stop().await;

        // While recovery records are off there is nothing to republish.
        let test = TestState::start().await;
        let admin = test.admin(token_access(), Network::Regtest);
        let (status, _, body) = send(
            &admin,
            request("GET", "/api/v1/admin/recovery", &bearer, ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !serde_json::from_str::<RecoveryStatus>(&body)
                .unwrap()
                .enabled
        );
        let (status, _, body) = send(
            &admin,
            request("POST", "/api/v1/admin/recovery/republish", &json, "{}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("recovery records are off"), "{body}");
        test.stop().await;
    }

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

    /// A settled competition in the list has neither its signed contract nor its event
    /// announcement, and every other field as its own route has it. That route, which the
    /// payouts page reads, keeps everything a payout authorization signs over.
    #[tokio::test]
    async fn the_list_leaves_the_signed_contract_and_announcement_to_the_competition_route() {
        let test = TestState::start().await;
        let public = test.public();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../synth/src/fixtures/lab-competition.json"
        ))
        .unwrap();
        let field = |name: &str| fixture[name].clone();
        let mut competition =
            Competition::new(&serde_json::from_value(field("event_submission")).unwrap());
        competition.contract_parameters =
            serde_json::from_value(field("contract_parameters")).unwrap();
        competition.event_announcement =
            serde_json::from_value(field("event_announcement")).unwrap();
        competition.signed_contract = serde_json::from_value(field("signed_contract")).unwrap();
        competition.attestation = serde_json::from_value(field("attestation")).unwrap();
        competition.funding_outpoint = serde_json::from_value(field("funding_outpoint")).unwrap();
        let now = time::OffsetDateTime::now_utc();
        competition.signed_at = Some(now);
        competition.funding_broadcasted_at = Some(now);
        competition.outcome_broadcasted_at = Some(now);
        competition.completed_at = Some(now);
        let store = &test.state.coordinator.competition_store;
        store
            .add_competition_with_tickets(competition.clone(), vec![])
            .await
            .unwrap();
        store
            .update_competitions(vec![competition.clone()])
            .await
            .unwrap();

        let (status, _, body) =
            send(&public, request("GET", "/api/v1/competitions", &[], "")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let listed: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let listed = listed
            .iter()
            .find(|listed| listed["id"] == competition.id.to_string())
            .expect("a competition settled now is on the default page");
        assert!(listed["signed_contract"].is_null(), "{listed}");
        assert!(listed["event_announcement"].is_null(), "{listed}");

        let path = format!("/api/v1/competitions/{}", competition.id);
        let (status, _, body) = send(&public, request("GET", &path, &[], "")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let own: serde_json::Value = serde_json::from_str(&body).unwrap();
        for read_by_payouts in [
            "/contract_parameters/players",
            "/contract_parameters/outcome_payouts",
            "/contract_parameters/funding_value",
            "/event_announcement/locking_points",
            "/attestation",
            "/funding_outpoint",
            "/signed_contract/signatures",
        ] {
            assert!(
                own.pointer(read_by_payouts)
                    .is_some_and(|value| !value.is_null()),
                "{read_by_payouts} missing from {own}"
            );
        }
        let mut expected = own;
        expected["signed_contract"] = serde_json::Value::Null;
        expected["event_announcement"] = serde_json::Value::Null;
        assert_eq!(listed, &expected);
        test.stop().await;
    }

    /// In settle-only mode the health check says so, no ticket can be priced, the public pages
    /// show only that entries are paused, and the reconciliation at start lets payouts run.
    #[tokio::test]
    async fn settle_only_mode_takes_no_new_money_and_says_so() {
        let test = TestState::start_with(|settings| {
            settings.coordinator_settings.settle_only = true;
        })
        .await;
        let public = test.public();
        let (status, _, body) =
            send(&public, request("GET", "/api/v1/health_check", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""settle_only":true"#), "{body}");
        let (status, _, body) = send(&public, request("GET", "/api/v1/network-fee", &[], "")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("Entries are paused"), "{body}");
        let (status, _, body) = send(&public, request("GET", "/competitions", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"id="entriesPausedBanner""#), "{body}");
        assert!(!body.to_lowercase().contains("settle-only"), "{body}");
        tokio::time::timeout(
            Duration::from_secs(10),
            test.state.coordinator.reconciled().wait(),
        )
        .await
        .expect("the reconciliation at start finishes against mock LND");
        test.stop().await;

        let test = TestState::start().await;
        let (_, _, body) = send(
            &test.public(),
            request("GET", "/api/v1/health_check", &[], ""),
        )
        .await;
        assert!(body.contains(r#""settle_only":false"#), "{body}");
        test.stop().await;
    }

    #[tokio::test]
    async fn unknown_public_paths_get_a_not_found_page() {
        let test = TestState::start().await;
        let public = test.public();
        let (status, _, body) = send(&public, request("GET", "/no/such/page", &[], "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("<!DOCTYPE html>") && body.contains("Page not found"));
        let (status, _, body) = send(
            &public,
            request("GET", "/no/such/page", &[("hx-request", "true")], ""),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.contains("<!DOCTYPE html>"));
        let (status, _, _) = send(&public, request("GET", "/", &[], "")).await;
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
                    && (status != StatusCode::NOT_FOUND
                        || path == &SETTLE_PATH
                        || path == &COMPETITION_PATH
                        || path == &PAYOUT_HOLD_RELEASE_PATH
                        || path == &OWED_WINNER_APPROVE_PATH
                        || path == &OWED_WINNER_SETTLE_PATH
                        || path == &FEEDBACK_PATH),
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

    /// `coordinator admin` against the real operator listener: create, list, show and delete a
    /// competition with the bearer token, and be refused without it.
    #[tokio::test]
    async fn the_admin_command_line_drives_competitions_through_the_operator_listener() {
        use crate::admin_cli::{list_table, show_text, AdminClient};

        let test = TestState::start().await;
        let admin = test.admin(token_access(), Network::Regtest);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, admin).await.unwrap() });

        let mut token = tempfile::NamedTempFile::new().unwrap();
        writeln!(token, "  {TOKEN}  ").unwrap();
        let client = AdminClient::with_token_file(&url, Some(token.path())).unwrap();

        let now = time::OffsetDateTime::now_utc();
        let event = crate::domain::CreateEvent {
            id: uuid::Uuid::now_v7(),
            signing_date: now + time::Duration::hours(33),
            start_observation_date: now + time::Duration::hours(6),
            end_observation_date: now + time::Duration::hours(30),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 1,
            number_of_places_win: 1,
            total_allowed_entries: 3,
            entry_fee: 5000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(5),
            total_competition_pool: 15000,
            relative_locktime_block_delta: None,
            unlisted: true,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
            contract_options: None,
        };
        let id = client.create(&event).await.unwrap();
        assert_eq!(id, event.id);

        let listed = client.competitions().await.unwrap();
        let created = listed.iter().find(|c| c.id == id).expect("listed");
        assert_eq!(created.state, "created");
        assert!(created.event_submission.unlisted);
        assert!(created.refunds.is_none());
        assert!(list_table(&listed).contains(&id.to_string()));

        let shown = client.competition(id).await.unwrap();
        assert_eq!(shown.milestones[0].name, "created");
        let text = show_text(&shown);
        assert!(
            text.contains("KDEN") && text.contains("no funded escrows"),
            "{text}"
        );

        let missing = client.competition(uuid::Uuid::now_v7()).await.unwrap_err();
        assert!(missing.to_string().contains("404"), "{missing:#}");

        let anonymous = AdminClient::new(&url, None).unwrap();
        let refused = anonymous.competitions().await.unwrap_err();
        assert!(refused.to_string().contains("401"), "{refused:#}");

        client.delete(id).await.unwrap();
        assert!(client.competition(id).await.is_err());

        server.abort();
        let _ = server.await;
        test.stop().await;
    }

    #[tokio::test]
    async fn operator_responses_are_gzipped_and_only_assets_are_cached() {
        let test = TestState::start().await;
        let admin = test.admin(token_access(), Network::Regtest);
        let gzip = [("authorization", BEARER), ("accept-encoding", "gzip")];
        for path in ["/admin/wallet", "/admin/services", "/admin/competition"] {
            let (status, headers, _) = send(&admin, request("GET", path, &gzip, "")).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(headers["content-encoding"], "gzip", "{path}");
            assert_eq!(headers["cache-control"], "no-store", "{path}");
        }
        let asset = crate::templates::assets::WEATHER_MAP_JS.url;
        let (status, headers, _) = send(&admin, request("GET", asset, &gzip, "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-encoding"], "gzip");
        assert!(headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable"));
        let (_, headers, _) = send(
            &admin,
            request("GET", "/admin/wallet", &[("authorization", BEARER)], ""),
        )
        .await;
        assert!(!headers.contains_key("content-encoding"));
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

    /// A JSON POST to `path` signed by `keys` with NIP-98, as the browser sends it.
    async fn signed_post(
        test: &TestState,
        keys: &nostr::Keys,
        path: &str,
        body: &serde_json::Value,
    ) -> Request<Body> {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        use nostr::hashes::{sha256::Hash as Sha256Hash, Hash};
        let body = body.to_string();
        let url = format!("{}{path}", test.state.remote_url);
        let auth = crate::api::extractors::create_auth_event(
            "POST",
            &url,
            Some(Sha256Hash::hash(body.as_bytes())),
            keys,
        )
        .await
        .unwrap();
        let header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&auth).unwrap())
        );
        request(
            "POST",
            path,
            &[
                ("authorization", &header),
                ("content-type", "application/json"),
            ],
            &body,
        )
    }

    /// Whether SHA-256(challenge ‖ nonce big-endian) starts with `difficulty` zero bits; written
    /// apart from `domain::users::SignupPow` to check it.
    fn solves_pow(challenge: &[u8], nonce: u64, difficulty: u64) -> bool {
        use sha2::{Digest, Sha256};
        let hash = Sha256::new()
            .chain_update(challenge)
            .chain_update(nonce.to_be_bytes())
            .finalize();
        let zeros = hash.iter().position(|byte| *byte != 0).map_or(256, |at| {
            at as u64 * 8 + u64::from(hash[at].leading_zeros())
        });
        zeros >= difficulty
    }

    /// Form fields carrying a solved feedback proof of work.
    async fn feedback_proof(public: &Router) -> String {
        public_form_proof(public, "/api/v1/feedback/challenge").await
    }

    async fn public_form_proof(public: &Router, path: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let (status, headers, body) = send(public, request("GET", path, &[], "")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(headers["cache-control"], "no-store");
        let issued: serde_json::Value = serde_json::from_str(&body).unwrap();
        let challenge = issued["challenge"].as_str().unwrap();
        let bytes = URL_SAFE_NO_PAD.decode(challenge).unwrap();
        let difficulty = issued["difficulty"].as_u64().unwrap();
        assert_eq!(
            difficulty,
            u64::from(crate::domain::feedback::FEEDBACK_POW_BITS)
        );
        let nonce = (0u64..)
            .find(|nonce| solves_pow(&bytes, *nonce, difficulty))
            .unwrap();
        format!("pow_challenge={challenge}&pow_nonce={nonce}")
    }

    const SESSION: &str = "Xq3vT9mPa1Lw0Zb8Yc7Rkd";

    fn feedback_post(path: &str, htmx: bool, body: &str) -> Request<Body> {
        let mut headers = vec![
            ("content-type", FORM),
            ("x-session-id", SESSION),
            ("user-agent", "test-agent/1.0"),
        ];
        if htmx {
            headers.push(("hx-request", "true"));
            headers.push((
                "hx-current-url",
                "http://127.0.0.1:9990/competitions?secret=1#x",
            ));
        }
        request("POST", path, &headers, body)
    }

    #[tokio::test]
    async fn mainnet_signup_stores_unique_emails_and_exports_only_to_the_operator() {
        let test = TestState::start().await;
        let public = test.public();
        let store = &test.state.mainnet_signup.store;
        let (status, _, home) = send(&public, request("GET", "/", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(home.contains("data-mainnet-signup-open"));
        assert!(home.contains("id=\"mainnetSignupModal\""));
        let (status, _, form) = send(&public, request("GET", "/mainnet-signup", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(form.contains("type=\"email\""));
        assert!(form.contains("only for this announcement"));

        for email in [
            "",
            "invalid",
            "a%0Ab%40example.com",
            "a%40example.com%2Cb%40example.com",
        ] {
            let (status, headers, body) = send(
                &public,
                feedback_post("/mainnet-signup", false, &format!("email={email}")),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(headers["cache-control"]
                .to_str()
                .unwrap()
                .contains("no-store"));
            assert!(body.contains("Enter a valid email address."));
        }
        let (_, _, body) = send(
            &public,
            feedback_post(
                "/api/v1/mainnet-signup",
                true,
                "email=%22%3E%3Cscript%3Ex%3C%2Fscript%3E",
            ),
        )
        .await;
        assert!(!body.contains("<script>"));
        assert!(body.contains("&lt;script&gt;"));
        let (status, _, body) = send(
            &public,
            feedback_post(
                "/mainnet-signup",
                false,
                "email=bot%40example.com&website=filled",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("on the list"));
        assert_eq!(store.count().await.unwrap(), 0);
        let (status, _, _) = send(
            &public,
            feedback_post(
                "/mainnet-signup",
                false,
                "email=a%40example.com&pow_challenge=invalid&pow_nonce=0",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        for email in [
            "+Player%2BLaunch%40Example.COM+",
            "player%2Blaunch%40example.com",
        ] {
            let proof = public_form_proof(&public, "/api/v1/mainnet-signup/challenge").await;
            let (status, headers, body) = send(
                &public,
                feedback_post(
                    "/api/v1/mainnet-signup",
                    true,
                    &format!("email={email}&{proof}"),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(headers["cache-control"]
                .to_str()
                .unwrap()
                .contains("no-store"));
            assert!(body.contains("on the list"));
            assert!(!body.contains("player"));
        }
        assert_eq!(store.count().await.unwrap(), 1);
        assert_eq!(
            store.list(100, 0).await.unwrap()[0].email,
            "player+launch@example.com"
        );
        // The plain HTML form works without a challenge, with a smaller rate allowance.
        let (status, _, body) = send(
            &public,
            feedback_post("/mainnet-signup", false, "email=nojs%40example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("on the list"));
        let (status, _, body) = send(
            &public,
            feedback_post("/mainnet-signup", false, "email=limited%40example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.contains("try again later"));
        assert_eq!(store.count().await.unwrap(), 2);
        let (status, _, _) = send(
            &public,
            feedback_post(
                "/mainnet-signup",
                false,
                &format!("email={}", "x".repeat(17000)),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

        let admin = test.admin(token_access(), Network::Regtest);
        for path in ["/admin/mainnet-signups", "/admin/mainnet-signups.csv"] {
            let (status, _, _) = send(&public, request("GET", path, &[], "")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, body) = send(&admin, request("GET", path, &[], "")).await;
            assert!(status.is_redirection() || status == StatusCode::UNAUTHORIZED);
            assert!(!body.contains("player+launch@example.com"));
            let (status, headers, body) = send(
                &admin,
                request("GET", path, &[("authorization", BEARER)], ""),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(headers["cache-control"]
                .to_str()
                .unwrap()
                .contains("no-store"));
            assert!(body.contains("player+launch@example.com"));
            assert!(body.contains("nojs@example.com"));
            if path.ends_with(".csv") {
                assert_eq!(headers["content-type"], "text/csv; charset=utf-8");
                assert!(body.starts_with("email,created_at\r\n"));
                assert_eq!(body.lines().count(), 3);
            }
        }
        test.stop().await;
    }

    #[tokio::test]
    async fn mainnet_signup_can_be_closed_without_removing_collected_emails() {
        let test =
            TestState::start_with(|settings| settings.mainnet_signup_settings.enabled = false)
                .await;
        let public = test.public();
        test.state
            .mainnet_signup
            .store
            .insert(
                "existing@example.com".into(),
                time::OffsetDateTime::now_utc(),
            )
            .await
            .unwrap();
        for (method, path) in [
            ("GET", "/mainnet-signup"),
            ("POST", "/mainnet-signup"),
            ("GET", "/api/v1/mainnet-signup/challenge"),
            ("POST", "/api/v1/mainnet-signup"),
        ] {
            let (status, _, _) = send(
                &public,
                request(
                    method,
                    path,
                    &[("content-type", FORM)],
                    "email=a%40example.com",
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        let (_, _, home) = send(&public, request("GET", "/", &[], "")).await;
        assert!(!home.contains("data-mainnet-signup-open"));
        let admin = test.admin(token_access(), Network::Regtest);
        let (status, _, body) = send(
            &admin,
            request(
                "GET",
                "/admin/mainnet-signups.csv",
                &[("authorization", BEARER)],
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("existing@example.com"));
        test.stop().await;
    }

    #[tokio::test]
    async fn feedback_is_off_by_default() {
        let test = TestState::start().await;
        let public = test.public();
        for (method, path) in [
            ("GET", "/api/v1/feedback/challenge"),
            ("POST", "/api/v1/feedback"),
            ("POST", "/feedback"),
            ("GET", "/feedback"),
        ] {
            let (status, _, _) = send(
                &public,
                request(method, path, &[("content-type", FORM)], "message=hi"),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
        }
        let (_, _, body) = send(&public, request("GET", "/", &[], "")).await;
        assert!(!body.contains("data-feedback-open"));
        test.stop().await;
    }

    #[tokio::test]
    async fn feedback_is_stored_once_with_the_servers_context_and_limited() {
        let test =
            TestState::start_with(|settings| settings.feedback_settings.enabled = true).await;
        let public = test.public();
        let store = test.state.feedback.store.clone();

        let (status, _, body) = send(&public, request("GET", "/", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("data-feedback-open"),
            "the footer links to it"
        );
        let (status, _, body) = send(&public, request("GET", "/feedback", &[], "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"action="/feedback""#));

        let proof = feedback_proof(&public).await;
        let (status, _, body) = send(
            &public,
            feedback_post(
                "/api/v1/feedback",
                true,
                &format!(
                    "message=Hello+there%0D%0Asecond%07&contact=me%40example.com&website=&{proof}"
                ),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("Your message reached the 5day4cast team."),
            "{body}"
        );
        assert!(!body.contains("<!DOCTYPE"));
        let stored = store.list(None, 50).await.unwrap();
        assert_eq!(stored.len(), 1);
        let row = &stored[0];
        assert_eq!(row.message, "Hello there\nsecond");
        assert_eq!(row.contact.as_deref(), Some("me@example.com"));
        assert_eq!(row.page.as_deref(), Some("/competitions"));
        assert_eq!(row.sid.as_deref(), Some(SESSION));
        assert!(row
            .rid
            .as_deref()
            .is_some_and(crate::api::request_context::valid_request_id));
        assert_eq!(row.ip.as_deref(), Some("0.0.0.0"));
        assert_eq!(row.user_agent.as_deref(), Some("test-agent/1.0"));
        assert_eq!(row.pubkey, None);
        assert_eq!(row.notified_at, None);

        // A spent proof is refused, and the form comes back with the text.
        let (status, _, body) = send(
            &public,
            feedback_post(
                "/api/v1/feedback",
                true,
                &format!("message=Another&{proof}"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Press Send again"), "{body}");
        assert!(body.contains(">Another</textarea>"), "{body}");

        // A repeat and a filled hidden field are thanked and dropped.
        for body in [
            "message=Hello+there%0Asecond".to_owned(),
            "message=Buy+now&website=https%3A%2F%2Fspam.example".to_owned(),
        ] {
            let proof = feedback_proof(&public).await;
            let (status, _, answer) = send(
                &public,
                feedback_post("/api/v1/feedback", true, &format!("{body}&{proof}")),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(answer.contains("reached the 5day4cast team"), "{answer}");
        }
        assert_eq!(store.list(None, 50).await.unwrap().len(), 1);

        // Without JavaScript: no proof, a whole page back.
        let (status, _, body) = send(
            &public,
            feedback_post("/feedback", false, "message=No+script+here&contact="),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<!DOCTYPE") && body.contains("reached the 5day4cast team"));
        assert_eq!(store.list(None, 50).await.unwrap().len(), 2);
        let (status, _, body) =
            send(&public, feedback_post("/feedback", false, "message=+%0A+")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("Write a message first."));

        // A tab sends three an hour: the first, the repeat and the page's one were counted.
        let proof = feedback_proof(&public).await;
        let (_, _, body) = send(
            &public,
            feedback_post("/api/v1/feedback", true, &format!("message=Four&{proof}")),
        )
        .await;
        assert!(body.contains("a lot of messages"), "{body}");
        assert_eq!(store.list(None, 50).await.unwrap().len(), 2);

        // The operator's pages list, open and update it.
        let admin = test.admin(token_access(), Network::Regtest);
        let bearer = [("authorization", BEARER)];
        let (status, _, body) = send(
            &admin,
            request("GET", "/admin/feedback/unread", &bearer, ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(">2</span>"), "{body}");
        let (status, _, body) = send(&admin, request("GET", "/admin/feedback", &bearer, "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Hello there second") && body.contains("No script here"),
            "{body}"
        );
        let detail = format!("/admin/feedback/{}", row.id);
        let (status, _, body) = send(&admin, request("GET", &detail, &bearer, "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Hello there\nsecond"), "{body}");
        assert!(body.contains("/admin/visitors?sid=Xq3vT9mPa1Lw0Zb8Yc7Rkd"));
        let (status, _, body) = send(
            &admin,
            request(
                "POST",
                &detail,
                &[("authorization", BEARER), ("content-type", FORM)],
                "status=done&note=Replied+by+email",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Saved."));
        let updated = store.get(row.id).await.unwrap().unwrap();
        assert_eq!(
            updated.status,
            crate::domain::feedback::FeedbackStatus::Done
        );
        assert_eq!(updated.operator_note.as_deref(), Some("Replied by email"));
        let (_, _, body) = send(
            &admin,
            request("GET", "/admin/feedback/unread", &bearer, ""),
        )
        .await;
        assert!(body.contains(">1</span>"), "{body}");
        test.stop().await;
    }

    #[tokio::test]
    async fn the_visitors_page_says_when_logs_are_not_configured() {
        let test = TestState::start().await;
        let admin = test.admin(token_access(), Network::Regtest);
        let bearer = [("authorization", BEARER)];
        let (status, _, body) = send(
            &admin,
            request("GET", "/admin/visitors?ip=203.0.113.7", &bearer, ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Visitor logs are not configured"), "{body}");
        let (status, _, body) = send(
            &admin,
            request("GET", "/admin/visitors/results?rid=bad%22rid", &bearer, ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("That is not a request id."), "{body}");
        let (_, _, body) = send(
            &admin,
            request("GET", "/admin/visitors/results?user=nobody", &bearer, ""),
        )
        .await;
        assert!(body.contains("No account has that username."), "{body}");
        let (_, _, body) = send(
            &admin,
            request("GET", "/admin/visitors/results?ip=203.0.113.7", &bearer, ""),
        )
        .await;
        assert!(body.contains("Visitor logs are not configured."), "{body}");
        test.stop().await;
    }

    /// A challenge from the public API, its bytes, and the first nonce solving it.
    async fn solved_pow(public: &Router) -> (serde_json::Value, Vec<u8>, u64) {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let (status, _, body) = send(public, request("POST", "/api/v1/users/pow", &[], "")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let issued: serde_json::Value = serde_json::from_str(&body).unwrap();
        let bytes = URL_SAFE_NO_PAD
            .decode(issued["challenge"].as_str().unwrap())
            .unwrap();
        let difficulty = issued["difficulty"].as_u64().unwrap();
        let nonce = (0u64..)
            .find(|nonce| solves_pow(&bytes, *nonce, difficulty))
            .unwrap();
        (issued, bytes, nonce)
    }

    /// `account` with the proof of work fields for `issued` and `nonce`.
    fn with_pow(
        mut account: serde_json::Value,
        issued: &serde_json::Value,
        nonce: u64,
    ) -> serde_json::Value {
        account["pow_challenge"] = issued["challenge"].clone();
        account["pow_nonce"] = serde_json::Value::String(nonce.to_string());
        account
    }

    fn extension_account(key: &str) -> serde_json::Value {
        serde_json::json!({
            "encrypted_bitcoin_private_key": key,
            "network": "regtest",
            "lightning_address": "player@mock-wallet.dev",
        })
    }

    /// A username sign-up whose Lightning Address is refused after the proof of work, so a test
    /// sees how far it got without waiting for Argon2.
    fn username_account(username: &str) -> serde_json::Value {
        serde_json::json!({
            "username": username,
            "auth_key": "ab".repeat(32),
            "encrypted_nsec": "sealed",
            "encrypted_bitcoin_private_key": format!("{username} key"),
            "network": "regtest",
            "lightning_address": "unknown@mock-wallet.dev",
        })
    }

    const NO_ADDRESS: &str = "has no Lightning Address unknown@mock-wallet.dev";

    #[tokio::test]
    async fn sign_ups_need_no_proof_of_work_while_it_is_off() {
        let test = TestState::start().await;
        let public = test.public();
        let (status, _, body) = send(&public, request("POST", "/api/v1/users/pow", &[], "")).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

        let (status, _, body) = send(
            &public,
            signed_post(
                &test,
                &nostr::Keys::generate(),
                "/api/v1/users/register",
                &extension_account("alice key"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // The username sign-up gets past the proof of work to the address check.
        let (status, _, body) = send(
            &public,
            signed_post(
                &test,
                &nostr::Keys::generate(),
                "/api/v1/users/username/register",
                &username_account("bob"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains(NO_ADDRESS), "{body}");
        test.stop().await;
    }

    #[tokio::test]
    async fn every_sign_up_spends_a_proof_of_work_when_it_is_on() {
        use nostr::ToBech32;
        let test = TestState::start_with(|settings| {
            settings.pow_settings = crate::config::PowSettings {
                enabled: true,
                base_bits: 4,
                max_bits: 8,
                step_signups: 1,
            };
        })
        .await;
        let public = test.public();
        // Two accounts created in the last hour add two bits.
        for i in 0..2 {
            test.state
                .users_info
                .register(
                    format!("earlier {i}"),
                    crate::api::routes::RegisterPayload {
                        encrypted_bitcoin_private_key: format!("earlier key {i}"),
                        network: "regtest".into(),
                        lightning_address: "player@mock-wallet.dev".into(),
                    },
                )
                .await
                .unwrap();
        }
        let (issued, bytes, nonce) = solved_pow(&public).await;
        assert_eq!(issued["difficulty"], 6);
        assert!(issued["expires_at"].as_u64().unwrap() > unix_now());

        // Without a proof, neither kind of sign-up creates an account.
        let alice = nostr::Keys::generate();
        for (path, account) in [
            ("/api/v1/users/register", extension_account("alice key")),
            ("/api/v1/users/username/register", username_account("alice")),
        ] {
            let (status, _, body) =
                send(&public, signed_post(&test, &alice, path, &account).await).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["code"], "pow_rejected", "{path}");
            assert_eq!(
                body["error"],
                "Signing up needs a proof of work from this page; reload it and try again"
            );
        }
        let alice_npub = alice.public_key().to_bech32().unwrap();
        assert!(test
            .state
            .users_info
            .login(alice_npub.clone())
            .await
            .is_err());

        // A wrong nonce is refused without spending the challenge.
        let wrong = (nonce + 1..)
            .find(|nonce| !solves_pow(&bytes, *nonce, 6))
            .unwrap();
        let (status, _, body) = send(
            &public,
            signed_post(
                &test,
                &alice,
                "/api/v1/users/register",
                &with_pow(extension_account("alice key"), &issued, wrong),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("does not solve its challenge"), "{body}");

        // A solved one creates one account.
        let (status, _, body) = send(
            &public,
            signed_post(
                &test,
                &alice,
                "/api/v1/users/register",
                &with_pow(extension_account("alice key"), &issued, nonce),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(test.state.users_info.login(alice_npub).await.is_ok());
        let (status, _, body) = send(
            &public,
            signed_post(
                &test,
                &nostr::Keys::generate(),
                "/api/v1/users/register",
                &with_pow(extension_account("carol key"), &issued, nonce),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("already used"), "{body}");

        // The username sign-up takes a fresh proof to its address check, and spends it.
        let (issued, _, nonce) = solved_pow(&public).await;
        let bob = nostr::Keys::generate();
        for refused in [NO_ADDRESS, "already used"] {
            let (status, _, body) = send(
                &public,
                signed_post(
                    &test,
                    &bob,
                    "/api/v1/users/username/register",
                    &with_pow(username_account("bob"), &issued, nonce),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains(refused), "{body}");
        }
        test.stop().await;
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
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
        escrow_expiry_margin_secs: settings.escrow_expiry_margin_secs,
        max_refund_fee_sats: settings.max_refund_fee_sats,
    }))
}
