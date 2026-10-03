//! Prometheus metrics, served by an optional listener of their own.
//!
//! The listener answers only `GET /metrics`; it is never merged into the public router.
//! Gauges derived from the database are refreshed at scrape time at most every
//! [`DB_REFRESH_INTERVAL`], so frequent scrapes do not load SQLite. Counters for in-process
//! events are process-wide statics, incremented where the events happen.

use axum::{
    extract::State,
    http::{header::CONTENT_TYPE, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use log::warn;
use prometheus::{
    Encoder, Gauge, GaugeVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, task::JoinHandle};

use crate::{
    domain::{ArkadeHealth, CompetitionStore, TicketStatus},
    infra::{ark_swap::SwapWallet, lightning::PAYMENT_FAILURE_REASONS},
};

/// How long database-derived gauges are reused between scrapes.
pub const DB_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// Every name `CompetitionStatus::state_name` returns, so each state reports 0 when empty.
const COMPETITION_STATES: [&str; 20] = [
    "created",
    "collecting_entries",
    "awaiting_escrow",
    "escrow_confirmed",
    "event_created",
    "entries_submitted",
    "contract_created",
    "awaiting_signatures",
    "signing_complete",
    "funding_broadcasted",
    "funding_confirmed",
    "funding_settled",
    "awaiting_attestation",
    "attested",
    "expiry_broadcasted",
    "outcome_broadcasted",
    "delta_broadcasted",
    "completed",
    "failed",
    "cancelled",
];

const TICKET_STATUSES: [TicketStatus; 7] = [
    TicketStatus::Created,
    TicketStatus::Reserved,
    TicketStatus::Paid,
    TicketStatus::Settled,
    TicketStatus::Used,
    TicketStatus::Expired,
    TicketStatus::Cancelled,
];

fn ticket_status_label(status: &TicketStatus) -> &'static str {
    match status {
        TicketStatus::Created => "created",
        TicketStatus::Reserved => "reserved",
        TicketStatus::Paid => "paid",
        TicketStatus::Settled => "settled",
        TicketStatus::Used => "used",
        TicketStatus::Expired => "expired",
        TicketStatus::Cancelled => "cancelled",
    }
}

/// Lightning payout attempts by the result recorded for them. Each payout row counts
/// once, when it first becomes succeeded or failed.
pub static PAYOUT_ATTEMPTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "coordinator_payout_attempts_total",
            "Lightning payout attempts by recorded result",
        ),
        &["result"],
    )
    .expect("valid metric")
});

/// Sends of a payout invoice that LND failed, by its failure reason. One payout counts once
/// for every send that failed, so a payout waiting for a route counts again at each retry.
pub static PAYOUT_SEND_FAILURES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "coordinator_payout_send_failures_total",
            "Failed sends of a Lightning payout invoice by LND failure reason",
        ),
        &["reason"],
    )
    .expect("valid metric")
});

/// Competition steps that returned an error; the runner retries them with backoff.
pub static COMPETITION_STEP_FAILURES: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "coordinator_competition_step_failures_total",
        "Competition lifecycle steps that failed and will be retried",
    )
    .expect("valid metric")
});

/// Whether entries to Arkade competitions are paused because the Arkade server is failing
/// batch steps (1) or not (0). Set where the pause is decided (`ArkadeHealth`).
pub static ARKADE_UNAVAILABLE: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "coordinator_arkade_unavailable",
        "Whether entries are paused because the Arkade server is failing batch steps",
    )
    .expect("valid metric")
});

/// Whether the LND invoice subscription is connected (1) or not (0). While it is not, the
/// invoice watcher polls at its fallback interval. Set by `SubscriptionHealth`.
pub static LN_INVOICE_SUBSCRIPTION_UP: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "coordinator_ln_invoice_subscription_up",
        "Whether the LND invoice subscription is connected",
    )
    .expect("valid metric")
});

/// Whether the LND payment subscription is connected (1) or not (0). While it is not, the
/// payout watcher polls at its fallback interval. Set by `SubscriptionHealth`.
pub static LN_PAYMENT_SUBSCRIPTION_UP: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "coordinator_ln_payment_subscription_up",
        "Whether the LND payment subscription is connected",
    )
    .expect("valid metric")
});

