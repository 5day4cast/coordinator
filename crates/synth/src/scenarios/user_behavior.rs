//! Concurrent players with recorded human delays and deliberate entry mistakes.
//! Futures stay on the runner's task so its durable, task-local recorder is preserved.

use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use super::common::{finish_result, load_users, run_step, wait_for_state, Steps};
use super::full_lifecycle::{self, Payer, PreparedEntry};
use super::types::*;
use crate::client::competitions::CompetitionResponse;
use crate::client::entries::{ApiRejection, EntrySubmission, TicketStatus};
use crate::client::CoordinatorClient;
use crate::crypto::keys::SynthUser;
use crate::db::SynthDb;
use crate::lnd::Lnd;
use crate::trail::{EntryTrace, EntryWait};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Scenario {
    FullLifecycle,
    EscrowRefund,
    AbandonedUnpaid,
    PaidAbandonment,
    DuplicateSubmission,
    LateSubmission,
    QueuedSplit,
    QueuedOnePool,
    QueuedTooFew,
    QueuedLeftoverRefund,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Self::FullLifecycle => "full_lifecycle",
            Self::EscrowRefund => "escrow_refund",
            Self::AbandonedUnpaid => "abandoned_unpaid",
            Self::PaidAbandonment => "paid_abandonment",
            Self::DuplicateSubmission => "duplicate_submission",
            Self::LateSubmission => "late_submission",
            Self::QueuedSplit => super::queued::QUEUED_SPLIT,
            Self::QueuedOnePool => super::queued::QUEUED_ONE_POOL,
            Self::QueuedTooFew => super::queued::QUEUED_TOO_FEW,
            Self::QueuedLeftoverRefund => super::queued::QUEUED_LEFTOVER_REFUND,
        }
    }

    fn refunds(self) -> bool {
        matches!(
            self,
            Self::EscrowRefund
                | Self::PaidAbandonment
                | Self::LateSubmission
                | Self::QueuedTooFew
                | Self::QueuedLeftoverRefund
        )
    }

    /// Every queued entry waits in an Arkade escrow, which only a real payment funds, and names
    /// the Lightning Address it is refunded to if its queue never starts.
    fn queued(self) -> bool {
        super::queued::is_queued(self.name())
    }
}

macro_rules! scenario {
    ($function:ident, $kind:ident) => {
        pub async fn $function(
            client: &CoordinatorClient,
            db: &SynthDb,
            config: &ScenarioConfig,
        ) -> ScenarioResult {
            run(client, db, config, Scenario::$kind).await
        }
    };
}
scenario!(run_abandoned_unpaid, AbandonedUnpaid);
scenario!(run_paid_abandonment, PaidAbandonment);
scenario!(run_duplicate_submission, DuplicateSubmission);
scenario!(run_late_submission, LateSubmission);
scenario!(run_queued_split, QueuedSplit);
scenario!(run_queued_one_pool, QueuedOnePool);
scenario!(run_queued_too_few, QueuedTooFew);
scenario!(run_queued_leftover_refund, QueuedLeftoverRefund);

pub(super) async fn run(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
    scenario: Scenario,
) -> ScenarioResult {
    let started_at = OffsetDateTime::now_utc();
    let started = Instant::now();
    let mut steps = Steps::new();
    // Refunding an Ark escrow needs an actual funding payment and a registered destination.
    // Check both before creating a competition or requesting a ticket.
    let setup = run_step("prepare_user_behavior", || async {
        let config = if config.seed.is_some()
            && config.planned_scenario.as_deref() == Some(scenario.name())
            && config.entry_plan.len() == config.users
        {
            ensure!(
                config
                    .entry_plan
                    .iter()
                    .enumerate()
                    .all(|(index, plan)| plan.user_index == index),
                "saved plan has invalid user indexes"
            );
            config.clone()
        } else {
            config.resolve_plan(scenario.name())?
        };
        let lnd = if scenario.refunds() || scenario.queued() {
            super::escrow_refund::refund_address(&config)?;
            Some(super::escrow_refund::payer(&config)?)
        } else {
            config.lnd.as_ref().map(Lnd::new).transpose()?
        };
        Ok((config, lnd))
    })
    .await;
    let (config, lnd) = match setup {
        Ok((step, prepared)) => {
            steps.push(step);
            prepared
        }
        Err(step) => {
            steps.push(*step);
            return finish_result(scenario.name(), started_at, started, steps, true);
        }
    };
    let payer = lnd.as_ref().map_or(Payer::TestEndpoint, Payer::Lnd);
    let failed = match run_steps(client, db, &config, scenario, &payer, &mut steps).await {
        Ok(()) => false,
        Err(step) => {
            steps.push(*step);
            true
        }
    };
    finish_result(scenario.name(), started_at, started, steps, failed)
}

