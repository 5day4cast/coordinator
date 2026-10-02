//! A competition pushed to its upper bound: as many seats as the coordinator gives one pool, and
//! players arriving all at once to take them.
//!
//! 1. Check the fees a ticket carries now fit `max_ticket_fees_sats`, so no ticket is refused for
//!    its price, and say the most the run can pay.
//! 2. Create a competition with [`MAX_POOL_PLAYERS`] seats, unlisted unless the lane lists it.
//! 3. Let `users` players arrive at random within `burst_window_secs` of its creation and, at most
//!    `concurrency` at a time, request a ticket, register, pay and submit with no wait between.
//!    A player the coordinator refuses tries again a moment later, up to `retries` times. Every
//!    step's time and every refusal, with its status and the coordinator's message, is recorded in
//!    the player's step.
//! 4. Follow the competition like `full_lifecycle`: on to its attestation with the winners paid,
//!    or, if it did not fill or was cancelled, every paid ticket refunded.
//!
//! The run passes when at least `min_admitted` players got in, the competition kicked off with all
//! of them, and the money settled. A player who has started paying never pays again, and a paid
//! ticket holds its seat, so the run pays for at most [`MAX_POOL_PLAYERS`] tickets of at most
//! `entry_fee + max_ticket_fees_sats` each, before routing fees.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use coordinator_core::keymeld::pools::MAX_POOL_PLAYERS;
use futures::{stream::FuturesUnordered, StreamExt};
use prometheus::{register_counter, register_counter_vec, register_histogram_vec};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::common::{entry_deadline, finish_result, load_users, run_step, Steps};
use super::full_lifecycle::{self, Payer, PreparedEntry};
use super::types::*;
use super::user_behavior;
use crate::client::entries::{ApiRejection, EntrySubmission};
use crate::client::CoordinatorClient;
use crate::crypto::keys::SynthUser;
use crate::db::SynthDb;
use crate::lnd::Lnd;
use crate::trail::EntryTrace;

pub const STRESS_FULL_POOL: &str = "stress_full_pool";

/// How hard a stress run pushes its competition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct StressSettings {
    /// Players who try to enter; the pool cap by default. More than the cap tests the refusals.
    pub users: usize,
    /// Every player arrives within this many seconds of the competition's creation.
    pub burst_window_secs: u64,
    /// Players entering at once.
    pub concurrency: usize,
    /// Times a refused player tries again.
    pub retries: u32,
    /// The fewest players who must get in for the run to pass; the pool cap by default.
    pub min_admitted: usize,
}

impl Default for StressSettings {
    fn default() -> Self {
        Self {
            users: MAX_POOL_PLAYERS,
            burst_window_secs: 60,
            concurrency: 10,
            retries: 3,
            min_admitted: MAX_POOL_PLAYERS,
        }
    }
}

/// What a burst leaves before the entry deadline for the last player to pay and submit, and for
/// the 60-second invoice cutoff.
const BURST_SLACK_SECS: u64 = 180;

impl StressSettings {
    pub fn validate(&self, entry_window_secs: u64) -> Result<()> {
        anyhow::ensure!(
            (1..=100).contains(&self.users),
            "stress.users must be between 1 and 100"
        );
        anyhow::ensure!(
            (1..=self.users.min(MAX_POOL_PLAYERS)).contains(&self.min_admitted),
            "stress.min_admitted must be between 1 and the {} seats players can take",
            self.users.min(MAX_POOL_PLAYERS)
        );
        anyhow::ensure!(
            (1..=100).contains(&self.concurrency),
            "stress.concurrency must be between 1 and 100"
        );
        anyhow::ensure!(self.retries <= 10, "stress.retries must be at most 10");
        anyhow::ensure!(
            self.burst_window_secs
                .checked_add(BURST_SLACK_SECS)
                .is_some_and(|end| end < entry_window_secs),
            "stress.burst_window_secs must end at least {BURST_SLACK_SECS} seconds before entries \
             close, inside the {entry_window_secs}-second entry window"
        );
        Ok(())
    }