/// Whether the Arkade subscription watching pending escrows is open (1) or not (0).
pub static ESCROW_SUBSCRIPTION_UP: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "coordinator_escrow_subscription_up",
        "Whether the Arkade subscription watching pending escrows is open",
    )
    .expect("valid metric")
});

/// Transactions the Arkade escrow subscription reported.
pub static ESCROW_EVENTS: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "coordinator_escrow_events_total",
        "Transactions the Arkade escrow subscription reported",
    )
    .expect("valid metric")
});

/// ark-swapd's Ark wallet by balance, in sats, as the coordinator last read it: what may pay an
/// escrow (`payable`), what waits at the boarding address for a batch (`boarding`), what is too
/// close to expiry to pay one (`expiring`), and what a batch must recover (`recoverable`). NaN
/// until a read reports it, so a missing observation never reads as an empty wallet.
pub static ARK_WALLET_SAT: LazyLock<GaugeVec> = LazyLock::new(|| {
    let gauge = GaugeVec::new(
        Opts::new(
            "coordinator_ark_wallet_sat",
            "ark-swapd's Ark wallet by balance, in sats, at the last read",
        ),
        &["balance"],
    )
    .expect("valid metric");
    for (balance, _) in ark_wallet_balances(&SwapWallet::default()) {
        gauge.with_label_values(&[balance]).set(f64::NAN);
    }
    gauge
});

/// When the Ark wallet's first spendable VTXO expires, in UNIX seconds; NaN when none.
pub static ARK_WALLET_EARLIEST_EXPIRY: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_ark_wallet_earliest_expiry_timestamp_seconds",
        "When the Ark wallet's first spendable VTXO expires",
    ))
});

/// When a batch last took one of ark-swapd's boards or renewals, in UNIX seconds.
pub static ARK_WALLET_LAST_BOARD_SUCCESS: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_ark_wallet_last_board_success_timestamp_seconds",
        "When a batch last took one of ark-swapd's boards or renewals",
    ))
});

/// When the Arkade server last failed one of ark-swapd's boards or renewals, in UNIX seconds.
pub static ARK_WALLET_LAST_BOARD_FAILURE: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_ark_wallet_last_board_failure_timestamp_seconds",
        "When the Arkade server last failed one of ark-swapd's boards or renewals",
    ))
});

/// When the coordinator last read ark-swapd's wallet, in UNIX seconds, so a stale reading shows.
pub static ARK_WALLET_READ: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_ark_wallet_read_timestamp_seconds",
        "When the coordinator last read ark-swapd's wallet",
    ))
});

