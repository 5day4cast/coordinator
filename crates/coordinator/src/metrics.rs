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
    Encoder, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, task::JoinHandle};

use crate::domain::{CompetitionStore, TicketStatus};

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

/// Competition steps that returned an error; the runner retries them with backoff.
pub static COMPETITION_STEP_FAILURES: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "coordinator_competition_step_failures_total",
        "Competition lifecycle steps that failed and will be retried",
    )
    .expect("valid metric")
});

/// Record a payout reaching its final result for the first time.
pub fn record_payout_result(succeeded: bool) {
    PAYOUT_ATTEMPTS
        .with_label_values(&[if succeeded { "succeeded" } else { "failed" }])
        .inc();
}

/// The coordinator's metrics and what they are computed from.
pub struct Metrics {
    registry: Registry,
    store: Arc<CompetitionStore>,
    background_threads: Arc<HashMap<String, JoinHandle<()>>>,
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
                "Automatic payout jobs that gave up",
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
            .register(Box::new(COMPETITION_STEP_FAILURES.clone()))?;
        // Show both results from the start, so a rate over them is defined.
        for result in ["succeeded", "failed"] {
            PAYOUT_ATTEMPTS.with_label_values(&[result]);
        }
        Ok(metrics)
    }

    /// Render every metric in the Prometheus text format.
    pub async fn render(&self) -> String {
        self.refresh_threads();
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
            "coordinator_competition_step_failures_total",
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
        ] {
            assert!(body.contains(sample), "missing {sample} in:\n{body}");
        }
        assert!(body.contains(&format!(
            "coordinator_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
        database.close().await.unwrap();
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
