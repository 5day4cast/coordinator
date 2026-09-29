use crate::client::competitions::CreateCompetition;
use crate::client::entries::{
    AddEntry, TicketRegistration, TicketStatus, ValueOption, WeatherChoices,
};
use crate::client::CoordinatorClient;
use crate::crypto;
use crate::crypto::keys::SynthUser;
use crate::db::SynthDb;
use crate::lnd::Lnd;
use crate::trail::{EntryPayment, EntryTrace, EscrowTerms, RouteHop};
use anyhow::{Context, Result};
use log::warn;
use rand::Rng;
use time::OffsetDateTime;
use uuid::Uuid;

use super::types::*;

/// Run staggered players through the funded competition lifecycle.
pub async fn run_full_lifecycle(
    client: &CoordinatorClient,
    db: &SynthDb,
    config: &ScenarioConfig,
) -> ScenarioResult {
    super::user_behavior::run(
        client,
        db,
        config,
        super::user_behavior::Scenario::FullLifecycle,
    )
    .await
}

pub(super) async fn create_competition(
    client: &CoordinatorClient,
    config: &ScenarioConfig,
) -> Result<Uuid> {
    // start_observation_date must be far enough in the future for ticket purchases
    // (coordinator requires start_observation_date - 1min > now for ticket expiry)
    let times = config.competition_times(OffsetDateTime::now_utc());
    let competition = CreateCompetition {
        id: times.id,
        signing_date: times.signing,
        start_observation_date: times.start,
        end_observation_date: times.end,
        locations: config.stations.clone(),
        number_of_values_per_entry: config.values_per_entry(),
        number_of_places_win: 1.min(config.users),
        total_allowed_entries: config.users,
        entry_fee: config.entry_fee,
        coordinator_fee_basis_points: 1000,
        coordinator_fee_percentage: 10,
        total_competition_pool: config.entry_fee * config.users,
        // A test competition: kept off the oracle's public events list.
        unlisted: true,
    };

    let resp = client.create_competition(&competition).await?;
    Ok(resp.id)
}

/// How a scenario pays an entry's invoice.
pub(super) enum Payer<'a> {
    /// The coordinator settles it for us. Cannot fund an Arkade escrow, which ark-swapd pays.
    TestEndpoint,
    /// A real payment, which ark-swapd swaps into the entry's escrow.
    Lnd(&'a crate::lnd::Lnd),
}

/// A ticket and its private registration material, held only until registration.
pub(super) struct RequestedEntry {
    pub ticket: crate::client::entries::TicketResponse,
    entry_id: Uuid,
    ephemeral: crate::crypto::keys::EphemeralKey,
    payout_preimage: String,
    payout_choice: coordinator_core::PayoutRegistrationRequest,
}

/// The exact entry body can be retried to test duplicate submission safely.
pub(super) struct PreparedEntry {
    pub ticket: crate::client::entries::TicketResponse,
    pub entry: AddEntry,
    /// The ticket's policy is a queued competition's template, not a concrete contract.
    pub queued: bool,
}

pub(super) async fn request_entry(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    lightning_address: Option<&str>,
    step: &str,
    trace: &mut EntryTrace,
) -> Result<RequestedEntry> {
    let entry_id = Uuid::now_v7();
    trace.entry_id = Some(entry_id);
    let ephemeral = user.derive_ephemeral_key(&entry_id)?;
    let (payout_preimage, payout_hash) =
        crypto::payout::generate_payout_pair(&ephemeral.secret_bytes);
    let payout_choice = coordinator_core::PayoutRegistrationRequest {
        entry_id,
        payout_hash,
        lightning_address: lightning_address.map(str::to_string),
        allow_invoice_fallback: true,
        release_entry_key_after_payment: true,
    };
    let ticket = client
        .request_ticket(
            &user.nostr_keys,
            competition_id,
            &ephemeral.public_key,
            Some(payout_choice.clone()),
        )
        .await
        .context("Failed to request ticket")?;
    ticket.check_price()?;
    trace.ticket_requested_at = Some(OffsetDateTime::now_utc());
    trace.ticket_id = Some(ticket.ticket_id);
    trace.amount_sats = Some(ticket.amount_sats);
    trace.payment_hash = Some(ticket.payment_hash.clone());
    trace.invoice = Some(ticket.payment_request.clone());
    trace.lightning_address = lightning_address.map(str::to_string);
    trace.escrow = escrow_terms(&ticket);
    trace.payment_started = Some(false);
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    Ok(RequestedEntry {
        ticket,
        entry_id,
        ephemeral,
        payout_preimage,
        payout_choice,
    })
}