    /// The most the run can pay for entries, before routing fees: a ticket for every seat at the
    /// highest price synth pays.
    pub fn max_spend_sats(entry_fee: u64, max_ticket_fees_sats: u64) -> u64 {
        (MAX_POOL_PLAYERS as u64).saturating_mul(entry_fee.saturating_add(max_ticket_fees_sats))
    }
}

/// Spread a planned stress run's arrivals over its burst, with no wait between a player's steps.
pub(super) fn burst(config: &mut ScenarioConfig, rng: &mut impl rand::Rng) {
    let window = config.stress.clone().unwrap_or_default().burst_window_secs;
    for plan in &mut config.entry_plan {
        plan.arrival_secs = rng.random_range(0..=window);
        plan.before_payment_secs = 0;
        plan.before_submit_secs = 0;
    }
}

lazy_static::lazy_static! {
    static ref STEP_SECONDS: prometheus::HistogramVec = register_histogram_vec!(
        "synth_stress_step_seconds",
        "How long each step of a stress run's entries took, refused or not",
        &["step"],
        vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]
    ).unwrap();

    static ref ADMITTED: prometheus::Counter = register_counter!(
        "synth_stress_admitted_total",
        "Players a stress run got into its competition"
    ).unwrap();

    static ref REFUSED: prometheus::CounterVec = register_counter_vec!(
        "synth_stress_refused_total",
        "Refusals stress-run players met, by reason",
        &["reason"]
    ).unwrap();
}

/// Register the stress metrics, each label at zero, so they show before the first stress run.
pub fn initialize_metrics() {
    for stage in Stage::ALL {
        STEP_SECONDS.with_label_values(&[stage.as_str()]);
    }
    lazy_static::initialize(&ADMITTED);
    for reason in Reason::ALL {
        REFUSED.with_label_values(&[reason.as_str()]);
    }
}

/// A step of a player's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Ticket,
    Registration,
    Payment,
    Submission,
}

impl Stage {
    const ALL: [Stage; 4] = [
        Self::Ticket,
        Self::Registration,
        Self::Payment,
        Self::Submission,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Ticket => "ticket",
            Self::Registration => "registration",
            Self::Payment => "payment",
            Self::Submission => "submission",
        }
    }
}

/// Why a player was refused: a bounded set, for a metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Reason {
    /// Entries are paused, for network fees or the Arkade server.
    Paused,
    /// Every seat is held or the competition closed.
    Full,
    Ticket,
    Payment,
    Registration,
    Submission,
    /// Out of time before the deadline, or nothing the coordinator said.
    Other,
}

impl Reason {
    const ALL: [Reason; 7] = [
        Self::Paused,
        Self::Full,
        Self::Ticket,
        Self::Payment,
        Self::Registration,
        Self::Submission,
        Self::Other,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Full => "full",
            Self::Ticket => "ticket",
            Self::Payment => "payment",
            Self::Registration => "registration",
            Self::Submission => "submission",
            Self::Other => "other",
        }
    }
}

/// One refusal a player met.
#[derive(Debug, Clone, Serialize)]
struct Refusal {
    stage: Stage,
    reason: Reason,
    /// The coordinator's HTTP status, when it answered.
    status: Option<u16>,
    message: String,
}

impl Refusal {
    fn of(stage: Stage, error: &anyhow::Error) -> Self {
        let rejection = error.downcast_ref::<ApiRejection>();
        Self {
            stage,
            reason: reason(stage, rejection),
            status: rejection.map(|rejection| rejection.status),
            message: rejection.map_or_else(
                || format!("{error:#}"),
                |rejection| rejection.message.clone(),
            ),
        }
    }

    fn rejected(stage: Stage, rejection: &ApiRejection) -> Self {
        Self {
            stage,
            reason: reason(stage, Some(rejection)),
            status: Some(rejection.status),
            message: rejection.message.clone(),
        }
    }

    fn other(stage: Stage, error: &anyhow::Error) -> Self {
        Self {
            stage,
            reason: Reason::Other,
            status: None,
            message: format!("{error:#}"),
        }
    }
}

