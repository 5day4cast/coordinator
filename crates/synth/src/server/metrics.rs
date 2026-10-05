use crate::db::{LifecycleEvidence, SynthDb};
use crate::runner::keep_open::Forms;
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
        "Latest verdict on the runs meant to pay out, full lifecycle or queued (0=one failed and none has passed or paid out since, or the newest money verdict is stuck; 1=otherwise, once a payout was verified; NaN=no payout verified yet or no fresh evidence)"
    ).unwrap();

    pub static ref LAST_SUCCESS: prometheus::Gauge = register_gauge!(
        "synth_last_successful_run_timestamp",
        "Unix timestamp when a run's payouts, full lifecycle or queued, were last verified"
    ).unwrap();

    pub static ref OPEN_COMPETITIONS: prometheus::Gauge = register_gauge!(
        "synth_open_competitions",
        "Listed competitions taking entries, not full, whose entries close far enough ahead"
    ).unwrap();

    pub static ref OPEN_COMPETITION_MINUTES_LEFT: prometheus::Gauge = register_gauge!(
        "synth_open_competition_minutes_left",
        "Minutes until entries close for the open competition with the most time left (0 when none)"
    ).unwrap();

    pub static ref OPEN_COMPETITION_ENTERABLE: prometheus::Gauge = register_gauge!(
        "synth_open_competition_enterable",
        "Whether a visitor can make every pick on the entry form of each competition open for visitors (1=yes, 0=a form has lines without a forecast or shows no picks, NaN=none open or not loaded yet)"
    ).unwrap();

    pub static ref ENTRY_FORM_MISSING_FORECASTS: prometheus::Gauge = register_gauge!(
        "synth_entry_form_missing_forecasts",
        "Forecast lines without a forecast on the entry forms of the competitions open for visitors, when last loaded"
    ).unwrap();

    pub static ref KEEP_OPEN_STARTS: prometheus::Counter = register_counter!(
        "synth_keep_open_starts_total",
        "Runs started early to keep a competition open for visitors"
    ).unwrap();

    pub static ref BACKFILL_PLAYERS: prometheus::Counter = register_counter!(
        "synth_backfill_players_total",
        "Players synth entered late so a competition reaches its minimum"
    ).unwrap();

    pub static ref ARK_REFILLS: prometheus::Counter = register_counter!(
        "synth_ark_refill_total",
        "On-chain refills of ark-swapd's Ark wallet the payer sent"
    ).unwrap();

    pub static ref ARK_REFILL_SATS: prometheus::Counter = register_counter!(
        "synth_ark_refill_sat_total",
        "Sats the payer sent on-chain to refill ark-swapd's Ark wallet"
    ).unwrap();

    pub static ref ARK_REFILL_LAST_SUCCESS: prometheus::Gauge = register_gauge!(
        "synth_ark_refill_last_success_timestamp_seconds",
        "Unix timestamp when the payer last sent a refill of ark-swapd's Ark wallet"
    ).unwrap();

    pub static ref ARK_REFILL_FAILURES: prometheus::Counter = register_counter!(
        "synth_ark_refill_failures_total",
        "Refills of ark-swapd's Ark wallet that were refused, dropped, or never reached it"
    ).unwrap();

    pub static ref ARKADE_TOPUPS: prometheus::Counter = register_counter!(
        "synth_arkade_topup_total",
        "On-chain top-ups of ark-swapd's Ark wallet the rebalancer sent (rebalance.arkade)"
    ).unwrap();

    pub static ref ARKADE_TOPUP_SATS: prometheus::Counter = register_counter!(
        "synth_arkade_topup_sat_total",
        "Sats the rebalancer sent on-chain to top up ark-swapd's Ark wallet"
    ).unwrap();

    pub static ref ARKADE_TOPUP_LAST_SUCCESS: prometheus::Gauge = register_gauge!(
        "synth_arkade_topup_last_success_timestamp_seconds",
        "Unix timestamp when the rebalancer last sent a top-up of ark-swapd's Ark wallet"
    ).unwrap();

    pub static ref ARKADE_TOPUP_FAILURES: prometheus::Counter = register_counter!(
        "synth_arkade_topup_failures_total",
        "Top-ups of ark-swapd's Ark wallet the rebalancer could not send"
    ).unwrap();

    pub static ref LANE_START_RETRIES: prometheus::CounterVec = register_counter_vec!(
        "synth_lane_start_retries_total",
        "Runs a lane tried again after an attempt that created no competition",
        &["lane"]
    ).unwrap();

    pub static ref RESUMED_RUNS: prometheus::Counter = register_counter!(
        "synth_resumed_runs_total",
        "Runs carried on after a restart from their last finished step"
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
    lazy_static::initialize(&ENTRY_FORM_MISSING_FORECASTS);
    // Set by the keep-open check once it has loaded a form.
    if OPEN_COMPETITION_ENTERABLE.get() == 0.0 && ENTRY_FORM_MISSING_FORECASTS.get() == 0.0 {
        OPEN_COMPETITION_ENTERABLE.set(f64::NAN);
    }
    lazy_static::initialize(&KEEP_OPEN_STARTS);
    lazy_static::initialize(&BACKFILL_PLAYERS);
    lazy_static::initialize(&ARK_REFILLS);
    lazy_static::initialize(&ARK_REFILL_SATS);
    lazy_static::initialize(&ARK_REFILL_FAILURES);
    // Set before the router is built when a refill was ever sent.
    if ARK_REFILL_LAST_SUCCESS.get() == 0.0 {
        ARK_REFILL_LAST_SUCCESS.set(f64::NAN);
    }
    lazy_static::initialize(&ARKADE_TOPUPS);
    lazy_static::initialize(&ARKADE_TOPUP_SATS);
    lazy_static::initialize(&ARKADE_TOPUP_FAILURES);
    // Set before the router is built when a top-up was ever sent.
    if ARKADE_TOPUP_LAST_SUCCESS.get() == 0.0 {
        ARKADE_TOPUP_LAST_SUCCESS.set(f64::NAN);
    }
    lazy_static::initialize(&LANE_START_RETRIES);
    lazy_static::initialize(&RESUMED_RUNS);
    for scenario in crate::runner::SCENARIOS {
        for status in ["passed", "failed"] {
            SCENARIO_RUNS.with_label_values(&[scenario, status]);
        }
        SCENARIO_DURATION.with_label_values(&[scenario]);
    }
    LIFECYCLE_HEALTHY.set(f64::NAN);
    LAST_SUCCESS.set(f64::NAN);
    crate::scenarios::stress::initialize_metrics();
}