// Keep registration's protocol context explicit, matching the surrounding actor helpers.
#[allow(clippy::too_many_arguments)]
pub(super) async fn register_entry(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    config: &ScenarioConfig,
    user_index: usize,
    requested: RequestedEntry,
    step: &str,
    trace: &mut EntryTrace,
) -> Result<PreparedEntry> {
    let RequestedEntry {
        ticket,
        entry_id,
        ephemeral,
        payout_preimage,
        payout_choice,
    } = requested;
    if ticket.keymeld_session_id.is_some() && ticket.keymeld_registration.is_none() {
        anyhow::bail!("Ticket is missing authorized Keymeld registration context");
    }
    let (registration, consent) = match &ticket.keymeld_registration {
        Some(assignment) => {
            let prepared = crypto::keymeld::prepare_for_ticket(
                &ephemeral.private_key_hex,
                assignment,
                &payout_choice,
                *competition_id,
                ticket.ticket_id,
                &ticket.payment_hash,
                &payout_preimage,
            )
            .await?;
            (Some(prepared.registration), Some(prepared.consent))
        }
        None => (None, None),
    };
    // A queued entry is its ticket: it is submitted under the ticket's id.
    let entry_id = consent.map_or(entry_id, |consent| consent.entry_id);
    trace.entry_id = Some(entry_id);
    // Refund authorization must reach the coordinator before any payment leaves.
    if let Some(data) = &registration {
        client
            .register_ticket(
                &user.nostr_keys,
                competition_id,
                &ticket.ticket_id,
                &TicketRegistration {
                    ephemeral_pubkey: ephemeral.public_key.clone(),
                    encrypted_keymeld_private_key: data.encrypted_private_key.clone(),
                    keymeld_auth_pubkey: data.auth_pubkey.clone(),
                    keymeld_registration_context: data.context.clone(),
                    keymeld_escrow_policy: data.escrow_policy.clone(),
                },
            )
            .await
            .context("Failed to register the ticket before paying")?;
        trace.ticket_registered = true;
    }
    let (
        encrypted_keymeld_private_key,
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
    let entry = AddEntry {
        id: entry_id,
        ticket_id: ticket.ticket_id,
        ephemeral_pubkey: ephemeral.public_key,
        payout_hash: payout_choice.payout_hash,
        event_id: *competition_id,
        expected_observations: generate_predictions(
            &config.stations,
            config.seed,
            user_index,
            config.window_shape(),
        ),
        encrypted_keymeld_private_key,
        keymeld_auth_pubkey,
        keymeld_registration_context,
        keymeld_escrow_policy,
    };
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    Ok(PreparedEntry {
        ticket,
        entry,
        queued: consent.is_some_and(|consent| consent.queued),
    })
}

pub(super) async fn pay_entry(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    prepared: &PreparedEntry,
    payer: &Payer<'_>,
    step: &str,
    trace: &mut EntryTrace,
) -> Result<()> {
    trace.payment_started = Some(true);
    // Persist both the ticket and the intent before payment: a restart can distinguish
    // an unpaid dropout from an interrupted Lightning request.
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    match payer {
        Payer::TestEndpoint => {
            client
                .test_settle_invoice(&prepared.ticket.ticket_id)
                .await
                .context("Failed to settle invoice")?;
            trace.settled_by_test_endpoint = true;
        }
        Payer::Lnd(lnd) => match lnd.pay(&prepared.ticket.payment_request).await {
            Ok(paid) => trace.payment = Some(entry_payment(lnd, paid).await),
            // The entry invoice is a hold invoice, in flight until the coordinator settles it.
            // The ticket status below decides whether the coordinator holds the payment.
            Err(e) if crate::lnd::stream_cut_short(&e) => {
                warn!(
                    "entry payment for ticket {} still in flight when its stream ended: {e:#}",
                    prepared.ticket.ticket_id
                );
            }
            Err(e) => return Err(e.context("Failed to pay the entry invoice")),
        },
    }
    trace.paid = true;
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    for attempt in 0..=30 {
        let status = client
            .check_ticket_status(&user.nostr_keys, competition_id, &prepared.ticket.ticket_id)
            .await
            .context("Failed to check ticket status")?;
        if status == TicketStatus::Paid || status == TicketStatus::Settled {
            return Ok(());
        }
        if attempt == 30 {
            anyhow::bail!(
                "Ticket {} still not paid after settle (status: {:?})",
                prepared.ticket.ticket_id,
                status
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    unreachable!()
}

pub(super) async fn submit_entry(
    client: &CoordinatorClient,
    user: &SynthUser,
    prepared: &PreparedEntry,
    step: &str,
    trace: &mut EntryTrace,
) -> Result<()> {
    trace.submission_attempts += 1;
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    let response = client
        .submit_entry(&user.nostr_keys, &prepared.entry)
        .await
        .context("Failed to submit entry")?;
    trace.entry_id = Some(response.id);
    trace.entry_submitted = true;
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    Ok(())
}

/// The terms of the ticket's Arkade escrow, from the payout policy the player signs. None for a
/// ticket without one.
fn escrow_terms(ticket: &crate::client::entries::TicketResponse) -> Option<EscrowTerms> {
    let policy = ticket
        .keymeld_registration
        .as_ref()?
        .payout_policy
        .as_deref()?;
    let policy: coordinator_core::keymeld::PayoutPolicy = serde_json::from_str(policy).ok()?;
    EscrowTerms::from_tap_tree(&policy.ark_escrow?.escrow_tap_tree)
}

/// How `lnd` paid an entry, naming the nodes it went through. A name it cannot look up is left
/// out rather than failing an entry that went through.
async fn entry_payment(lnd: &Lnd, paid: crate::lnd::Paid) -> EntryPayment {
    let payer = lnd.identity().await.unwrap_or_else(|e| {
        warn!("Cannot name the paying node: {e:#}");
        crate::lnd::NodeIdentity::default()
    });
    let mut route = Vec::new();
    for hop in paid.route {
        let alias = lnd.alias_of(&hop.pub_key).await.ok().flatten();
        route.push(RouteHop {
            chan_id: hop.chan_id,
            pubkey: hop.pub_key,
            alias,
        });
    }
    EntryPayment {
        payer_alias: payer.alias,
        payer_pubkey: payer.pubkey,
        preimage: paid.preimage,
        fee_msat: paid.fee_msat,
        route,
    }
}

/// A player's picks: over, par or under for each station and metric `shape` scores. Every
/// metric is drawn whatever the window, so a seed replays the same picks.
pub(super) fn generate_predictions(
    stations: &[String],
    seed: Option<u64>,
    user_index: usize,
    shape: WindowShape,
) -> Vec<WeatherChoices> {
    let [high, low, wind] = shape.scores();
    use rand::SeedableRng;
    // Separate each user's picks from the timing RNG and from every other user.
    let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(
        seed.unwrap_or(0) ^ (user_index as u64).wrapping_mul(0x9e3779b97f4a7c15),
    );
    stations
        .iter()
        .map(|station| {
            let mut pick = || match rng.random_range(0..3u8) {
                0 => Some(ValueOption::Over),
                1 => Some(ValueOption::Par),
                _ => Some(ValueOption::Under),
            };
            let (wind_speed, temp_high, temp_low) = (pick(), pick(), pick());
            WeatherChoices {
                stations: station.clone(),
                wind_speed: wind_speed.filter(|_| wind),
                temp_high: temp_high.filter(|_| high),
                temp_low: temp_low.filter(|_| low),
            }
        })
        .collect()
}
