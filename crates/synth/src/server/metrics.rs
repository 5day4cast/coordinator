use axum::{response::IntoResponse, routing::get, Router};
use prometheus::{Encoder, TextEncoder, register_counter_vec, register_gauge, register_histogram_vec};

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
        "Whether the last full lifecycle test passed (1=healthy, 0=unhealthy)"
    ).unwrap();

    pub static ref LAST_SUCCESS: prometheus::Gauge = register_gauge!(
        "synth_last_successful_run_timestamp",
        "Unix timestamp of the last successful run"
    ).unwrap();
}

pub fn router() -> Router {
    Router::new().route("/metrics", get(metrics_handler))
}

async fn metrics_handler() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    (
        [("content-type", "text/plain; charset=utf-8")],
        String::from_utf8(buffer).unwrap(),
    )
}

/// Record metrics for a completed scenario run
pub fn record_scenario(
    scenario: &str,
    passed: bool,
    duration_ms: i64,
    steps: &[(String, i64)], // (step_name, duration_ms)
) {
    let status = if passed { "passed" } else { "failed" };
    SCENARIO_RUNS
        .with_label_values(&[scenario, status])
        .inc();
    SCENARIO_DURATION
        .with_label_values(&[scenario])
        .observe(duration_ms as f64 / 1000.0);

    for (step_name, step_duration) in steps {
        STEP_DURATION
            .with_label_values(&[scenario, step_name])
            .observe(*step_duration as f64 / 1000.0);
    }

    if scenario == "full_lifecycle" {
        LIFECYCLE_HEALTHY.set(if passed { 1.0 } else { 0.0 });
        if passed {
            LAST_SUCCESS.set(time::OffsetDateTime::now_utc().unix_timestamp() as f64);
        }
    }
}