/// The lifecycle gauge at `now`, in UNIX seconds, from the latest verdicts on the runs meant to
/// end in a payout.
///
/// - 0 from when such a run fails after making its competition until the next pass: the next
///   such run to finish with every step passed, or the next payout verified, whichever comes
///   first. Also 0 while the newest such run whose money has a verdict left it stuck or written
///   off.
/// - 1 otherwise, once a payout has been verified: steps passing alone is not a payout.
/// - NaN until a payout has been verified, and without fresh evidence.
///
/// Verdicts count by when they were reached, not by when their run started. A run takes a day
/// to pay out, so one that failed in its first hour used to hide every earlier run that paid out
/// after it, and the gauge stayed at 0 until a run started after the failure had paid out too.
pub(crate) fn lifecycle_health(evidence: &LifecycleEvidence, now: i64) -> f64 {
    // The slowest normal tracker cadence is fifteen minutes. Do not carry an old
    // healthy result through a stopped tracker or an idle installation indefinitely.
    let fresh = evidence
        .observed
        .is_some_and(|at| (0..=30 * 60).contains(&now.saturating_sub(at)));
    if !fresh {
        return f64::NAN;
    }
    let last_good = evidence.last_passed.max(evidence.last_paid_out);
    // Within the same second the failure counts, which a later pass then clears.
    let failed_since = evidence
        .last_failed
        .is_some_and(|failed| last_good.is_none_or(|good| good <= failed));
    let money_lost = matches!(
        evidence.newest_money.as_deref(),
        Some("stuck" | "written_off")
    );
    if failed_since || money_lost {
        0.0
    } else if evidence.last_paid_out.is_some() {
        1.0
    } else {
        f64::NAN
    }
}

/// The enterable gauge for what the open competitions' entry forms last showed.
pub(crate) fn enterable(forms: Option<Forms>) -> f64 {
    match forms {
        Some(forms) if forms.checked > 0 => f64::from(u8::from(forms.all_enterable())),
        _ => f64::NAN,
    }
}