fn reason(stage: Stage, rejection: Option<&ApiRejection>) -> Reason {
    match rejection {
        Some(rejection)
            if rejection.status == 503 && rejection.message.starts_with("Entries are paused") =>
        {
            Reason::Paused
        }
        Some(rejection) if rejection.is_no_capacity() || rejection.is_entries_closed() => {
            Reason::Full
        }
        _ => match stage {
            Stage::Ticket => Reason::Ticket,
            Stage::Registration => Reason::Registration,
            Stage::Payment => Reason::Payment,
            Stage::Submission => Reason::Submission,
        },
    }
}

/// What a player's step records about the stress, next to its entry trace.
#[derive(Debug, Clone, Default, Serialize)]
struct PlayerStress {
    /// Times the player asked for a ticket.
    attempts: u32,
    /// How long the player waited for one of the `concurrency` slots after arriving.
    slot_wait_ms: u64,
    /// How long each step took the last time it went through.
    latency_ms: BTreeMap<Stage, u64>,
    refusals: Vec<Refusal>,
    admitted: bool,
}

impl PlayerStress {
    fn timed(&mut self, stage: Stage, started: Instant, refused: Option<Refusal>) {
        let took = started.elapsed();
        STEP_SECONDS
            .with_label_values(&[stage.as_str()])
            .observe(took.as_secs_f64());
        match refused {
            Some(refusal) => self.refuse(refusal),
            None => {
                self.latency_ms
                    .insert(stage, took.as_millis().try_into().unwrap_or(u64::MAX));
            }
        }
    }

    fn refuse(&mut self, refusal: Refusal) {
        REFUSED.with_label_values(&[refusal.reason.as_str()]).inc();
        self.refusals.push(refusal);
    }
}

/// What the players of one stress run share.
struct Burst<'a> {
    client: &'a CoordinatorClient,
    config: &'a ScenarioConfig,
    settings: &'a StressSettings,
    payer: &'a Payer<'a>,
    competition_id: Uuid,
    deadline: OffsetDateTime,
    anchor: Instant,
    slots: Semaphore,
}

pub async fn run_stress_full_pool(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
) -> ScenarioResult {
    let started_at = OffsetDateTime::now_utc();
    let started = Instant::now();
    let mut steps = Steps::new();
    let settings = config.stress.clone().unwrap_or_default();
    let prepared = run_step("prepare_stress", || prepare(client, config, &settings)).await;
    let lnd = match prepared {
        Ok((mut step, (details, lnd))) => {
            step.details = Some(details);
            steps.push(step);
            lnd
        }
        Err(step) => {
            steps.push(*step);
            return finish_result(STRESS_FULL_POOL, started_at, started, steps, true);
        }
    };
    let payer = lnd.as_ref().map_or(Payer::TestEndpoint, Payer::Lnd);
    let failed = run_steps(client, db, config, &settings, &payer, &mut steps)
        .await
        .map_err(|step| steps.push(*step))
        .is_err();
    finish_result(STRESS_FULL_POOL, started_at, started, steps, failed)
}

/// Check the fees fit before anything is created or paid, and name the payer; with the step's
/// details, which say the most the run can pay.
async fn prepare(
    client: &CoordinatorClient,
    config: &ScenarioConfig,
    settings: &StressSettings,
) -> Result<(serde_json::Value, Option<Lnd>)> {
    settings.validate(config.entry_window_secs)?;
    anyhow::ensure!(
        config.entry_plan.len() == settings.users,
        "the recorded plan has {} players for {} stress users",
        config.entry_plan.len(),
        settings.users
    );
    let entry_fee = u64::try_from(config.entry_fee).context("entry fee exceeds u64")?;
    let quote = client.network_fee_quote().await?;
    let coordinator_fee =
        (entry_fee * u64::from(full_lifecycle::COORDINATOR_FEE_BASIS_POINTS)).div_ceil(10_000);
    let fees = coordinator_fee + quote.network_fee_sats;
    let max_spend_sats = StressSettings::max_spend_sats(entry_fee, config.max_ticket_fees_sats);
    anyhow::ensure!(
        fees <= config.max_ticket_fees_sats,
        "max_ticket_fees_sats ({}) does not cover a ticket's fees now: {coordinator_fee} sats to \
         the coordinator and {} sats of network fee; nothing was created or paid",
        config.max_ticket_fees_sats,
        quote.network_fee_sats
    );
    let lnd = config.lnd.as_ref().map(Lnd::new).transpose()?;
    let details = serde_json::json!({
        "players": settings.users,
        "seats": MAX_POOL_PLAYERS,
        "concurrency": settings.concurrency,
        "burst_window_secs": settings.burst_window_secs,
        "retries": settings.retries,
        "min_admitted": settings.min_admitted,
        "ticket_fees_now_sats": fees,
        "max_ticket_fees_sats": config.max_ticket_fees_sats,
        "max_spend_sats": max_spend_sats,
        "pays_from_node": lnd.is_some(),
        "paused_now": quote.paused(entry_fee),
    });
    Ok((details, lnd))
}

