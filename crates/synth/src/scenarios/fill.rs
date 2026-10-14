//! Fill a competition synth did not make, so the people who entered it get to play: a queue that
//! is not full starts only if it has its smallest pool, and a single competition that is not full
//! when entries close is cancelled. The fill loop ([`crate::runner::fill`]) plans the run shortly
//! before entries close, with a player for each seat empty then. When the run starts it counts
//! the empty seats again, and that many of synth's players arrive over the time left, as people
//! do. A seat someone else takes meanwhile is theirs: synth's player stands down. Synth's players
//! then follow the competition, or the queue's pools, to its attestation as any run's do.

use std::time::Instant;

use futures::{stream::FuturesUnordered, StreamExt};
use time::OffsetDateTime;

use super::common::{finish_result, load_users, run_step, Steps};
use super::full_lifecycle::Payer;
use super::types::*;
use super::{queued, user_behavior};
use crate::client::competitions::{CompetitionKind, CompetitionResponse};
use crate::client::CoordinatorClient;
use crate::db::SynthDb;

pub const FILL_COMPETITION: &str = "fill_competition";

/// The step that takes up the competition. It names the competition, so the run is known by it.
const JOIN_STEP: &str = "join_competition";

/// The seats of `competition` still empty: what a queue's cap or a single competition's seats
/// leave once paid tickets and tickets held for an unpaid invoice are counted. None for a pool,
/// whose players come from its queue, and for a queue without a cap.
pub fn empty_seats(competition: &CompetitionResponse) -> Option<u64> {
    let (seats, taken) = match competition.kind {
        CompetitionKind::Queued => (u64::from(competition.max_entries?), competition.taken()),
        CompetitionKind::Single => (
            competition.seats()?,
            competition
                .total_entries
                .max(competition.total_paid_entries),
        ),
        CompetitionKind::Pool => return None,
    };
    Some(seats.saturating_sub(taken))
}

pub async fn run_fill_competition(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
) -> ScenarioResult {
    let started_at = OffsetDateTime::now_utc();
    let started = Instant::now();
    let mut steps = Steps::new();
    let failed = run_steps(client, db, config, &mut steps)
        .await
        .map_err(|step| steps.push(*step))
        .is_err();
    finish_result(FILL_COMPETITION, started_at, started, steps, failed)
}