/// The balances `coordinator_ark_wallet_sat` reports, each with its value from `wallet`.
fn ark_wallet_balances(wallet: &SwapWallet) -> [(&'static str, f64); 4] {
    let sat = |value: Option<u64>| value.map_or(f64::NAN, |sat| sat as f64);
    [
        ("payable", sat(wallet.payable_sat)),
        ("boarding", sat(wallet.boarding_sat)),
        ("expiring", sat(wallet.expiring_sat)),
        ("recoverable", sat(wallet.recoverable_sat)),
    ]
}

/// A gauge that reads NaN until it is first set.
fn unknown(gauge: prometheus::Result<Gauge>) -> Gauge {
    let gauge = gauge.expect("valid metric");
    gauge.set(f64::NAN);
    gauge
}

fn timestamp(seconds: Option<i64>) -> f64 {
    seconds.map_or(f64::NAN, |at| at as f64)
}

/// Record what a read of ark-swapd's wallet reported. A field an older ark-swapd leaves out
/// reads NaN.
pub fn record_ark_wallet(wallet: &SwapWallet, read_at: time::OffsetDateTime) {
    for (balance, value) in ark_wallet_balances(wallet) {
        ARK_WALLET_SAT.with_label_values(&[balance]).set(value);
    }
    ARK_WALLET_EARLIEST_EXPIRY.set(timestamp(wallet.earliest_expiry));
    ARK_WALLET_LAST_BOARD_SUCCESS.set(timestamp(wallet.last_board_success_at));
    ARK_WALLET_LAST_BOARD_FAILURE.set(timestamp(
        wallet.last_board_failure.as_ref().map(|failure| failure.at),
    ));
    ARK_WALLET_READ.set(read_at.unix_timestamp() as f64);
}

/// Record a payout reaching its final result for the first time.
pub fn record_payout_result(succeeded: bool) {
    PAYOUT_ATTEMPTS
        .with_label_values(&[if succeeded { "succeeded" } else { "failed" }])
        .inc();
}

/// Record a failed send of a payout invoice. `reason` comes from
/// [`classify_payment_failure`](crate::infra::lightning::classify_payment_failure).
pub fn record_payout_send_failure(reason: &'static str) {
    PAYOUT_SEND_FAILURES.with_label_values(&[reason]).inc();
}

/// The coordinator's metrics and what they are computed from.
pub struct Metrics {
    registry: Registry,
    store: Arc<CompetitionStore>,
    background_threads: Arc<HashMap<String, JoinHandle<()>>>,
    /// Decided again at each scrape, so a pause that lapsed with nothing running reads 0.
    arkade: Option<Arc<ArkadeHealth>>,
    last_refresh: Mutex<Option<Instant>>,
    competitions: IntGaugeVec,
    entries: IntGaugeVec,
    tickets: IntGaugeVec,
    payouts: IntGaugeVec,
    payout_jobs_open: IntGauge,
    payout_jobs_failed: IntGauge,
    payout_jobs_retrying: IntGauge,
    payout_job_oldest_open_age: IntGauge,
    background_thread_up: IntGaugeVec,
}

impl Metrics {
    pub fn new(
        store: Arc<CompetitionStore>,
        background_threads: Arc<HashMap<String, JoinHandle<()>>>,
    ) -> Result<Self, prometheus::Error> {
        let registry = Registry::new();
        let gauge_vec = |name: &str, help: &str, label: &str| {
            let gauge = IntGaugeVec::new(Opts::new(name, help), &[label])?;
            registry.register(Box::new(gauge.clone()))?;
            Ok::<_, prometheus::Error>(gauge)
        };
        let gauge = |name: &str, help: &str| {
            let gauge = IntGauge::new(name, help)?;
            registry.register(Box::new(gauge.clone()))?;
            Ok::<_, prometheus::Error>(gauge)
        };

        let metrics = Self {
            competitions: gauge_vec(
                "coordinator_competitions",
                "Competitions by lifecycle state",
                "state",
            )?,
            entries: gauge_vec(
                "coordinator_entries",
                "Entries with a paid ticket (paid) and entries whose owner signed (signed)",
                "status",
            )?,
            tickets: gauge_vec("coordinator_tickets", "Tickets by status", "status")?,
            payouts: gauge_vec(
                "coordinator_payouts",
                "Lightning payouts by status",
                "status",
            )?,
            payout_jobs_open: gauge(
                "coordinator_payout_jobs_open",
                "Automatic payout jobs neither completed nor failed",
            )?,
            payout_jobs_failed: gauge(
                "coordinator_payout_jobs_failed",
                "Retained failed automatic payout job records, including replaced jobs; not a count of unpaid entries",
            )?,
            payout_jobs_retrying: gauge(
                "coordinator_payout_jobs_retrying",
                "Open automatic payout jobs that failed at least once and will retry",
            )?,
            payout_job_oldest_open_age: gauge(
                "coordinator_payout_job_oldest_open_age_seconds",
                "Age of the oldest open automatic payout job, 0 when none is open",
            )?,
            background_thread_up: gauge_vec(
                "coordinator_background_thread_up",
                "Whether a background worker is running (1) or has stopped (0)",
                "thread",
            )?,
            registry,
            store,
            background_threads,
            arkade: None,
            last_refresh: Mutex::new(None),
        };

        let build_info = IntGaugeVec::new(
            Opts::new("coordinator_build_info", "Coordinator build information"),
            &["version"],
        )?;
        build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);
        metrics.registry.register(Box::new(build_info))?;
        metrics
            .registry
            .register(Box::new(PAYOUT_ATTEMPTS.clone()))?;
        metrics
            .registry
            .register(Box::new(PAYOUT_SEND_FAILURES.clone()))?;
        metrics
            .registry
            .register(Box::new(COMPETITION_STEP_FAILURES.clone()))?;
        metrics
            .registry
            .register(Box::new(ARKADE_UNAVAILABLE.clone()))?;
        metrics
            .registry
            .register(Box::new(LN_INVOICE_SUBSCRIPTION_UP.clone()))?;
        metrics
            .registry
            .register(Box::new(LN_PAYMENT_SUBSCRIPTION_UP.clone()))?;
        metrics
            .registry
            .register(Box::new(ESCROW_SUBSCRIPTION_UP.clone()))?;
        metrics.registry.register(Box::new(ESCROW_EVENTS.clone()))?;
        metrics
            .registry
            .register(Box::new(ARK_WALLET_SAT.clone()))?;
        for gauge in [
            &*ARK_WALLET_EARLIEST_EXPIRY,
            &*ARK_WALLET_LAST_BOARD_SUCCESS,
            &*ARK_WALLET_LAST_BOARD_FAILURE,
            &*ARK_WALLET_READ,
        ] {
            metrics.registry.register(Box::new(gauge.clone()))?;
        }
        // Show both results from the start, so a rate over them is defined.
        for result in ["succeeded", "failed"] {
            PAYOUT_ATTEMPTS.with_label_values(&[result]);
        }
        for reason in PAYMENT_FAILURE_REASONS {
            PAYOUT_SEND_FAILURES.with_label_values(&[reason]);
        }
        Ok(metrics)
    }

    pub fn with_arkade_health(mut self, arkade: Arc<ArkadeHealth>) -> Self {
        self.arkade = Some(arkade);
        self
    }

    /// Render every metric in the Prometheus text format.
    pub async fn render(&self) -> String {
        self.refresh_threads();
        if let Some(arkade) = &self.arkade {
            arkade.unavailable(time::OffsetDateTime::now_utc());
        }
        self.refresh_database().await;
        let mut buffer = Vec::new();
        if let Err(error) = TextEncoder::new().encode(&self.registry.gather(), &mut buffer) {
            warn!("Cannot encode metrics: {error}");
        }
        String::from_utf8(buffer).unwrap_or_default()
    }

    fn refresh_threads(&self) {
        for (name, handle) in self.background_threads.iter() {
            self.background_thread_up
                .with_label_values(&[name])
                .set(i64::from(!handle.is_finished()));
        }
    }

    /// Recompute the database gauges unless they are fresher than the refresh interval.
    /// Concurrent scrapes wait for one refresh instead of each querying the database.
    async fn refresh_database(&self) {
        let mut last_refresh = self.last_refresh.lock().await;
        if last_refresh.is_some_and(|at| at.elapsed() < DB_REFRESH_INTERVAL) {
            return;
        }
        // On an error the previous values stay, and the next scrape tries again.
        match self.store.competition_state_counts().await {
            Ok(counts) => {
                for state in COMPETITION_STATES {
                    self.competitions.with_label_values(&[state]).set(0);
                }
                for (state, count) in counts {
                    self.competitions.with_label_values(&[state]).set(count);
                }
            }
            Err(error) => {
                warn!("Cannot count competitions for metrics: {error}");
                return;
            }
        }
        let counts = match self.store.store_counts().await {
            Ok(counts) => counts,
            Err(error) => {
                warn!("Cannot count entries and payouts for metrics: {error}");
                return;
            }
        };
        self.entries
            .with_label_values(&["paid"])
            .set(counts.entries_paid);
        self.entries
            .with_label_values(&["signed"])
            .set(counts.entries_signed);
        for status in &TICKET_STATUSES {
            self.tickets
                .with_label_values(&[ticket_status_label(status)])
                .set(0);
        }
        for (status, count) in &counts.tickets {
            self.tickets
                .with_label_values(&[ticket_status_label(status)])
                .set(*count);
        }
        self.payouts
            .with_label_values(&["pending"])
            .set(counts.payouts_pending);
        self.payouts
            .with_label_values(&["succeeded"])
            .set(counts.payouts_succeeded);
        self.payouts
            .with_label_values(&["failed"])
            .set(counts.payouts_failed);
        self.payout_jobs_open.set(counts.payout_jobs_open);
        self.payout_jobs_failed.set(counts.payout_jobs_failed);
        self.payout_jobs_retrying.set(counts.payout_jobs_retrying);
        self.payout_job_oldest_open_age
            .set(counts.oldest_open_payout_job_age_secs);
        *last_refresh = Some(Instant::now());
    }
}