async fn run_steps(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
    settings: &StressSettings,
    payer: &Payer<'_>,
    steps: &mut Steps,
) -> std::result::Result<(), Box<StepResult>> {
    let (mut created, competition_id) = run_step(REFUSED_AS_SMALL_STEP, || {
        full_lifecycle::create_single(client, config, MAX_POOL_PLAYERS, !config.listed)
    })
    .await?;
    created.details = Some(serde_json::json!({
        "competition_id": competition_id,
        "seats": MAX_POOL_PLAYERS,
        "listed": config.listed,
    }));
    steps.push(created);
    let anchor = Instant::now();
    let (deadline_step, deadline) =
        run_step("entry_deadline", || entry_deadline(client, &competition_id)).await?;
    steps.push(deadline_step);
    let (loaded, users) = run_step("load_users", || load_users(db, config.users)).await?;
    steps.push(loaded);

    let burst = Burst {
        client,
        config,
        settings,
        payer,
        competition_id,
        deadline,
        anchor,
        slots: Semaphore::new(settings.concurrency),
    };
    let mut players: FuturesUnordered<_> = config
        .entry_plan
        .iter()
        .map(|plan| enter(&burst, &users[plan.user_index], plan))
        .collect();
    let mut traces = Vec::new();
    let mut stresses = Vec::new();
    while let Some((step, trace, stress)) = players.next().await {
        steps.push(step);
        stresses.push((trace.user.clone(), stress));
        traces.push(trace);
    }

    let summary = Summary::of(&stresses, settings);
    ADMITTED.inc_by(summary.admitted as f64);
    steps.push(StepResult {
        name: "stress_entries".into(),
        status: StepStatus::Passed,
        duration_ms: anchor.elapsed().as_millis() as i64,
        details: serde_json::to_value(&summary).ok(),
        error: None,
    });

    let settled = settle(
        client,
        &users,
        &competition_id,
        config,
        deadline,
        &traces,
        &summary,
        steps,
    )
    .await;
    let verdict = summary.verdict(settled.as_ref().err().map(String::as_str));
    let status = match verdict {
        Ok(()) => StepStatus::Passed,
        Err(_) => StepStatus::Failed,
    };
    let result = StepResult {
        name: "stress_result".into(),
        status,
        duration_ms: 0,
        details: serde_json::to_value(&summary).ok(),
        error: verdict.err(),
    };
    if result.status == StepStatus::Failed {
        return Err(Box::new(result));
    }
    steps.push(result);
    Ok(())
}

