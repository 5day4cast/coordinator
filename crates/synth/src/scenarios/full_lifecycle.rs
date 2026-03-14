use crate::client::competitions::CreateCompetition;
use crate::client::entries::{AddEntry, ValueOption, WeatherChoices};
use crate::client::CoordinatorClient;
use crate::crypto;
use crate::crypto::keys::SynthUser;
use crate::db::SynthDb;
use anyhow::{Context, Result};
use log::{error, info, warn};
use rand::Rng;
use std::time::Instant;
use time::OffsetDateTime;
use uuid::Uuid;

use super::types::*;

/// Run the full competition lifecycle scenario:
/// 1. Create competition
/// 2. Generate/load synthetic users
/// 3. Each user: request ticket → settle → submit entry
/// 4. Wait for state transitions through to completion
pub async fn run_full_lifecycle(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
) -> ScenarioResult {
    let started_at = OffsetDateTime::now_utc();
    let scenario_start = Instant::now();
    let mut steps = Vec::new();
    // Step 1: Create competition
    let comp_id = match run_step("create_competition", || async {
        create_competition(client, config).await
    })
    .await
    {
        Ok((step, comp_id)) => {
            steps.push(step);
            comp_id
        }
        Err(step) => {
            steps.push(step);
            return finish_result("full_lifecycle", started_at, scenario_start, steps, true);
        }
    };
    info!("Created competition: {}", comp_id);

    // Step 2: Load/create synthetic users
    let users = match run_step("load_users", || async {
        load_users(db, config.users).await
    })
    .await
    {
        Ok((step, users)) => {
            steps.push(step);
            users
        }
        Err(step) => {
            steps.push(step);
            return finish_result("full_lifecycle", started_at, scenario_start, steps, true);
        }
    };

    info!("Loaded {} synthetic users", users.len());

    // Step 3: Each user requests a ticket and submits an entry
    for (i, user) in users.iter().enumerate() {
        let step_name = format!("user_{}_enter", user.name);
        match run_step(&step_name, || async {
            enter_competition(client, user, &comp_id, config, i as u32).await
        })
        .await
        {
            Ok((step, _)) => steps.push(step),
            Err(step) => {
                steps.push(step);
                return finish_result("full_lifecycle", started_at, scenario_start, steps, true);
            }
        }
        info!("User {} entered competition", user.name);
    }

    // Step 4: Wait for state transitions
    let target_states = vec![
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

    for target_state in &target_states {
        let step_name = format!("wait_{}", target_state);
        match run_step(&step_name, || async {
            wait_for_state(client, &comp_id, target_state, config).await
        })
        .await
        {
            Ok((step, _)) => {
                steps.push(step);
                info!("Competition reached state: {}", target_state);
            }
            Err(step) => {
                steps.push(step);
                warn!(
                    "Competition did not reach state: {} - stopping at current state",
                    target_state
                );
                return finish_result("full_lifecycle", started_at, scenario_start, steps, true);
            }
        }
    }

    // Step 5: Wait for completion (this may take longer due to oracle attestation)
    match run_step("wait_completed", || async {
        wait_for_state(client, &comp_id, "completed", config).await
    })
    .await
    {
        Ok((step, _)) => {
            steps.push(step);
            info!("Competition completed successfully!");
        }
        Err(step) => {
            steps.push(step);
            warn!("Competition did not reach completed state within timeout");
            return finish_result("full_lifecycle", started_at, scenario_start, steps, true);
        }
    }

    finish_result("full_lifecycle", started_at, scenario_start, steps, false)
}

async fn create_competition(client: &CoordinatorClient, config: &ScenarioConfig) -> Result<Uuid> {
    let now = OffsetDateTime::now_utc();
    let observation_window = time::Duration::seconds(config.observation_window_secs as i64);

    // start_observation_date must be far enough in the future for ticket purchases
    // (coordinator requires start_observation_date - 1min > now for ticket expiry)
    let entry_window = time::Duration::seconds(120);
    let competition = CreateCompetition {
        id: Uuid::now_v7(),
        signing_date: now + entry_window + observation_window + time::Duration::seconds(60),
        start_observation_date: now + entry_window,
        end_observation_date: now + entry_window + observation_window,
        locations: config.stations.clone(),
        number_of_values_per_entry: config.stations.len() * 3,
        number_of_places_win: 1.min(config.users),
        total_allowed_entries: config.users,
        entry_fee: config.entry_fee,
        coordinator_fee_percentage: 10,
        total_competition_pool: config.entry_fee * config.users,
    };

    let resp = client.create_competition(&competition).await?;
    Ok(resp.id)
}

async fn load_users(db: &SynthDb, count: usize) -> Result<Vec<SynthUser>> {
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

async fn enter_competition(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    config: &ScenarioConfig,
    entry_index: u32,
) -> Result<()> {
    // Derive ephemeral key for this entry
    let ephemeral = user.derive_ephemeral_key(entry_index)?;

    // Request ticket
    let ticket = client
        .request_ticket(&user.nostr_keys, competition_id, &ephemeral.public_key)
        .await
        .context("Failed to request ticket")?;

    info!(
        "  {} got ticket {} ({}sats)",
        user.name, ticket.ticket_id, ticket.amount_sats
    );

    // Settle invoice via test endpoint
    client
        .test_settle_invoice(&ticket.ticket_id)
        .await
        .context("Failed to settle invoice")?;

    info!("  {} invoice settled", user.name);

    // Generate payout preimage/hash
    let (payout_preimage, payout_hash) =
        crypto::payout::generate_payout_pair(&ephemeral.secret_bytes);

    // Encrypt ephemeral private key and payout preimage to user's nostr key
    let ephemeral_encrypted = user
        .nip44_encrypt_to_self(&ephemeral.private_key_hex)
        .await?;
    let preimage_encrypted = user.nip44_encrypt_to_self(&payout_preimage).await?;

    // Prepare keymeld data if session info is available
    let (encrypted_keymeld_key, keymeld_auth_pubkey) =
        if let (Some(session_id), Some(enclave_pubkey)) = (
            &ticket.keymeld_session_id,
            &ticket.keymeld_enclave_public_key,
        ) {
            let encrypted =
                crypto::keymeld::encrypt_for_enclave(&ephemeral.private_key_hex, enclave_pubkey)?;
            let auth_pubkey =
                crypto::keymeld::derive_auth_pubkey(&ephemeral.private_key_hex, session_id)?;
            (Some(encrypted), Some(auth_pubkey))
        } else {
            (None, None)
        };

    // Generate random weather predictions
    let predictions = generate_random_predictions(&config.stations);

    // Submit entry
    let entry = AddEntry {
        id: Uuid::now_v7(),
        ticket_id: ticket.ticket_id,
        ephemeral_pubkey: ephemeral.public_key,
        ephemeral_privatekey_encrypted: ephemeral_encrypted,
        payout_hash,
        payout_preimage_encrypted: preimage_encrypted,
        event_id: *competition_id,
        expected_observations: predictions,
        encrypted_keymeld_private_key: encrypted_keymeld_key,
        keymeld_auth_pubkey,
    };

    client
        .submit_entry(&user.nostr_keys, &entry)
        .await
        .context("Failed to submit entry")?;

    info!("  {} entry submitted", user.name);
    Ok(())
}

fn generate_random_predictions(stations: &[String]) -> Vec<WeatherChoices> {
    let mut rng = rand::rng();
    stations
        .iter()
        .map(|station| {
            let mut pick = || match rng.random_range(0..3u8) {
                0 => Some(ValueOption::Over),
                1 => Some(ValueOption::Par),
                _ => Some(ValueOption::Under),
            };
            WeatherChoices {
                stations: station.clone(),
                wind_speed: pick(),
                temp_high: pick(),
                temp_low: pick(),
            }
        })
        .collect()
}

async fn wait_for_state(
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

/// Helper to run a step with timing
async fn run_step<F, Fut, T>(name: &str, f: F) -> std::result::Result<(StepResult, T), StepResult>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
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
            Err(StepResult {
                name: name.to_string(),
                status: StepStatus::Failed,
                duration_ms: duration,
                details: None,
                error: Some(e.to_string()),
            })
        }
    }
}

fn finish_result(
    scenario: &str,
    started_at: OffsetDateTime,
    start: Instant,
    steps: Vec<StepResult>,
    failed: bool,
) -> ScenarioResult {
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
