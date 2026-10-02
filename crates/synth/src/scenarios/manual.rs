//! A competition an operator asked for, from the dashboard's form or `synth run
//! manual-competition`. It is made the way the scenarios make theirs and recorded as a run, so its
//! money is followed like theirs. Asked for players, synth's players enter it over its entry
//! window as a lifecycle run's do; asked for none, it is left open for people.

use std::time::Instant;

use anyhow::Result;
use futures::{stream::FuturesUnordered, StreamExt};
use time::OffsetDateTime;

use super::common::{entry_deadline, finish_result, load_users, run_step, Steps};
use super::full_lifecycle::{self, Payer};
use super::types::*;
use super::user_behavior;
use crate::client::CoordinatorClient;
use crate::db::SynthDb;
use crate::lnd::Lnd;

pub const MANUAL_COMPETITION: &str = "manual_competition";

pub async fn run_manual_competition(
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
    finish_result(MANUAL_COMPETITION, started_at, started, steps, failed)
}

/// The seats a manual competition has: as asked, else one per player.
pub fn seats(config: &ScenarioConfig) -> usize {
    config.seats.unwrap_or(config.users)
}

async fn run_steps(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
    steps: &mut Steps,
) -> std::result::Result<(), Box<StepResult>> {
    let arrival_anchor = Instant::now();
    // Synth's players pay from the node when one is configured; check it before creating.
    let lnd = match config.users {
        0 => None,
        _ => {
            let (step, lnd) = run_step("prepare_players", || async {
                config.lnd.as_ref().map(Lnd::new).transpose()
            })
            .await?;
            steps.push(step);
            lnd
        }
    };
    let (mut created, competition_id) = run_step(REFUSED_AS_SMALL_STEP, || {
        full_lifecycle::create_single(client, config, seats(config), !config.listed)
    })
    .await?;
    created.details = Some(serde_json::json!({
        "competition_id": competition_id,
        "seats": seats(config),
        "listed": config.listed,
        "players": config.users,
    }));
    steps.push(created);
    if config.users == 0 {
        return Ok(());
    }

    let (deadline_step, deadline) =
        run_step("entry_deadline", || entry_deadline(client, &competition_id)).await?;
    steps.push(deadline_step);
    let (loaded, users) = run_step("load_users", || load_users(db, config.users)).await?;
    steps.push(loaded);
    let payer = lnd.as_ref().map_or(Payer::TestEndpoint, Payer::Lnd);
    let mut actors: FuturesUnordered<_> = config
        .entry_plan
        .iter()
        .map(|plan| {
            user_behavior::run_actor(
                client,
                &users[plan.user_index],
                &users,
                &competition_id,
                config,
                &payer,
                plan,
                arrival_anchor,
                deadline,
            )
        })
        .collect();
    let mut failure = None;
    // A failed player must not cancel another in the middle of paying.
    while let Some((step, _)) = actors.next().await {
        if step.status == StepStatus::Failed {
            failure = step.error.clone();
        }
        steps.push(step);
    }
    match failure {
        Some(error) => Err(Box::new(StepResult {
            name: "entry_wave".into(),
            status: StepStatus::Failed,
            duration_ms: 0,
            details: None,
            error: Some(error),
        })),
        None => Ok(()),
    }
}

/// Whether the coordinator took a run's competition, once the run's creation step has finished:
/// Err holds what it said. For a caller that waits to tell the operator.
pub fn created(steps: &[crate::db::TestStep]) -> Option<Result<(), String>> {
    let step = steps
        .iter()
        .find(|step| step.step_name == REFUSED_AS_SMALL_STEP && step.status != "running")?;
    Some(match &step.error_message {
        Some(error) => Err(error.clone()),
        None => Ok(()),
    })
}