/// Follow the money to where it settles: the competition's attestation if it filled, or every
/// paid ticket's refund if it did not run. Err says why the competition did not kick off with
/// every admitted player, or what kept the money from settling.
#[allow(clippy::too_many_arguments)]
async fn settle(
    client: &CoordinatorClient,
    users: &[SynthUser],
    competition_id: &Uuid,
    config: &ScenarioConfig,
    deadline: OffsetDateTime,
    traces: &[EntryTrace],
    summary: &Summary,
    steps: &mut Steps,
) -> std::result::Result<(), String> {
    let filled = if summary.admitted < summary.seats {
        // It cannot fill unless other players take the seats left; it is cancelled otherwise.
        let mut cancellation = config.clone();
        cancellation.state_timeout_secs = user_behavior::cancellation_budget_secs(
            config.state_timeout_secs,
            deadline,
            OffsetDateTime::now_utc(),
        );
        let ours = traces.iter().filter(|trace| trace.paid).count() as u64;
        let waited = run_step("wait_cancelled", || {
            user_behavior::wait_cancelled_or_filled(client, competition_id, &cancellation, ours)
        })
        .await;
        match waited {
            Ok((step, filled)) => {
                steps.push(step);
                filled
            }
            Err(step) => {
                let error = step.error.clone().unwrap_or_default();
                steps.push(*step);
                return Err(error);
            }
        }
    } else {
        true
    };
    if !filled {
        user_behavior::collect_refunds(client, users, competition_id, config, traces, steps)
            .await
            .map_err(|step| {
                let error = step.error.clone().unwrap_or_default();
                steps.push(*step);
                error
            })?;
        return Err(format!(
            "the competition did not fill and was cancelled; {} paid tickets refunded",
            traces.iter().filter(|trace| trace.paid).count()
        ));
    }
    user_behavior::follow_lifecycle(client, users, competition_id, config, traces, steps)
        .await
        .map_err(|step| {
            let error = step.error.clone().unwrap_or_default();
            steps.push(*step);
            error
        })?;
    // The lifecycle also ends, with refunds collected, if the competition was cancelled for
    // something synth's players did not do; that is not a kickoff.
    let competition = client
        .get_competition(competition_id)
        .await
        .map_err(|error| format!("{error:#}"))?;
    if competition.cancelled_at.is_some() || competition.failed_at.is_some() {
        let why = user_behavior::expected_cancellation(&competition, traces)
            .unwrap_or("the coordinator stopped it");
        return Err(format!(
            "the competition did not kick off ({why}); its paid tickets were refunded"
        ));
    }
    if competition.total_entries < summary.admitted as u64 {
        return Err(format!(
            "the competition kicked off with {} entries, fewer than the {} players admitted",
            competition.total_entries, summary.admitted
        ));
    }
    Ok(())
}

/// One player: arrive, wait for a slot, then ticket, registration, payment and submission with
/// no wait between, trying again after a refusal. Never pays twice.
async fn enter(
    burst: &Burst<'_>,
    user: &SynthUser,
    plan: &EntryPlan,
) -> (StepResult, EntryTrace, PlayerStress) {
    let name = format!("user_{}_enter", user.name);
    let mut trace = EntryTrace::new(user);
    trace.behavior = Some(plan.behavior);
    trace.payment_started = Some(false);
    let mut stress = PlayerStress::default();
    let result = run_step(&name, || async {
        tokio::time::sleep_until((burst.anchor + Duration::from_secs(plan.arrival_secs)).into())
            .await;
        let waiting = Instant::now();
        let _slot = burst.slots.acquire().await.context("stress slots closed")?;
        stress.slot_wait_ms = waiting.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        let Some(prepared) = take_seat(burst, user, plan, &name, &mut trace, &mut stress).await?
        else {
            return Ok(());
        };
        if let Err(error) = user_behavior::ensure_payment_time(burst.deadline, burst.config) {
            stress.refuse(Refusal::other(Stage::Payment, &error));
            return Ok(());
        }
        let paying = Instant::now();
        let paid = full_lifecycle::pay_entry(
            burst.client,
            user,
            &burst.competition_id,
            &prepared,
            burst.payer,
            &name,
            &mut trace,
        )
        .await;
        match paid {
            Ok(()) => stress.timed(Stage::Payment, paying, None),
            // Paid, but the coordinator has not seen it yet: submitting says whether it has.
            Err(error) if trace.paid => stress.timed(
                Stage::Payment,
                paying,
                Some(Refusal::of(Stage::Payment, &error)),
            ),
            // A payment that may have gone out is never sent again.
            Err(error) => {
                stress.timed(
                    Stage::Payment,
                    paying,
                    Some(Refusal::of(Stage::Payment, &error)),
                );
                return Ok(());
            }
        }
        submit(burst, user, &prepared, &name, &mut trace, &mut stress).await
    })
    .await;
    let mut step = match result {
        Ok((step, ())) => step,
        Err(step) => *step,
    };
    stress.admitted = trace.entry_submitted;
    if step.status == StepStatus::Passed && !stress.admitted {
        // Refused, not broken: the summary judges the run.
        step.status = StepStatus::Skipped;
        step.error = stress
            .refusals
            .last()
            .map(|refusal| format!("refused ({}): {}", refusal.reason.as_str(), refusal.message));
    }
    let mut step = trace.attach(step);
    if let Some(serde_json::Value::Object(details)) = step.details.as_mut() {
        details.insert(
            "stress".into(),
            serde_json::to_value(&stress).unwrap_or_default(),
        );
    }
    (step, trace, stress)
}