/// The metrics listener's router: `GET /metrics`, and 404 for every other path.
pub fn metrics_app(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(serve_metrics))
        .with_state(metrics)
}

async fn serve_metrics(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        metrics.render().await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    async fn metrics(directory: &tempfile::TempDir) -> (Arc<Metrics>, DBConnection) {
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let store = Arc::new(CompetitionStore::new(database.clone()));
        let running = tokio::spawn(std::future::pending::<()>());
        let finished = tokio::spawn(async {});
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        let threads = HashMap::from([
            ("running_worker".to_string(), running),
            ("stopped_worker".to_string(), finished),
        ]);
        let metrics = Metrics::new(store, Arc::new(threads)).unwrap();
        (Arc::new(metrics), database)
    }

    async fn get(app: Router, path: &str) -> (StatusCode, String) {
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn renders_every_metric_family() {
        let directory = tempfile::tempdir().unwrap();
        let (metrics, database) = metrics(&directory).await;
        COMPETITION_STEP_FAILURES.inc();
        record_payout_result(true);

        let (status, body) = get(metrics_app(metrics), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        for family in [
            "coordinator_competitions",
            "coordinator_entries",
            "coordinator_tickets",
            "coordinator_payouts",
            "coordinator_payout_jobs_open",
            "coordinator_payout_jobs_failed",
            "coordinator_payout_jobs_retrying",
            "coordinator_payout_job_oldest_open_age_seconds",
            "coordinator_background_thread_up",
            "coordinator_payout_attempts_total",
            "coordinator_payout_send_failures_total",
            "coordinator_competition_step_failures_total",
            "coordinator_arkade_unavailable",
            "coordinator_ark_wallet_sat",
            "coordinator_ark_wallet_earliest_expiry_timestamp_seconds",
            "coordinator_ark_wallet_last_board_success_timestamp_seconds",
            "coordinator_ark_wallet_last_board_failure_timestamp_seconds",
            "coordinator_ark_wallet_read_timestamp_seconds",
            "coordinator_build_info",
        ] {
            assert!(
                body.contains(&format!("# TYPE {family} ")),
                "missing {family} in:\n{body}"
            );
        }
        for sample in [
            "coordinator_competitions{state=\"collecting_entries\"} 0",
            "coordinator_tickets{status=\"expired\"} 0",
            "coordinator_payouts{status=\"failed\"} 0",
            "coordinator_payout_job_oldest_open_age_seconds 0",
            "coordinator_background_thread_up{thread=\"running_worker\"} 1",
            "coordinator_background_thread_up{thread=\"stopped_worker\"} 0",
            "coordinator_payout_attempts_total{result=\"failed\"}",
            "coordinator_payout_send_failures_total{reason=\"FAILURE_REASON_NO_ROUTE\"}",
            "coordinator_payout_send_failures_total{reason=\"other\"}",
        ] {
            assert!(body.contains(sample), "missing {sample} in:\n{body}");
        }
        assert!(body.contains(&format!(
            "coordinator_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
        database.close().await.unwrap();
    }

    /// Each balance ark-swapd reports is exported as it is; one an older ark-swapd leaves out is
    /// NaN, never zero.
    #[test]
    fn ark_wallet_balances_keep_missing_ones_unknown() {
        let wallet: SwapWallet = serde_json::from_str(
            r#"{"boarding_address":"tb1p","payable_sat":142572,"expiring_sat":0,"boarding_sat":0,
                "earliest_expiry":1791344880,"last_board_success_at":1791022847}"#,
        )
        .unwrap();
        let balances = ark_wallet_balances(&wallet);
        assert_eq!(balances[0], ("payable", 142_572.0));
        assert_eq!(balances[1], ("boarding", 0.0));
        assert_eq!(balances[2], ("expiring", 0.0));
        assert_eq!(balances[3].0, "recoverable");
        assert!(balances[3].1.is_nan(), "not reported");
        assert_eq!(timestamp(wallet.earliest_expiry), 1_791_344_880.0);
        assert!(timestamp(None).is_nan());
    }

    #[tokio::test]
    async fn serves_nothing_but_metrics() {
        let directory = tempfile::tempdir().unwrap();
        let (metrics, database) = metrics(&directory).await;
        for path in [
            "/",
            "/health_check",
            "/api/v1/competitions",
            "/metrics/extra",
        ] {
            let (status, _) = get(metrics_app(metrics.clone()), path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
        database.close().await.unwrap();
    }
}
