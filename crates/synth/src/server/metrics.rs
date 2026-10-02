use crate::db::{SynthDb, TestRun};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use prometheus::{
    register_counter, register_counter_vec, register_gauge, register_histogram_vec, Encoder,
    TextEncoder,
};

lazy_static::lazy_static! {
    pub static ref SCENARIO_RUNS: prometheus::CounterVec = register_counter_vec!(
        "synth_scenario_runs_total",
        "Total scenario runs by scenario and status",
        &["scenario", "status"]
    ).unwrap();

    pub static ref SCENARIO_DURATION: prometheus::HistogramVec = register_histogram_vec!(
        "synth_scenario_duration_seconds",
        "Scenario execution duration in seconds",
        &["scenario"],
        vec![1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0]
    ).unwrap();

    pub static ref STEP_DURATION: prometheus::HistogramVec = register_histogram_vec!(
        "synth_step_duration_seconds",
        "Step execution duration in seconds",
        &["scenario", "step"],
        vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]
    ).unwrap();

    pub static ref LIFECYCLE_HEALTHY: prometheus::Gauge = register_gauge!(
        "synth_competition_lifecycle_healthy",
        "Latest assessed full lifecycle outcome (1=paid out, 0=failed, NaN=unverified or absent)"
    ).unwrap();

    pub static ref LAST_SUCCESS: prometheus::Gauge = register_gauge!(
        "synth_last_successful_run_timestamp",
        "Unix timestamp when full lifecycle payouts were last verified"
    ).unwrap();

    pub static ref OPEN_COMPETITIONS: prometheus::Gauge = register_gauge!(
        "synth_open_competitions",
        "Listed competitions taking entries, not full, whose entries close far enough ahead"
    ).unwrap();

    pub static ref OPEN_COMPETITION_MINUTES_LEFT: prometheus::Gauge = register_gauge!(
        "synth_open_competition_minutes_left",
        "Minutes until entries close for the open competition with the most time left (0 when none)"
    ).unwrap();

    pub static ref KEEP_OPEN_STARTS: prometheus::Counter = register_counter!(
        "synth_keep_open_starts_total",
        "Runs started early because no competition was open for visitors"
    ).unwrap();

    pub static ref BACKFILL_PLAYERS: prometheus::Counter = register_counter!(
        "synth_backfill_players_total",
        "Players synth entered late so a competition reaches its minimum"
    ).unwrap();
}

pub fn router(db: SynthDb) -> Router {
    initialize();
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(db)
}

fn initialize() {
    lazy_static::initialize(&SCENARIO_RUNS);
    lazy_static::initialize(&SCENARIO_DURATION);
    lazy_static::initialize(&STEP_DURATION);
    lazy_static::initialize(&LIFECYCLE_HEALTHY);
    lazy_static::initialize(&LAST_SUCCESS);
    lazy_static::initialize(&OPEN_COMPETITIONS);
    lazy_static::initialize(&OPEN_COMPETITION_MINUTES_LEFT);
    lazy_static::initialize(&KEEP_OPEN_STARTS);
    lazy_static::initialize(&BACKFILL_PLAYERS);
    for scenario in crate::runner::SCENARIOS {
        for status in ["passed", "failed"] {
            SCENARIO_RUNS.with_label_values(&[scenario, status]);
        }
        SCENARIO_DURATION.with_label_values(&[scenario]);
    }
    LIFECYCLE_HEALTHY.set(f64::NAN);
    LAST_SUCCESS.set(f64::NAN);
}

fn lifecycle_health(run: Option<&TestRun>) -> f64 {
    match run {
        Some(run) if matches!(run.status.as_str(), "failed" | "interrupted") => 0.0,
        Some(run) if matches!(run.money.as_deref(), Some("stuck" | "written_off")) => 0.0,
        Some(run) if run.status == "passed" && run.money.as_deref() == Some("paid_out") => 1.0,
        _ => f64::NAN,
    }
}

async fn metrics_handler(State(db): State<SynthDb>) -> Response {
    let (latest, last_success, observed_at) = match db.lifecycle_metrics().await {
        Ok(evidence) => evidence,
        Err(error) => {
            log::warn!("Cannot read lifecycle metrics: {error:#}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Lifecycle metrics unavailable",
            )
                .into_response();
        }
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    // The slowest normal tracker cadence is fifteen minutes. Do not carry an old
    // healthy result through a stopped tracker or an idle installation indefinitely.
    let fresh = observed_at.is_some_and(|at| (0..=30 * 60).contains(&now.saturating_sub(at)));
    LIFECYCLE_HEALTHY.set(if fresh {
        lifecycle_health(latest.as_ref())
    } else {
        f64::NAN
    });
    LAST_SUCCESS.set(last_success.map_or(f64::NAN, |at| at as f64));
    let mut buffer = Vec::new();
    if let Err(error) = TextEncoder::new().encode(&prometheus::gather(), &mut buffer) {
        log::warn!("Cannot encode metrics: {error}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    (
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        buffer,
    )
        .into_response()
}

/// Record metrics for a completed scenario run
pub fn record_scenario(
    scenario: &str,
    passed: bool,
    duration_ms: i64,
    steps: &[(String, i64)], // (step_name, duration_ms)
) {
    let status = if passed { "passed" } else { "failed" };
    SCENARIO_RUNS.with_label_values(&[scenario, status]).inc();
    SCENARIO_DURATION
        .with_label_values(&[scenario])
        .observe(duration_ms as f64 / 1000.0);

    for (step_name, step_duration) in steps {
        STEP_DURATION
            .with_label_values(&[scenario, step_name])
            .observe(*step_duration as f64 / 1000.0);
    }
}

/// Record the competitions open for visitors at a keep-open check: how many, and the most
/// minutes left before one's entries close.
pub fn record_open(open: usize, minutes_left: i64) {
    OPEN_COMPETITIONS.set(open as f64);
    OPEN_COMPETITION_MINUTES_LEFT.set(minutes_left.max(0) as f64);
}

/// Record a run started early to keep a competition open.
pub fn record_keep_open_start() {
    KEEP_OPEN_STARTS.inc();
}

/// Record players entered by a backfill.
pub fn record_backfill(players: usize) {
    BACKFILL_PLAYERS.inc_by(players as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_passing_is_not_lifecycle_success() {
        let mut run = TestRun {
            id: "r".into(),
            scenario: "full_lifecycle".into(),
            status: "passed".into(),
            started_at: "2026-10-01T00:00:00Z".into(),
            completed_at: None,
            error_message: None,
            config_json: None,
            competition_id: None,
            money: Some("following".into()),
        };
        assert!(lifecycle_health(Some(&run)).is_nan());
        run.money = Some("paid_out".into());
        assert_eq!(lifecycle_health(Some(&run)), 1.0);
        run.money = Some("stuck".into());
        assert_eq!(lifecycle_health(Some(&run)), 0.0);
        run.money = Some("unverified".into());
        assert!(lifecycle_health(Some(&run)).is_nan());
        assert!(lifecycle_health(None).is_nan());
    }
}