/// Request a ticket and register it, trying again after a refusal; None once out of tries.
async fn take_seat(
    burst: &Burst<'_>,
    user: &SynthUser,
    plan: &EntryPlan,
    step: &str,
    trace: &mut EntryTrace,
    stress: &mut PlayerStress,
) -> Result<Option<PreparedEntry>> {
    loop {
        if let Err(error) = user_behavior::ensure_payment_time(burst.deadline, burst.config) {
            stress.refuse(Refusal::other(Stage::Ticket, &error));
            return Ok(None);
        }
        stress.attempts += 1;
        let asking = Instant::now();
        let requested = full_lifecycle::request_entry(
            burst.client,
            user,
            &burst.competition_id,
            burst.config.lightning_address.as_deref(),
            step,
            trace,
        )
        .await;
        let refused = match requested {
            Ok(requested) => {
                stress.timed(Stage::Ticket, asking, None);
                let registering = Instant::now();
                match full_lifecycle::register_entry(
                    burst.client,
                    user,
                    &burst.competition_id,
                    burst.config,
                    plan.user_index,
                    requested,
                    step,
                    trace,
                )
                .await
                {
                    Ok(prepared) => {
                        stress.timed(Stage::Registration, registering, None);
                        return Ok(Some(prepared));
                    }
                    Err(error) => (Stage::Registration, registering, error),
                }
            }
            Err(error) => (Stage::Ticket, asking, error),
        };
        let (stage, began, error) = refused;
        stress.timed(stage, began, Some(Refusal::of(stage, &error)));
        if stress.attempts > burst.settings.retries {
            return Ok(None);
        }
        tokio::time::sleep(backoff(stress.attempts)).await;
    }
}

/// Submit the paid entry, trying again after a refusal. An entry the coordinator says was
/// already used went in on an earlier try whose answer was lost.
async fn submit(
    burst: &Burst<'_>,
    user: &SynthUser,
    prepared: &PreparedEntry,
    step: &str,
    trace: &mut EntryTrace,
    stress: &mut PlayerStress,
) -> Result<()> {
    for attempt in 1..=burst.settings.retries + 1 {
        if let Err(error) = user_behavior::ensure_submission_time(burst.deadline, burst.config) {
            stress.refuse(Refusal::other(Stage::Submission, &error));
            return Ok(());
        }
        trace.submission_attempts += 1;
        crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
        let submitting = Instant::now();
        match burst
            .client
            .attempt_submit_entry(&user.nostr_keys, &prepared.entry)
            .await
        {
            Ok(EntrySubmission::Accepted(entry)) => {
                trace.entry_id = Some(entry.id);
                trace.entry_submitted = true;
                stress.timed(Stage::Submission, submitting, None);
            }
            Ok(EntrySubmission::Rejected(rejection))
                if attempt > 1 && rejection.message == "Ticket has already been used" =>
            {
                trace.entry_submitted = true;
                stress.timed(Stage::Submission, submitting, None);
            }
            Ok(EntrySubmission::Rejected(rejection)) => {
                trace.rejected_submission = Some(rejection.clone());
                stress.timed(
                    Stage::Submission,
                    submitting,
                    Some(Refusal::rejected(Stage::Submission, &rejection)),
                );
            }
            Err(error) => stress.timed(
                Stage::Submission,
                submitting,
                Some(Refusal::of(Stage::Submission, &error)),
            ),
        }
        crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
        if trace.entry_submitted {
            return Ok(());
        }
        tokio::time::sleep(backoff(attempt)).await;
    }
    Ok(())
}

/// A short wait before trying again, longer each time.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500) * attempt.min(10)
}