async fn run_steps(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
    steps: &mut Steps,
) -> std::result::Result<(), Box<StepResult>> {
    // A resumed run's players keep the arrival times they were planned at.
    let arrival_anchor = crate::runner::resumed_start().unwrap_or_else(Instant::now);
    // Every entry waits in an Arkade escrow that a real payment funds, and names the Lightning
    // Address it is refunded to if the competition never starts.
    let (step, lnd) = run_step("prepare_players", || async {
        super::escrow_refund::refund_address(config)?;
        super::escrow_refund::payer(config)
    })
    .await?;
    steps.push(step);
    let payer = Payer::Lnd(&lnd);
    let Some(competition_id) = config.competition_id else {
        return Err(Box::new(StepResult {
            name: JOIN_STEP.into(),
            status: StepStatus::Failed,
            duration_ms: 0,
            details: None,
            error: Some("the run names no competition to fill".into()),
        }));
    };
    // A resumed run counted its players before the restart, and its own entries now take seats.
    let counted = crate::runner::prior_step(JOIN_STEP)
        .filter(|row| row.status == "passed")
        .and_then(|row| {
            let step = crate::runner::resume::finished_step(&row);
            step.details.as_ref()?.get("entering")?.as_u64()
        })
        .map(|entering| (entering as usize).min(config.users));
    let (mut joined, (competition, entering)) = run_step(JOIN_STEP, || async {
        let competition = client.get_competition(&competition_id).await?;
        anyhow::ensure!(
            competition.kind != CompetitionKind::Pool,
            "competition {competition_id} is a pool, whose players come from its queue"
        );
        let entering = counted.unwrap_or_else(|| {
            let empty = if competition.open_to_enter(OffsetDateTime::now_utc()) {
                empty_seats(&competition).map_or(0, |empty| empty as usize)
            } else {
                0
            };
            empty.min(config.users)
        });
        Ok((competition, entering))
    })
    .await?;
    joined.details = Some(serde_json::json!({
        "competition_id": competition_id,
        "kind": competition.kind,
        "paid_entries": competition.paid_entries(),
        "planned": config.users,
        "entering": entering,
    }));
    if entering == 0 {
        // People took every seat, or entries closed, since the run was planned.
        joined.status = StepStatus::Skipped;
        steps.push(joined);
        return Ok(());
    }
    steps.push(joined);
    let Some(deadline) = competition.entries_close() else {
        return Err(Box::new(StepResult {
            name: "entry_deadline".into(),
            status: StepStatus::Failed,
            duration_ms: 0,
            details: None,
            error: Some(format!(
                "competition {competition_id} omitted its entry deadline"
            )),
        }));
    };
    let (loaded, users) = run_step("load_users", || load_users(db, config.users)).await?;
    steps.push(loaded);

    let players = &users;
    let mut actors: FuturesUnordered<_> = config.entry_plan[..entering]
        .iter()
        .map(|plan| {
            let user = &players[plan.user_index];
            let payer = &payer;
            async move {
                let name = format!("user_{}_enter", user.name);
                match user_behavior::resume_entry(client, user, &competition_id, config, &name)
                    .await
                {
                    Some(resumed) => resumed,
                    None => {
                        user_behavior::run_actor(
                            client,
                            user,
                            players,
                            &competition_id,
                            config,
                            payer,
                            plan,
                            arrival_anchor,
                            deadline,
                        )
                        .await
                    }
                }
            }
        })
        .collect();
    let mut traces = Vec::new();
    let mut failure = None;
    // A failed player must not cancel another in the middle of paying.
    while let Some((step, trace)) = actors.next().await {
        if step.status == StepStatus::Failed {
            failure = step.error.clone();
        }
        steps.push(step);
        traces.push(trace);
    }
    if let Some(error) = failure {
        return Err(Box::new(StepResult {
            name: "entry_wave".into(),
            status: StepStatus::Failed,
            duration_ms: 0,
            details: None,
            error: Some(error),
        }));
    }

    match (competition.kind, competition.pool_rules) {
        (CompetitionKind::Queued, Some(rules)) => {
            let shape = queued::QueueShape {
                rules,
                players: entering,
                abandoned: 0,
                max_entries: competition.max_entries,
                places: competition
                    .event_submission
                    .get("number_of_places_win")
                    .and_then(serde_json::Value::as_u64)
                    .map_or(1, |places| places as u32),
            };
            queued::after_entries(
                client,
                &users,
                &competition_id,
                config,
                &shape,
                deadline,
                &traces,
                steps,
            )
            .await
        }
        (CompetitionKind::Queued, None) => Err(Box::new(StepResult {
            name: "wait_pools_formed".into(),
            status: StepStatus::Failed,
            duration_ms: 0,
            details: None,
            error: Some(format!(
                "queue {competition_id} did not say how its pools are sized"
            )),
        })),
        _ => {
            user_behavior::follow_lifecycle(client, &users, &competition_id, config, &traces, steps)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn competition(kind: &str, extra: serde_json::Value) -> CompetitionResponse {
        let mut value = serde_json::json!({
            "id": "01a13a67-b8d9-7ab3-b0a6-680d75e5118a",
            "created_at": "2026-10-14T12:34:00Z",
            "event_submission": { "total_allowed_entries": 20 },
            "total_entries": 0,
            "total_paid_entries": 0,
            "total_paid_out_entries": 0,
            "kind": kind,
        });
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn empty_seats_count_paid_and_held_tickets() {
        let queue = competition(
            "queued",
            serde_json::json!({ "max_entries": 20, "entries": 1, "held": 3 }),
        );
        assert_eq!(empty_seats(&queue), Some(17));
        let uncapped = competition("queued", serde_json::json!({ "entries": 1 }));
        assert_eq!(empty_seats(&uncapped), None);
        let single = competition(
            "single",
            serde_json::json!({ "total_entries": 4, "total_paid_entries": 5 }),
        );
        assert_eq!(empty_seats(&single), Some(15));
        let full = competition(
            "queued",
            serde_json::json!({ "max_entries": 20, "entries": 20, "held": 20 }),
        );
        assert_eq!(empty_seats(&full), Some(0));
        assert_eq!(
            empty_seats(&competition("pool", serde_json::json!({}))),
            None
        );
    }
}