async fn run_steps(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
    scenario: Scenario,
    payer: &Payer<'_>,
    steps: &mut Steps,
) -> std::result::Result<(), Box<StepResult>> {
    let arrival_anchor = Instant::now();
    let queue = super::queued::QueueShape::of(scenario.name(), config).map_err(|error| {
        Box::new(StepResult {
            name: "create_competition".into(),
            status: StepStatus::Failed,
            duration_ms: 0,
            details: None,
            error: Some(format!("{error:#}")),
        })
    })?;
    let created = run_step(REFUSED_AS_SMALL_STEP, || async {
        if let Some(shape) = &queue {
            super::queued::create_queue(client, config, shape).await
        } else if scenario == Scenario::EscrowRefund {
            super::escrow_refund::create_competition(client, config).await
        } else {
            full_lifecycle::create_competition(client, config).await
        }
    })
    .await;
    let (mut created, competition_id) = match created {
        // Fees rose since the run drew its players; the scheduler draws again. Not a failure.
        Err(mut step)
            if step
                .error
                .as_deref()
                .is_some_and(|error| error.contains("A competition needs at least")) =>
        {
            step.status = StepStatus::Skipped;
            step.details = Some(serde_json::json!({ "players": config.users }));
            steps.push(*step);
            return Ok(());
        }
        created => created?,
    };
    created.details = Some(serde_json::json!({ "competition_id": competition_id }));
    steps.push(created);
    let (deadline_step, deadline) = run_step("entry_deadline", || async {
        if let Some(shape) = &queue {
            super::queued::check_queue(client, &competition_id, shape).await?;
        }
        let competition = client.get_competition(&competition_id).await?;
        let value = competition
            .event_submission
            .get("start_observation_date")
            .and_then(|value| value.as_str())
            .context("Competition omitted its entry deadline")?;
        OffsetDateTime::parse(value, &Rfc3339).context("Invalid competition entry deadline")
    })
    .await?;
    steps.push(deadline_step);
    let extra = usize::from(matches!(
        scenario,
        Scenario::AbandonedUnpaid | Scenario::PaidAbandonment
    ));
    let (loaded, users) = run_step("load_users", || load_users(db, config.users + extra)).await?;
    steps.push(loaded);

    // A backfilled run's early players enter now, and the others wait for the backfill.
    let drawn = config.entry_plan.len();
    let early = config
        .backfill
        .map_or(drawn, |backfill| backfill.early_players.min(drawn));
    let actor = |index: usize| {
        let plan = &config.entry_plan[index];
        run_actor(
            client,
            &users[plan.user_index],
            &users,
            &competition_id,
            config,
            payer,
            plan,
            arrival_anchor,
            deadline,
        )
    };
    let mut actors: FuturesUnordered<_> = (0..early).map(actor).collect();
    let backfill = async {
        match config.backfill {
            Some(backfill) => {
                backfill_step(client, &competition_id, backfill, deadline, drawn - early).await
            }
            None => std::future::pending().await,
        }
    };
    tokio::pin!(backfill);
    let mut backfilling = config.backfill.is_some();
    let mut entered = early;
    let mut traces = Vec::new();
    let mut failure = None;
    // A failed actor must not cancel another actor in the middle of paying.
    loop {
        tokio::select! {
            Some((step, trace)) = actors.next(), if !actors.is_empty() => {
                if step.status == StepStatus::Failed {
                    failure = Some(step.error.clone().unwrap_or_default());
                }
                steps.push(step);
                traces.push(trace);
            }
            (step, entering) = &mut backfill, if backfilling => {
                backfilling = false;
                steps.push(step);
                actors.extend((early..early + entering).map(actor));
                entered += entering;
            }
            else => break,
        }
    }
    // A backfilled queue expects the players who entered, not every drawn one.
    let queue = queue.map(|shape| match config.backfill {
        Some(_) => super::queued::QueueShape {
            players: entered,
            ..shape
        },
        None => shape,
    });
    if let Some(error) = failure {
        return Err(Box::new(StepResult {
            name: "entry_wave".into(),
            status: StepStatus::Failed,
            duration_ms: 0,
            details: None,
            error: Some(error),
        }));
    }

    let abandoner = traces.iter().find(|trace| {
        matches!(
            trace.behavior,
            Some(EntryBehavior::AbandonUnpaid | EntryBehavior::AbandonPaid)
        )
    });
    if let Some(abandoner) = abandoner.filter(|trace| trace.seat_taken) {
        // Other players filled the competition before the abandoning player got a seat, so
        // there is no abandoned ticket to follow.
        steps.push(abandoner.attach(StepResult {
            name: format!("user_{}_replacement", users[config.users].name),
            status: StepStatus::Skipped,
            duration_ms: 0,
            details: None,
            error: None,
        }));
    } else if scenario == Scenario::AbandonedUnpaid {
        let abandoned = abandoner.expect("resolved abandonment plan");
        let abandoner_user = users
            .iter()
            .find(|user| user.name == abandoned.user)
            .expect("abandoning player is a loaded user");
        let user = &users[config.users];
        let name = format!("user_{}_enter", user.name);
        let mut trace = planned_trace(
            user,
            &EntryPlan {
                user_index: config.users,
                arrival_secs: 0,
                before_payment_secs: 0,
                before_submit_secs: 0,
                behavior: EntryBehavior::Complete,
            },
        );
        trace.waits.push(EntryWait {
            stage: "reservation_release".into(),
            planned_ms: 600_000,
            elapsed_ms: None,
        });
        let result = run_step(&name, || async {
            crate::runner::step_progress(&name, serde_json::to_value(&trace)?).await?;
            let waiting = Instant::now();
            loop {
                ensure_payment_time(deadline, config)?;
                match full_lifecycle::request_entry(
                    client,
                    user,
                    &competition_id,
                    config.lightning_address.as_deref(),
                    &name,
                    &mut trace,
                )
                .await
                {
                    Ok(requested) => {
                        // The coordinator frees an abandoned seat when its swap expires, and a
                        // late player may take it first. What must hold is that the abandoned
                        // invoice pays for nothing.
                        if Some(requested.ticket.ticket_id) != abandoned.ticket_id {
                            log::info!(
                                "abandoned seat {:?} was reclaimed by another player before \
                                 replacement {} got ticket {}",
                                abandoned.ticket_id,
                                user.name,
                                requested.ticket.ticket_id
                            );
                        }
                        ensure!(
                            Some(&requested.ticket.payment_hash) != abandoned.payment_hash.as_ref(),
                            "recycled unpaid ticket retained its old invoice hash"
                        );
                        trace.waits[3].elapsed_ms = Some(elapsed_ms(waiting));
                        let prepared = full_lifecycle::register_entry(
                            client,
                            user,
                            &competition_id,
                            config,
                            config.users,
                            requested,
                            &name,
                            &mut trace,
                        )
                        .await?;
                        ensure_payment_time(deadline, config)?;
                        full_lifecycle::pay_entry(
                            client,
                            user,
                            &competition_id,
                            &prepared,
                            payer,
                            &name,
                            &mut trace,
                        )
                        .await?;
                        ensure_abandoned_released(
                            client,
                            abandoner_user,
                            &competition_id,
                            abandoned,
                        )
                        .await?;
                        ensure_submission_time(deadline, config)?;
                        return full_lifecycle::submit_entry(
                            client, user, &prepared, &name, &mut trace,
                        )
                        .await;
                    }
                    Err(error)
                        if error
                            .downcast_ref::<ApiRejection>()
                            .is_some_and(ApiRejection::is_no_capacity) =>
                    {
                        // Another player reclaimed the abandoned seat and paid for it first.
                        if client
                            .get_competition(&competition_id)
                            .await?
                            .seats_all_paid()
                        {
                            trace.seat_taken = true;
                            crate::runner::step_progress(&name, serde_json::to_value(&trace)?)
                                .await?;
                            return Ok(());
                        }
                        crate::runner::step_progress(&name, serde_json::to_value(&trace)?).await?;
                        tokio::time::sleep(Duration::from_secs(config.poll_interval_secs.max(1)))
                            .await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await;
        let step = match result {
            Ok((mut step, ())) => {
                if trace.seat_taken {
                    step.status = StepStatus::Skipped;
                }
                trace.attach(step)
            }
            Err(step) => {
                return Err(Box::new(trace.attach(*step)));
            }
        };
        steps.push(step);
        traces.push(trace);
    } else if scenario == Scenario::PaidAbandonment {
        let user = &users[config.users];
        let name = format!("user_{}_replacement_blocked", user.name);
        let mut trace = EntryTrace::new(user);
        trace.payment_started = Some(false);
        let abandoned = traces
            .iter()
            .find(|trace| trace.behavior == Some(EntryBehavior::AbandonPaid))
            .expect("resolved abandonment plan");
        let wait_ms = reservation_hold_ms(
            abandoned
                .ticket_requested_at
                .expect("abandoned ticket timestamp"),
            OffsetDateTime::now_utc(),
        );
        trace.waits.push(EntryWait {
            stage: "paid_reservation_hold".into(),
            planned_ms: wait_ms,
            elapsed_ms: None,
        });
        let result = run_step(&name, || async {
            wait_until(&name, &mut trace, 0, Instant::now() + Duration::from_millis(wait_ms)).await?;
            ensure_payment_time(deadline, config)?;
            let result = full_lifecycle::request_entry(client, user, &competition_id, config.lightning_address.as_deref(), &name, &mut trace).await;
            match result {
                Err(error) if error.downcast_ref::<ApiRejection>().is_some_and(ApiRejection::is_no_capacity) => Ok(()),
                Err(error) => Err(error),
                Ok(_) => anyhow::bail!("paid abandoned ticket unexpectedly released capacity; replacement was not paid"),
            }
        }).await;
        match result {
            Ok((step, ())) => steps.push(trace.attach(step)),
            Err(step) => return Err(Box::new(trace.attach(*step))),
        }
    }

    if let Some(shape) = &queue {
        return super::queued::after_entries(
            client,
            &users,
            &competition_id,
            config,
            shape,
            deadline,
            &traces,
            steps,
        )
        .await;
    }

    if scenario.refunds() {
        let mut cancellation = config.clone();
        cancellation.state_timeout_secs = cancellation_budget_secs(
            config.state_timeout_secs,
            deadline,
            OffsetDateTime::now_utc(),
        );
        let ours = traces.iter().filter(|trace| trace.paid).count() as u64;
        let (mut step, filled) = run_step("wait_cancelled", || {
            wait_cancelled_or_filled(client, &competition_id, &cancellation, ours)
        })
        .await?;
        if !filled {
            steps.push(step);
            return collect_refunds(client, &users, &competition_id, config, &traces, steps).await;
        }
        // Other players took the seats this scenario left empty, so the competition runs.
        step.status = StepStatus::Skipped;
        step.details = Some(serde_json::json!({ "filled_by_other_players": true }));
        steps.push(step);
    }
    follow_lifecycle(client, &users, &competition_id, config, &traces, steps).await
}

/// Wait until `backfill.before_close_secs` before `deadline`, then work out how many of the
/// `waiting` players the competition still needs to start, counting everyone's paid entries, and
/// return the step and how many enter. A competition that cannot be read gets them all.
async fn backfill_step(
    client: &CoordinatorClient,
    competition_id: &Uuid,
    backfill: Backfill,
    deadline: OffsetDateTime,
    waiting: usize,
) -> (StepResult, usize) {
    let at = deadline - time::Duration::seconds(backfill.before_close_secs as i64);
    let wait = (at - OffsetDateTime::now_utc()).max(time::Duration::ZERO);
    tokio::time::sleep(wait.unsigned_abs()).await;
    let counted = run_step("backfill", || async {
        let competition = client.get_competition(competition_id).await?;
        Ok(BackfillCount::new(
            competition.min_players(),
            backfill.margin,
            competition.paid_entries(),
            waiting,
        ))
    })
    .await;
    match counted {
        Ok((mut step, count)) => {
            if count.short() {
                log::warn!(
                    "Competition {competition_id} needs {} more players but the run drew only {} \
                     more; its pool may still be short",
                    count.needed,
                    count.waiting
                );
            }
            crate::server::metrics::record_backfill(count.entering);
            step.details = Some(serde_json::to_value(count).unwrap_or_default());
            (step, count.entering)
        }
        Err(step) => {
            log::warn!(
                "Cannot count competition {competition_id}'s entries for its backfill; all {waiting} \
                 waiting players enter: {}",
                step.error.as_deref().unwrap_or_default()
            );
            crate::server::metrics::record_backfill(waiting);
            let mut step = *step;
            step.status = StepStatus::Passed;
            step.details = Some(serde_json::json!({
                "waiting": waiting,
                "entering": waiting,
                "uncounted": step.error.take(),
            }));
            (step, waiting)
        }
    }
}

/// The states a single competition passes through on its way to its attestation.
const LIFECYCLE: [&str; 9] = [
    "collecting_entries",
    "escrow_confirmed",
    "event_created",
    "entries_submitted",
    "contract_created",
    "signing_complete",
    "funding_broadcasted",
    "funding_confirmed",
    "awaiting_attestation",
];

/// Follow a single competition to its attestation. A competition may instead be cancelled for
/// what synth's players did not do: a kickoff check failed on fees that rose after the run drew
/// its players, or other players held or paid for seats and never entered. Then every paid
/// ticket's refund is collected instead.
async fn follow_lifecycle(
    client: &CoordinatorClient,
    users: &[SynthUser],
    competition_id: &Uuid,
    config: &ScenarioConfig,
    traces: &[EntryTrace],
    steps: &mut Steps,
) -> std::result::Result<(), Box<StepResult>> {
    for state in LIFECYCLE {
        match run_step(&format!("wait_{state}"), || {
            wait_for_state(client, competition_id, state, config)
        })
        .await
        {
            Ok((step, ())) => steps.push(step),
            Err(failed) => {
                let competition = client.get_competition(competition_id).await.ok();
                let Some(reason) = competition
                    .as_ref()
                    .and_then(|competition| expected_cancellation(competition, traces))
                else {
                    return Err(failed);
                };
                steps.push(StepResult {
                    name: "wait_cancelled".into(),
                    status: StepStatus::Passed,
                    duration_ms: failed.duration_ms,
                    details: Some(serde_json::json!({ "reason": reason })),
                    error: None,
                });
                return collect_refunds(client, users, competition_id, config, traces, steps).await;
            }
        }
    }
    Ok(())
}

/// Why a competition that stopped short of its attestation did so for reasons outside synth's
/// players, or None if synth has nothing to blame but the coordinator.
pub(super) fn expected_cancellation(
    competition: &CompetitionResponse,
    traces: &[EntryTrace],
) -> Option<&'static str> {
    if competition.cancelled_at.is_none() && competition.failed_at.is_none() {
        return None;
    }
    if competition.failed_kickoff() {
        return Some("its kickoff check failed at the network fees then");
    }
    let ours = traces.iter().filter(|trace| trace.paid).count() as u64;
    let others =
        traces.iter().any(|trace| trace.seat_taken) || competition.total_paid_entries > ours;
    (competition.cancelled_at.is_some() && others)
        .then_some("other players held seats and never entered")
}

/// Wait for a competition expected to close unfilled to be cancelled: false. True if it filled
/// instead, when other players paid for the seats synth's `ours` paid entries left empty.
async fn wait_cancelled_or_filled(
    client: &CoordinatorClient,
    competition_id: &Uuid,
    config: &ScenarioConfig,
    ours: u64,
) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(config.state_timeout_secs);
    loop {
        let competition = client.get_competition(competition_id).await?;
        if competition.cancelled_at.is_some() {
            return Ok(false);
        }
        ensure!(
            competition.failed_at.is_none(),
            "Competition failed while waiting to be cancelled"
        );
        if competition.seats_all_paid() && competition.total_paid_entries > ours {
            return Ok(true);
        }
        ensure!(
            Instant::now() < deadline,
            "Timeout waiting for state: cancelled"
        );
        tokio::time::sleep(Duration::from_secs(config.poll_interval_secs)).await;
    }
}

/// As the player who abandoned it, check the abandoned ticket is no longer reserved: the
/// coordinator released it, whoever holds the seat now.
async fn ensure_abandoned_released(
    client: &CoordinatorClient,
    abandoner: &SynthUser,
    competition_id: &Uuid,
    abandoned: &EntryTrace,
) -> Result<()> {
    let Some(ticket_id) = abandoned.ticket_id else {
        return Ok(());
    };
    match client
        .check_ticket_status(&abandoner.nostr_keys, competition_id, &ticket_id)
        .await
    {
        Ok(TicketStatus::Reserved) => {
            anyhow::bail!("abandoned ticket {ticket_id} is still reserved by its abandoner")
        }
        Ok(_) => Ok(()),
        // A released ticket is no longer the abandoner's to look up.
        Err(error) if format!("{error:#}").contains("not reserved by this user") => Ok(()),
        Err(error) => Err(error.context("Failed to check the abandoned ticket")),
    }
}

fn reservation_hold_ms(requested_at: OffsetDateTime, now: OffsetDateTime) -> u64 {
    (requested_at + time::Duration::seconds(601) - now)
        .whole_milliseconds()
        .max(0) as u64
}

pub(super) fn cancellation_budget_secs(
    timeout: u64,
    deadline: OffsetDateTime,
    now: OffsetDateTime,
) -> u64 {
    timeout.saturating_add((deadline - now).whole_seconds().max(0) as u64 + 1)
}

pub(super) async fn collect_refunds(
    client: &CoordinatorClient,
    users: &[SynthUser],
    competition_id: &Uuid,
    config: &ScenarioConfig,
    traces: &[EntryTrace],
    steps: &mut Steps,
) -> std::result::Result<(), Box<StepResult>> {
    // One absent entry prevents the competition filling. Every paid ticket needs
    // a refund, including tickets that were never submitted. Check them all even
    // if one request fails, preserving each result for the restart tracker.
    let mut refunds = FuturesUnordered::new();
    for trace in traces.iter().filter(|trace| trace.paid) {
        let user = users
            .iter()
            .find(|user| user.name == trace.user)
            .expect("scenario user");
        let ticket = trace.ticket_id.expect("paid ticket");
        let refund_at = trace.escrow.map(|escrow| escrow.refund_at);
        refunds.push(async move {
            let (mut step, refund) = run_step(&format!("refund_{}", user.name), || {
                super::escrow_refund::wait_for_refund(
                    client,
                    user,
                    competition_id,
                    &ticket,
                    refund_at,
                    config,
                )
            })
            .await?;
            step.details = Some(refund);
            Ok::<_, Box<StepResult>>(step)
        });
    }
    let mut failed = 0;
    while let Some(result) = refunds.next().await {
        match result {
            Ok(step) => steps.push(step),
            Err(step) => {
                failed += 1;
                steps.push(*step);
            }
        }
    }
    if failed > 0 {
        return Err(Box::new(StepResult {
            name: "verify_refunds".into(),
            status: StepStatus::Failed,
            duration_ms: 0,
            details: None,
            error: Some(format!(
                "{failed} paid ticket refunds could not be confirmed"
            )),
        }));
    }
    Ok(())
}

fn planned_trace(user: &SynthUser, plan: &EntryPlan) -> EntryTrace {
    let mut trace = EntryTrace::new(user);
    trace.behavior = Some(plan.behavior);
    trace.payment_started = Some(false);
    trace.waits = [
        ("arrival", plan.arrival_secs),
        ("before_payment", plan.before_payment_secs),
        ("before_submit", plan.before_submit_secs),
    ]
    .into_iter()
    .map(|(stage, seconds)| EntryWait {
        stage: stage.into(),
        planned_ms: seconds.saturating_mul(1000),
        elapsed_ms: None,
    })
    .collect();
    trace
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

async fn wait_until(
    step: &str,
    trace: &mut EntryTrace,
    index: usize,
    target: Instant,
) -> Result<()> {
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    let start = Instant::now();
    tokio::time::sleep(target.saturating_duration_since(start)).await;
    trace.waits[index].elapsed_ms = Some(elapsed_ms(start));
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    Ok(())
}

fn ensure_payment_time(deadline: OffsetDateTime, config: &ScenarioConfig) -> Result<()> {
    let margin = config.entry_timing.deadline_margin_secs.max(60);
    ensure!(
        OffsetDateTime::now_utc() < deadline - time::Duration::seconds(margin as i64),
        "planned user missed the safe invoice payment deadline; no payment sent"
    );
    Ok(())
}

fn ensure_submission_time(deadline: OffsetDateTime, config: &ScenarioConfig) -> Result<()> {
    ensure!(
        OffsetDateTime::now_utc()
            < deadline - time::Duration::seconds(config.entry_timing.deadline_margin_secs as i64),
        "planned user missed the entry submission margin"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_actor(
    client: &CoordinatorClient,
    user: &SynthUser,
    players: &[SynthUser],
    competition_id: &Uuid,
    config: &ScenarioConfig,
    payer: &Payer<'_>,
    plan: &EntryPlan,
    arrival_anchor: Instant,
    deadline: OffsetDateTime,
) -> (StepResult, EntryTrace) {
    let name = format!("user_{}_enter", user.name);
    let mut trace = planned_trace(user, plan);
    if plan.behavior == EntryBehavior::LateSubmission {
        let target = deadline + time::Duration::milliseconds(250);
        trace.submission_not_before = Some(target);
        let other_waits = plan
            .arrival_secs
            .saturating_add(plan.before_payment_secs)
            .saturating_add(plan.before_submit_secs)
            .saturating_mul(1000);
        let planned_ms = (target - OffsetDateTime::now_utc())
            .whole_milliseconds()
            .max(0) as u64;
        trace.waits.push(EntryWait {
            stage: "after_entry_close".into(),
            planned_ms: planned_ms.saturating_sub(other_waits),
            elapsed_ms: None,
        });
    }
    let result = run_step(&name, || async {
        wait_until(
            &name,
            &mut trace,
            0,
            arrival_anchor + Duration::from_secs(plan.arrival_secs),
        )
        .await?;
        ensure_payment_time(deadline, config)?;
        let Some(requested) = request_seat(
            client,
            user,
            players,
            competition_id,
            config,
            deadline,
            &name,
            &mut trace,
        )
        .await?
        else {
            return Ok(());
        };
        if plan.behavior == EntryBehavior::AbandonUnpaid {
            return Ok(());
        }
        let prepared = full_lifecycle::register_entry(
            client,
            user,
            competition_id,
            config,
            plan.user_index,
            requested,
            &name,
            &mut trace,
        )
        .await?;
        if config
            .planned_scenario
            .as_deref()
            .is_some_and(super::queued::is_queued)
        {
            ensure!(
                prepared.queued,
                "the queued competition's ticket asked for a single competition's contract terms"
            );
        }
        wait_until(
            &name,
            &mut trace,
            1,
            Instant::now() + Duration::from_secs(plan.before_payment_secs),
        )
        .await?;
        ensure_payment_time(deadline, config)?;
        full_lifecycle::pay_entry(
            client,
            user,
            competition_id,
            &prepared,
            payer,
            &name,
            &mut trace,
        )
        .await?;
        if plan.behavior == EntryBehavior::AbandonPaid {
            return Ok(());
        }
        finish_entry(
            client,
            user,
            competition_id,
            config,
            &prepared,
            plan,
            deadline,
            &name,
            &mut trace,
        )
        .await
    })
    .await;
    let step = match result {
        Ok((mut step, ())) => {
            if trace.seat_taken {
                step.status = StepStatus::Skipped;
                if trace.outside_entries.is_some_and(|outside| outside > 0) {
                    step.error = Some("seat taken by outside player".into());
                }
            }
            step
        }
        Err(step) => *step,
    };
    (trace.attach(step), trace)
}

/// Request a ticket for `user`, as a real player would: while every seat is held, keep trying
/// until one frees up. Other people can enter synth's competitions too, so a seat may never come:
/// None, with the trace's `seat_taken` set, once others have paid for every seat, the
/// competition closed to entries with some of them outside synth's `players`, or the invoice
/// deadline passes before one frees up.
#[allow(clippy::too_many_arguments)]
async fn request_seat(
    client: &CoordinatorClient,
    user: &SynthUser,
    players: &[SynthUser],
    competition_id: &Uuid,
    config: &ScenarioConfig,
    deadline: OffsetDateTime,
    step: &str,
    trace: &mut EntryTrace,
) -> Result<Option<full_lifecycle::RequestedEntry>> {
    loop {
        match full_lifecycle::request_entry(
            client,
            user,
            competition_id,
            config.lightning_address.as_deref(),
            step,
            trace,
        )
        .await
        {
            Ok(requested) => return Ok(Some(requested)),
            Err(error)
                if error
                    .downcast_ref::<ApiRejection>()
                    .is_some_and(ApiRejection::is_no_capacity) => {}
            // Synth sizes a competition to its players, so it closes early only if someone
            // else took a seat.
            Err(error)
                if error
                    .downcast_ref::<ApiRejection>()
                    .is_some_and(ApiRejection::is_entries_closed) =>
            {
                let outside = outside_entries(client, players, competition_id).await?;
                trace.outside_entries = Some(outside);
                if outside == 0 {
                    return Err(error);
                }
                trace.seat_taken = true;
                crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        let full = client
            .get_competition(competition_id)
            .await?
            .seats_all_paid();
        if full || ensure_payment_time(deadline, config).is_err() {
            if full {
                trace.outside_entries = outside_entries(client, players, competition_id).await.ok();
            }
            trace.seat_taken = true;
            crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_secs(config.poll_interval_secs.max(1))).await;
    }
}

/// How many of the competition's entries synth's `players` did not make.
async fn outside_entries(
    client: &CoordinatorClient,
    players: &[SynthUser],
    competition_id: &Uuid,
) -> Result<u64> {
    let total = client.get_competition(competition_id).await?.total_entries;
    let mut ours = std::collections::BTreeSet::new();
    for player in players {
        for entry in client
            .list_entries(&player.nostr_keys, Some(competition_id))
            .await?
        {
            if entry.event_id == *competition_id {
                ours.insert(entry.id);
            }
        }
    }
    Ok(total.saturating_sub(ours.len() as u64))
}

#[allow(clippy::too_many_arguments)]
async fn finish_entry(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    config: &ScenarioConfig,
    prepared: &PreparedEntry,
    plan: &EntryPlan,
    deadline: OffsetDateTime,
    step: &str,
    trace: &mut EntryTrace,
) -> Result<()> {
    wait_until(
        step,
        trace,
        2,
        Instant::now() + Duration::from_secs(plan.before_submit_secs),
    )
    .await?;
    if plan.behavior == EntryBehavior::LateSubmission {
        let target = trace
            .submission_not_before
            .context("late entry omitted its planned deadline")?;
        let remaining = (target - OffsetDateTime::now_utc())
            .whole_milliseconds()
            .max(0) as u64;
        wait_until(
            step,
            trace,
            3,
            Instant::now() + Duration::from_millis(remaining),
        )
        .await?;
        return assert_rejected_submission(
            client,
            user,
            competition_id,
            prepared,
            step,
            trace,
            false,
        )
        .await;
    }
    ensure_submission_time(deadline, config)?;
    if plan.behavior == EntryBehavior::DuplicateSubmission {
        // Double-clicks send the same body twice. Drain both responses and preserve
        // the accepted entry even when the other request has an unexpected failure.
        trace.submission_attempts += 2;
        crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
        let (first, second) = tokio::join!(
            client.attempt_submit_entry(&user.nostr_keys, &prepared.entry),
            client.attempt_submit_entry(&user.nostr_keys, &prepared.entry),
        );
        let mut accepted = 0;
        let mut rejections = Vec::new();
        let mut errors = Vec::new();
        for response in [first, second] {
            match response {
                Ok(EntrySubmission::Accepted(entry)) => {
                    accepted += 1;
                    trace.entry_id = Some(entry.id);
                    trace.entry_submitted = true;
                }
                Ok(EntrySubmission::Rejected(error)) => {
                    trace.rejected_submission = Some(error.clone());
                    rejections.push(error);
                }
                Err(error) => errors.push(error),
            }
        }
        crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
        ensure!(
            errors.is_empty(),
            "duplicate request transport failure: {:?}",
            errors
        );
        ensure!(
            accepted == 1 && rejections.len() == 1,
            "double submission returned {accepted} acceptances and {} rejections",
            rejections.len()
        );
        ensure!(
            expected_rejection(&rejections[0], true),
            "unexpected duplicate rejection: {}",
            rejections[0]
        );
        // A later retry must also stay rejected and leave just one stored entry.
        return assert_rejected_submission(
            client,
            user,
            competition_id,
            prepared,
            step,
            trace,
            true,
        )
        .await;
    }
    full_lifecycle::submit_entry(client, user, prepared, step, trace).await
}

fn expected_rejection(rejection: &ApiRejection, duplicate: bool) -> bool {
    rejection.status == 400
        && (rejection.message == "Competition is no longer accepting entries"
            || (duplicate && rejection.message == "Ticket has already been used"))
}

async fn assert_rejected_submission(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    prepared: &PreparedEntry,
    step: &str,
    trace: &mut EntryTrace,
    duplicate: bool,
) -> Result<()> {
    trace.submission_attempts += 1;
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    let rejection = match client
        .attempt_submit_entry(&user.nostr_keys, &prepared.entry)
        .await?
    {
        EntrySubmission::Accepted(entry) => {
            trace.entry_id = Some(entry.id);
            trace.entry_submitted = true;
            crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
            anyhow::bail!(
                "{} submission unexpectedly accepted",
                if duplicate { "duplicate" } else { "late" }
            );
        }
        EntrySubmission::Rejected(rejection) => rejection,
    };
    trace.rejected_submission = Some(rejection.clone());
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    ensure!(
        expected_rejection(&rejection, duplicate),
        "unexpected rejection: {rejection}"
    );
    let entries = client
        .list_entries(&user.nostr_keys, Some(competition_id))
        .await?;
    let count = entries
        .iter()
        .filter(|entry| entry.ticket_id == prepared.ticket.ticket_id)
        .count();
    ensure!(
        count == usize::from(duplicate),
        "expected {} accepted entries for this ticket; found {count}",
        usize::from(duplicate)
    );
    Ok(())
}

#[cfg(test)]
#[path = "user_behavior_tests.rs"]
mod tests;