/// The slowest step any player took.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct Slowest {
    user: String,
    step: Stage,
    ms: u64,
}

/// How the burst went: who got in, who was refused and why, and the slowest step.
#[derive(Debug, Clone, Serialize)]
struct Summary {
    players: usize,
    seats: usize,
    min_admitted: usize,
    admitted: usize,
    refused: usize,
    /// Players left out, by the reason of the last refusal each met.
    refused_by_reason: BTreeMap<Reason, usize>,
    /// Every refusal met, admitted players' included, by reason.
    refusals_by_reason: BTreeMap<Reason, usize>,
    slowest: Option<Slowest>,
}

impl Summary {
    fn of(players: &[(String, PlayerStress)], settings: &StressSettings) -> Self {
        let mut refused_by_reason = BTreeMap::new();
        let mut refusals_by_reason = BTreeMap::new();
        let mut slowest: Option<Slowest> = None;
        for (user, stress) in players {
            for refusal in &stress.refusals {
                *refusals_by_reason.entry(refusal.reason).or_default() += 1;
            }
            if !stress.admitted {
                let reason = stress
                    .refusals
                    .last()
                    .map_or(Reason::Other, |refusal| refusal.reason);
                *refused_by_reason.entry(reason).or_default() += 1;
            }
            for (step, ms) in &stress.latency_ms {
                if slowest.as_ref().is_none_or(|slowest| *ms > slowest.ms) {
                    slowest = Some(Slowest {
                        user: user.clone(),
                        step: *step,
                        ms: *ms,
                    });
                }
            }
        }
        let admitted = players.iter().filter(|(_, stress)| stress.admitted).count();
        Self {
            players: players.len(),
            seats: MAX_POOL_PLAYERS,
            min_admitted: settings.min_admitted,
            admitted,
            refused: players.len() - admitted,
            refused_by_reason,
            refusals_by_reason,
            slowest,
        }
    }

