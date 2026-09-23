//! A competition that never fills must give every buy-in back.
//!
//! 1. Create a competition with room for more players than will enter, so it cannot fill.
//! 2. Enter with the players it has, each paying for real and naming where a refund goes.
//! 3. Wait for the competition to be cancelled, once its event expires unfilled.
//! 4. Wait for every ticket's refund to settle.
//!
//! This is the whole Arkade path end to end: a Lightning payment swapped into an escrow VTXO, an
//! escrow refunded into a swap after its locktime, Keymeld signing as the player, and the swap
//! service paying the player's Lightning Address.
//!
//! It needs two things a lifecycle run does not. Entries must be paid for real, because
//! `ark-swapd` funds the escrow from the payment and the coordinator holds no invoice to settle
//! itself. And the players need a Lightning Address that resolves publicly, because the enclave
//! checks a refund's invoice against the address's own provider before signing anything.

use std::time::Instant;

use anyhow::{Context, Result};
use log::info;
use time::OffsetDateTime;
use uuid::Uuid;

use super::common::{finish_result, load_users, run_step, wait_for_state};
use super::full_lifecycle::{enter_competition_with, Payer};
use super::types::*;
use crate::client::competitions::CreateCompetition;
use crate::client::CoordinatorClient;
use crate::crypto::keys::SynthUser;
use crate::db::SynthDb;
use crate::lnd::Lnd;

const SCENARIO: &str = "escrow_refund";

pub async fn run_escrow_refund(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
) -> ScenarioResult {
    let started_at = OffsetDateTime::now_utc();
    let scenario_start = Instant::now();
    let mut steps = Vec::new();

    macro_rules! step {
        ($name:expr, $work:expr) => {
            match run_step($name, || async { $work }).await {
                Ok((step, value)) => {
                    steps.push(step);
                    value
                }
                Err(step) => {
                    steps.push(*step);
                    return finish_result(SCENARIO, started_at, scenario_start, steps, true);
                }
            }
        };
    }

    let address = step!("refund_address", refund_address(config));
    let lnd = step!("open_payer", payer(config));
    let comp_id = step!(
        "create_unfillable_competition",
        create_competition(client, config).await
    );
    info!("Created competition {comp_id}, which cannot fill");
    let users = step!("load_users", load_users(db, config.users).await);

    let mut tickets = Vec::new();
    for user in &users {
        let ticket = step!(
            &format!("user_{}_enter", user.name),
            enter(client, user, &comp_id, config, &address, &lnd).await
        );
        tickets.push((user.clone(), ticket));
    }

    step!(
        "wait_cancelled",
        wait_for_state(client, &comp_id, "cancelled", config).await
    );
    for (user, ticket) in &tickets {
        step!(
            &format!("refund_{}", user.name),
            wait_for_refund(client, user, &comp_id, ticket, config).await
        );
    }

    finish_result(SCENARIO, started_at, scenario_start, steps, false)
}

fn refund_address(config: &ScenarioConfig) -> Result<String> {
    config.refund_lightning_address.clone().context(
        "set refund_lightning_address: a refund pays the player's own address, and the \
         enclave resolves it before signing",
    )
}

fn payer(config: &ScenarioConfig) -> Result<Lnd> {
    let lnd = config.lnd.as_ref().context(
        "set lnd: an escrow is funded by ark-swapd swapping a real payment, so entries \
         cannot be settled through the coordinator's test endpoint",
    )?;
    Lnd::new(lnd)
}

/// A competition with one more seat than the scenario fills, so it expires unfilled.
async fn create_competition(client: &CoordinatorClient, config: &ScenarioConfig) -> Result<Uuid> {
    let now = OffsetDateTime::now_utc();
    let entry_window = time::Duration::seconds(config.entry_window_secs as i64);
    let observation_window = time::Duration::seconds(config.observation_window_secs as i64);
    let seats = config.users + 1;
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
        number_of_places_win: 1,
        total_allowed_entries: seats,
        entry_fee: config.entry_fee,
        coordinator_fee_percentage: 10,
        total_competition_pool: config.entry_fee * seats,
    };
    Ok(client.create_competition(&competition).await?.id)
}

async fn enter(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    config: &ScenarioConfig,
    address: &str,
    lnd: &Lnd,
) -> Result<Uuid> {
    enter_competition_with(
        client,
        user,
        competition_id,
        config,
        Some(address),
        &Payer::Lnd(lnd),
    )
    .await
}

/// Wait for a ticket's refund to settle, which is the swap service claiming what it paid for.
async fn wait_for_refund(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    ticket_id: &Uuid,
    config: &ScenarioConfig,
) -> Result<serde_json::Value> {
    let deadline = Instant::now() + std::time::Duration::from_secs(config.refund_timeout_secs);
    let mut last = None;
    while Instant::now() < deadline {
        let refund = client
            .check_ticket_refund(&user.nostr_keys, competition_id, ticket_id)
            .await?;
        if let Some(refund) = refund {
            if refund.state == "settled" {
                info!(
                    "  {} refunded {} sats in {}",
                    user.name,
                    refund.paid_sats,
                    refund
                        .ark_txid
                        .as_deref()
                        .unwrap_or("an Arkade transaction")
                );
                return Ok(serde_json::json!({
                    "paid_sats": refund.paid_sats,
                    "ark_txid": refund.ark_txid,
                }));
            }
            last = Some(refund.state);
        }
        tokio::time::sleep(std::time::Duration::from_secs(config.poll_interval_secs)).await;
    }
    anyhow::bail!(
        "the refund of {}'s ticket stopped at {}",
        user.name,
        last.as_deref().unwrap_or("nothing")
    )
}
