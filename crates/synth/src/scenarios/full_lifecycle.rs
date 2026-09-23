use crate::client::competitions::CreateCompetition;
use crate::client::entries::{AddEntry, TicketStatus, ValueOption, WeatherChoices};
use crate::client::CoordinatorClient;
use crate::crypto;
use crate::crypto::keys::SynthUser;
use crate::db::SynthDb;
use crate::lnd::Lnd;
use anyhow::{Context, Result};
use log::{info, warn};
use rand::Rng;
use std::time::Instant;
use time::OffsetDateTime;
use uuid::Uuid;

use super::common::{finish_result, load_users, run_step, wait_for_state};
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
        Ok((mut step, comp_id)) => {
            step.details = Some(serde_json::json!({ "competition_id": comp_id }));
            steps.push(step);
            comp_id
        }
        Err(step) => {
            steps.push(*step);
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
            steps.push(*step);
            return finish_result("full_lifecycle", started_at, scenario_start, steps, true);
        }
    };

    info!("Loaded {} synthetic users", users.len());

    // An Arkade coordinator funds each escrow from a real payment, so pay for real when a node
    // is configured and fall back to the coordinator's test endpoint when not.
    let lnd = match run_step("open_payer", || async {
        config.lnd.as_ref().map(Lnd::new).transpose()
    })
    .await
    {
        Ok((step, lnd)) => {
            steps.push(step);
            lnd
        }
        Err(step) => {
            steps.push(*step);
            return finish_result("full_lifecycle", started_at, scenario_start, steps, true);
        }
    };
    let payer = lnd.as_ref().map_or(Payer::TestEndpoint, Payer::Lnd);

    // Step 3: Each user requests a ticket and submits an entry
    for user in &users {
        let step_name = format!("user_{}_enter", user.name);
        let mut trace = EntryTrace::new(user);
        let entered = run_step(&step_name, || {
            enter_competition_with(
                client,
                user,
                &comp_id,
                config,
                config.lightning_address.as_deref(),
                &payer,
                &mut trace,
            )
        })
        .await;
        match entered {
            Ok((step, _)) => steps.push(trace.attach(step)),
            Err(step) => {
                steps.push(trace.attach(*step));
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
                steps.push(*step);
                warn!(
                    "Competition did not reach state: {} - stopping at current state",
                    target_state
                );
                return finish_result("full_lifecycle", started_at, scenario_start, steps, true);
            }
        }
    }

    // Reaching awaiting_attestation means the full lifecycle (creation, entry,
    // escrow, signing, funding) succeeded.  Completion requires oracle attestation
    // which normally takes over a day, so we treat awaiting_attestation as success.
    info!("Competition reached awaiting_attestation — lifecycle test passed");

    finish_result("full_lifecycle", started_at, scenario_start, steps, false)
}

