//! What every scenario does the same way: run a step, wait for a state, finish a result.

use std::time::Instant;

use anyhow::Result;
use log::error;
use time::OffsetDateTime;
use uuid::Uuid;

use super::types::*;
use crate::client::CoordinatorClient;
use crate::crypto::keys::SynthUser;
use crate::db::SynthDb;

/// Load or create `count` synthetic users, by name.
pub(super) async fn load_users(db: &SynthDb, count: usize) -> Result<Vec<SynthUser>> {
    let names = [
        "alice", "bob", "charlie", "dave", "eve", "frank", "grace", "heidi",
    ];
    let mut users = Vec::new();

    for i in 0..count {
        let name = if i < names.len() {
            names[i].to_string()
        } else {
            format!("user_{}", i)
        };

        let record = db.get_or_create_user(&name).await?;
        let user = SynthUser::from_secret_key(&record.name, &record.nostr_secret_key)?;
        users.push(user);
    }

    Ok(users)
}

pub(super) async fn wait_for_state(
    client: &CoordinatorClient,
    competition_id: &Uuid,
    target_state: &str,
    config: &ScenarioConfig,
) -> Result<()> {
    let deadline = Instant::now() + std::time::Duration::from_secs(config.state_timeout_secs);

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("Timeout waiting for state: {}", target_state);
        }

        let comp = client.get_competition(competition_id).await?;
        let current = comp.inferred_status();

        if current == target_state || is_past_state(current, target_state) {
            return Ok(());
        }

        if current == "failed" || current == "cancelled" {
            anyhow::bail!(
                "Competition entered terminal state '{}' while waiting for '{}'",
                current,
                target_state
            );
        }

        tokio::time::sleep(std::time::Duration::from_secs(config.poll_interval_secs)).await;
    }
}

/// Check if `current` is a later state than `target` in the lifecycle
fn is_past_state(current: &str, target: &str) -> bool {
    let order = [
        "created",
        "collecting_entries",
        "escrow_confirmed",
        "event_created",
        "entries_submitted",
        "contract_created",
        "signing_complete",
        "funding_broadcasted",
        "funding_confirmed",
        "funding_settled",
        "awaiting_attestation",
        "outcome_broadcasted",
        "delta_broadcasted",
        "completed",
    ];

    let current_idx = order.iter().position(|s| *s == current);
    let target_idx = order.iter().position(|s| *s == target);

    match (current_idx, target_idx) {
        (Some(c), Some(t)) => c > t,
        _ => false,
    }
}

/// A scenario's steps. Each is saved and announced as it is pushed, so a run can be watched while
/// it goes; set a step's details before pushing it.
#[derive(Default)]
pub(super) struct Steps(Vec<StepResult>);

impl Steps {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn push(&mut self, step: StepResult) {
        crate::runner::step_finished(&step);
        self.0.push(step);
    }
}

/// Helper to run a step with timing
pub(super) async fn run_step<F, Fut, T>(
    name: &str,
    f: F,
) -> std::result::Result<(StepResult, T), Box<StepResult>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    crate::runner::step_started(name);
    let start = Instant::now();
    match f().await {
        Ok(value) => {
            let duration = start.elapsed().as_millis() as i64;
            Ok((
                StepResult {
                    name: name.to_string(),
                    status: StepStatus::Passed,
                    duration_ms: duration,
                    details: None,
                    error: None,
                },
                value,
            ))
        }
        Err(e) => {
            let duration = start.elapsed().as_millis() as i64;
            error!("Step '{}' failed: {:?}", name, e);
            Err(Box::new(StepResult {
                name: name.to_string(),
                status: StepStatus::Failed,
                duration_ms: duration,
                details: None,
                error: Some(e.to_string()),
            }))
        }
    }
}

pub(super) fn finish_result(
    scenario: &str,
    started_at: OffsetDateTime,
    start: Instant,
    steps: Steps,
    failed: bool,
) -> ScenarioResult {
    let steps = steps.0;
    let total_duration_ms = start.elapsed().as_millis() as i64;
    let error = if failed {
        steps
            .iter()
            .rev()
            .find(|s| s.status == StepStatus::Failed)
            .and_then(|s| s.error.clone())
    } else {
        None
    };

    ScenarioResult {
        scenario: scenario.to_string(),
        status: if failed {
            ScenarioStatus::Failed
        } else {
            ScenarioStatus::Passed
        },
        steps,
        total_duration_ms,
        started_at,
        completed_at: Some(OffsetDateTime::now_utc()),
        error,
    }
}