    /// Whether the run passed: enough players in, and the competition ran with them and settled,
    /// which `settled` says it did not when set. Err names the counts.
    fn verdict(&self, settled: Option<&str>) -> std::result::Result<(), String> {
        let counts = format!(
            "{} of {} players admitted (at least {} needed), {} refused{}",
            self.admitted,
            self.players,
            self.min_admitted,
            self.refused,
            if self.refused_by_reason.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    self.refused_by_reason
                        .iter()
                        .map(|(reason, count)| format!("{}: {count}", reason.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        );
        match settled {
            _ if self.admitted < self.min_admitted => Err(counts),
            Some(why) => Err(format!("{why}; {counts}")),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stress(admitted: bool, refusals: &[Reason], latency: &[(Stage, u64)]) -> PlayerStress {
        PlayerStress {
            attempts: refusals.len() as u32 + 1,
            slot_wait_ms: 0,
            latency_ms: latency.iter().copied().collect(),
            refusals: refusals
                .iter()
                .map(|reason| Refusal {
                    stage: Stage::Ticket,
                    reason: *reason,
                    status: Some(400),
                    message: "No ticket available for competition".into(),
                })
                .collect(),
            admitted,
        }
    }

    #[test]
    fn the_defaults_fill_the_pool_cap() {
        let settings = StressSettings::default();
        assert_eq!(settings.users, MAX_POOL_PLAYERS);
        assert_eq!(settings.min_admitted, MAX_POOL_PLAYERS);
        assert_eq!(
            (
                settings.burst_window_secs,
                settings.concurrency,
                settings.retries
            ),
            (60, 10, 3)
        );
        settings.validate(3600).unwrap();
        // The burst must leave time to pay and submit before entries close.
        assert!(settings.validate(200).is_err());
        let too_many = StressSettings {
            min_admitted: MAX_POOL_PLAYERS + 1,
            users: 30,
            ..Default::default()
        };
        assert!(too_many.validate(3600).is_err());
        assert_eq!(
            StressSettings::max_spend_sats(5_000, 1_000),
            MAX_POOL_PLAYERS as u64 * 6_000
        );
    }

    #[test]
    fn a_stress_plan_has_the_caps_players_arriving_in_the_burst() {
        let config = ScenarioConfig {
            seed: Some(9),
            entry_window_secs: 3600,
            player_mix: Some(PlayerMix::default()),
            stress: Some(StressSettings {
                burst_window_secs: 30,
                ..Default::default()
            }),
            ..Default::default()
        };
        let plan = config.resolve_plan(STRESS_FULL_POOL).unwrap();
        assert_eq!(plan.users, MAX_POOL_PLAYERS, "the mix does not draw it");
        assert_eq!(plan.entry_plan.len(), MAX_POOL_PLAYERS);
        assert!(plan.entry_plan.iter().all(|entry| entry.arrival_secs <= 30
            && entry.before_payment_secs == 0
            && entry.before_submit_secs == 0
            && entry.behavior == EntryBehavior::Complete));
        assert_eq!(
            plan.resolve_plan(STRESS_FULL_POOL).unwrap().entry_plan,
            plan.entry_plan,
            "a recorded plan resolves to itself"
        );
        // Too short an entry window for the burst is refused before anything is recorded.
        let short = ScenarioConfig {
            entry_window_secs: 120,
            ..config
        };
        assert!(short.resolve_plan(STRESS_FULL_POOL).is_err());
    }

    #[test]
    fn refusals_are_sorted_into_a_bounded_set_of_reasons() {
        let rejected = |status, message: &str| ApiRejection {
            status,
            message: message.into(),
        };
        let paused = rejected(
            503,
            "Entries are paused while Bitcoin network fees are high",
        );
        assert_eq!(reason(Stage::Ticket, Some(&paused)), Reason::Paused);
        let full = rejected(400, "No ticket available for competition");
        assert_eq!(reason(Stage::Ticket, Some(&full)), Reason::Full);
        let closed = rejected(400, "Competition is no longer accepting entries");
        assert_eq!(reason(Stage::Submission, Some(&closed)), Reason::Full);
        let other = rejected(500, "database is locked");
        assert_eq!(
            reason(Stage::Registration, Some(&other)),
            Reason::Registration
        );
        assert_eq!(reason(Stage::Payment, None), Reason::Payment);
        let error = anyhow::Error::new(full).context("Failed to request ticket");
        let refusal = Refusal::of(Stage::Ticket, &error);
        assert_eq!(
            (refusal.reason, refusal.status, refusal.message.as_str()),
            (
                Reason::Full,
                Some(400),
                "No ticket available for competition"
            )
        );
        assert_eq!(Reason::ALL.len(), 7);
    }

    #[test]
    fn the_summary_counts_who_got_in_and_fails_below_the_minimum() {
        let settings = StressSettings {
            users: 4,
            min_admitted: 2,
            ..Default::default()
        };
        let players = vec![
            (
                "alice".to_string(),
                stress(true, &[], &[(Stage::Ticket, 40), (Stage::Payment, 900)]),
            ),
            (
                "bob".to_string(),
                stress(true, &[Reason::Full], &[(Stage::Submission, 70)]),
            ),
            (
                "charlie".to_string(),
                stress(false, &[Reason::Full, Reason::Paused], &[]),
            ),
            ("dave".to_string(), stress(false, &[Reason::Full], &[])),
        ];
        let summary = Summary::of(&players, &settings);
        assert_eq!((summary.admitted, summary.refused), (2, 2));
        assert_eq!(
            summary.refused_by_reason,
            BTreeMap::from([(Reason::Full, 1), (Reason::Paused, 1)])
        );
        assert_eq!(summary.refusals_by_reason[&Reason::Full], 3);
        assert_eq!(
            summary.slowest,
            Some(Slowest {
                user: "alice".into(),
                step: Stage::Payment,
                ms: 900
            })
        );
        summary.verdict(None).unwrap();
        let error = summary
            .verdict(Some("the competition did not fill"))
            .unwrap_err();
        assert!(error.starts_with("the competition did not fill; 2 of 4 players admitted"));
        let strict = Summary::of(
            &players,
            &StressSettings {
                min_admitted: 3,
                ..settings
            },
        );
        let error = strict.verdict(None).unwrap_err();
        assert_eq!(
            error,
            "2 of 4 players admitted (at least 3 needed), 2 refused (paused: 1, full: 1)"
        );
    }
}