async fn create_competition(client: &CoordinatorClient, config: &ScenarioConfig) -> Result<Uuid> {
    let now = OffsetDateTime::now_utc();
    let observation_window = time::Duration::seconds(config.observation_window_secs as i64);

    // start_observation_date must be far enough in the future for ticket purchases
    // (coordinator requires start_observation_date - 1min > now for ticket expiry)
    let entry_window = time::Duration::seconds(config.entry_window_secs as i64);
    let competition = CreateCompetition {
        id: Uuid::now_v7(),
        signing_date: now
            + entry_window
            + observation_window
            + time::Duration::seconds(config.signing_delay_secs as i64),
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

/// Where an entry's money went, recorded as it happens so a failed entry still shows how far its
/// payment got.
#[derive(Debug, Default, serde::Serialize)]
pub(super) struct EntryTrace {
    pub user: String,
    pub nostr_pubkey: String,
    pub entry_id: Option<Uuid>,
    pub ticket_id: Option<Uuid>,
    pub amount_sats: Option<u64>,
    /// Identifies the payment on the paying node, the invoice's node, and in ark-swapd.
    pub payment_hash: Option<String>,
    pub paid: bool,
    pub entry_submitted: bool,
}

impl EntryTrace {
    pub(super) fn new(user: &SynthUser) -> Self {
        Self {
            user: user.name.clone(),
            nostr_pubkey: user.nostr_pubkey_hex(),
            ..Self::default()
        }
    }

    /// The step, carrying this trace as its details.
    pub(super) fn attach(&self, mut step: StepResult) -> StepResult {
        step.details = serde_json::to_value(self).ok();
        step
    }
}

/// How a scenario pays an entry's invoice.
pub(super) enum Payer<'a> {
    /// The coordinator settles it for us. Cannot fund an Arkade escrow, which ark-swapd pays.
    TestEndpoint,
    /// A real payment, which ark-swapd swaps into the entry's escrow.
    Lnd(&'a crate::lnd::Lnd),
}

/// Enter, paying with `payer` and registering `lightning_address` for payouts and refunds.
///
/// Returns the ticket, which a refund is later read from. `trace` records each stage as it
/// completes.
#[allow(clippy::too_many_arguments)]
pub(super) async fn enter_competition_with(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    config: &ScenarioConfig,
    lightning_address: Option<&str>,
    payer: &Payer<'_>,
    trace: &mut EntryTrace,
) -> Result<Uuid> {
    let entry_id = Uuid::now_v7();
    trace.entry_id = Some(entry_id);
    let ephemeral = user.derive_ephemeral_key(&entry_id)?;
    let (payout_preimage, payout_hash) =
        crypto::payout::generate_payout_pair(&ephemeral.secret_bytes);
    let payout_choice = coordinator_core::PayoutRegistrationRequest {
        entry_id,
        payout_hash: payout_hash.clone(),
        lightning_address: lightning_address.map(str::to_string),
        allow_invoice_fallback: true,
        release_entry_key_after_payment: true,
    };
    // Payouts and refunds go to the Lightning Address when one is configured, and to a
    // signed invoice from the player otherwise.
    let ticket = client
        .request_ticket(
            &user.nostr_keys,
            competition_id,
            &ephemeral.public_key,
            Some(payout_choice.clone()),
        )
        .await
        .context("Failed to request ticket")?;
    trace.ticket_id = Some(ticket.ticket_id);
    trace.amount_sats = Some(ticket.amount_sats);
    trace.payment_hash = Some(ticket.payment_hash.clone());

    info!(
        "  {} got ticket {} ({}sats)",
        user.name, ticket.ticket_id, ticket.amount_sats
    );

    if ticket.keymeld_session_id.is_some() && ticket.keymeld_registration.is_none() {
        anyhow::bail!("Ticket is missing authorized Keymeld registration context");
    }
    let registration = match &ticket.keymeld_registration {
        Some(assignment) => Some(
            crypto::keymeld::prepare_for_ticket(
                &ephemeral.private_key_hex,
                assignment,
                &payout_choice,
                *competition_id,
                &ticket.payment_hash,
                &payout_preimage,
            )
            .await?,
        ),
        None => None,
    };
    let (
        encrypted_keymeld_key,
        keymeld_auth_pubkey,
        keymeld_registration_context,
        keymeld_escrow_policy,
    ) = match registration {
        Some(data) => (
            Some(data.encrypted_private_key),
            Some(data.auth_pubkey),
            Some(data.context),
            data.escrow_policy,
        ),
        None => (None, None, None, None),
    };

    match payer {
        Payer::TestEndpoint => {
            client
                .test_settle_invoice(&ticket.ticket_id)
                .await
                .context("Failed to settle invoice")?;
            info!("  {} invoice settled", user.name);
        }
        Payer::Lnd(lnd) => {
            lnd.pay(&ticket.payment_request)
                .await
                .context("Failed to pay the entry invoice")?;
            info!("  {} paid {} sats", user.name, ticket.amount_sats);
        }
    }
    trace.paid = true;

    // Wait for ticket payment to propagate (SQLite WAL read/write pool sync)
    let mut retries = 0;
    loop {
        let status = client
            .check_ticket_status(&user.nostr_keys, competition_id, &ticket.ticket_id)
            .await
            .context("Failed to check ticket status")?;
        if status == TicketStatus::Paid || status == TicketStatus::Settled {
            break;
        }
        retries += 1;
        if retries > 30 {
            anyhow::bail!(
                "Ticket {} still not paid after settle (status: {:?})",
                ticket.ticket_id,
                status
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Generate random weather predictions
    let predictions = generate_random_predictions(&config.stations);

    // Submit entry
    let entry = AddEntry {
        id: entry_id,
        ticket_id: ticket.ticket_id,
        ephemeral_pubkey: ephemeral.public_key,
        payout_hash,
        event_id: *competition_id,
        expected_observations: predictions,
        encrypted_keymeld_private_key: encrypted_keymeld_key,
        keymeld_auth_pubkey,
        keymeld_registration_context,
        keymeld_escrow_policy,
    };

    client
        .submit_entry(&user.nostr_keys, &entry)
        .await
        .context("Failed to submit entry")?;
    trace.entry_submitted = true;

    info!("  {} entry submitted", user.name);
    Ok(ticket.ticket_id)
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