async fn metrics_handler(State(db): State<SynthDb>) -> Response {
    let evidence = match db.lifecycle_metrics().await {
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
    if let Ok(Some(at)) = db.last_rebalance_at("arkade").await {
        record_arkade_topup_last_success(at.unix_timestamp());
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    LIFECYCLE_HEALTHY.set(lifecycle_health(&evidence, now));
    LAST_SUCCESS.set(evidence.last_paid_out.map_or(f64::NAN, |at| at as f64));
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

/// Record what the entry forms of the competitions open for visitors showed when last loaded;
/// None when none is open.
pub fn record_entry_forms(forms: Option<Forms>) {
    OPEN_COMPETITION_ENTERABLE.set(enterable(forms));
    ENTRY_FORM_MISSING_FORECASTS.set(forms.map_or(0.0, |forms| forms.missing_forecasts as f64));
}

/// Record a top-up of ark-swapd's Ark wallet the rebalancer sent at `at`, in UNIX seconds.
pub fn record_arkade_topup(sats: u64, at: i64) {
    ARKADE_TOPUPS.inc();
    ARKADE_TOPUP_SATS.inc_by(sats as f64);
    record_arkade_topup_last_success(at);
}

/// Record when the rebalancer last sent a top-up, in UNIX seconds.
pub fn record_arkade_topup_last_success(at: i64) {
    let at = at as f64;
    if ARKADE_TOPUP_LAST_SUCCESS.get().is_nan() || ARKADE_TOPUP_LAST_SUCCESS.get() < at {
        ARKADE_TOPUP_LAST_SUCCESS.set(at);
    }
}

/// Record a top-up the rebalancer could not send.
pub fn record_arkade_topup_failure() {
    ARKADE_TOPUP_FAILURES.inc();
}

/// Record a refill of ark-swapd's Ark wallet the payer sent at `at`, in UNIX seconds.
pub fn record_ark_refill(sats: u64, at: i64) {
    ARK_REFILLS.inc();
    ARK_REFILL_SATS.inc_by(sats as f64);
    record_ark_refill_last_success(at);
}

/// Record when the payer last sent a refill, in UNIX seconds.
pub fn record_ark_refill_last_success(at: i64) {
    let at = at as f64;
    if ARK_REFILL_LAST_SUCCESS.get().is_nan() || ARK_REFILL_LAST_SUCCESS.get() < at {
        ARK_REFILL_LAST_SUCCESS.set(at);
    }
}

/// Record a refill that was refused, dropped, or never reached ark-swapd.
pub fn record_ark_refill_failure() {
    ARK_REFILL_FAILURES.inc();
}

/// Record players entered by a backfill.
pub fn record_backfill(players: usize) {
    BACKFILL_PLAYERS.inc_by(players as f64);
}

/// Record a lane trying a run again after it created no competition.
pub fn record_lane_retry(lane: &str) {
    LANE_START_RETRIES.with_label_values(&[lane]).inc();
}

/// Record a run carried on after a restart.
pub fn record_resumed_run() {
    RESUMED_RUNS.inc();
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_000_000;

    /// Verdicts at these times: the last failure, the last pass, the last verified payout, and
    /// where the newest run with a money verdict left its money.
    fn evidence(
        failed: Option<i64>,
        passed: Option<i64>,
        paid_out: Option<i64>,
        money: Option<&str>,
    ) -> LifecycleEvidence {
        LifecycleEvidence {
            last_failed: failed,
            last_passed: passed,
            last_paid_out: paid_out,
            newest_money: money.map(str::to_owned),
            observed: Some(NOW - 60),
        }
    }

    fn health(evidence: LifecycleEvidence) -> f64 {
        lifecycle_health(&evidence, NOW)
    }

    #[test]
    fn steps_passing_is_not_lifecycle_success() {
        assert!(health(evidence(None, Some(10), None, None)).is_nan());
        assert_eq!(
            health(evidence(None, Some(10), Some(20), Some("paid_out"))),
            1.0
        );
        assert!(lifecycle_health(&LifecycleEvidence::default(), NOW).is_nan());
    }

    /// One failed run read as unhealthy for a day, through nine later passes: the gauge took
    /// the newest run by its start, and none started after the failure had paid out yet.
    #[test]
    fn a_failed_run_reads_unhealthy_only_until_the_next_pass() {
        let failed = Some(100);
        assert_eq!(
            health(evidence(failed, Some(90), Some(80), Some("paid_out"))),
            0.0
        );
        // The next run to finish with every step passed clears it,
        assert_eq!(
            health(evidence(failed, Some(110), Some(80), Some("paid_out"))),
            1.0
        );
        // and so does the next payout verified, of a run that started long before.
        assert_eq!(
            health(evidence(failed, Some(90), Some(120), Some("paid_out"))),
            1.0
        );
        // Before any payout was verified a failure counts, and a pass after it proves nothing.
        assert_eq!(health(evidence(failed, None, None, None)), 0.0);
        assert!(health(evidence(failed, Some(110), None, None)).is_nan());
    }

    #[test]
    fn stuck_money_reads_unhealthy_and_stale_evidence_reads_as_none() {
        for money in ["stuck", "written_off"] {
            assert_eq!(
                health(evidence(None, Some(110), Some(120), Some(money))),
                0.0
            );
        }
        let mut stale = evidence(None, Some(110), Some(120), Some("paid_out"));
        stale.observed = Some(NOW - 31 * 60);
        assert!(lifecycle_health(&stale, NOW).is_nan());
        stale.observed = None;
        assert!(lifecycle_health(&stale, NOW).is_nan());
    }

    #[test]
    fn the_enterable_gauge_reads_the_forms_last_loaded() {
        let forms = |checked, enterable, missing_forecasts| {
            Some(Forms {
                checked,
                enterable,
                missing_forecasts,
            })
        };
        assert_eq!(enterable(forms(2, 2, 0)), 1.0);
        assert_eq!(enterable(forms(2, 1, 9)), 0.0);
        assert!(enterable(forms(0, 0, 0)).is_nan(), "none loaded");
        assert!(enterable(None).is_nan(), "none open");
    }
}
