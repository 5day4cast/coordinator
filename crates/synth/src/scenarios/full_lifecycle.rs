use crate::client::competitions::CreateCompetition;
use crate::client::entries::{
    AddEntry, TicketRegistration, TicketStatus, ValueOption, WeatherChoices,
};
use crate::client::CoordinatorClient;
use crate::crypto;
use crate::crypto::keys::SynthUser;
use crate::db::SynthDb;
use crate::lnd::{Lnd, Tracked};
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
    // A test competition: kept off the oracle's public events list.
    create_single(client, config, config.users, true).await
}

/// A single competition of `seats` seats paying one winner, as every scenario but the queued
/// ones makes it.
pub(super) async fn create_single(
    client: &CoordinatorClient,
    config: &ScenarioConfig,
    seats: usize,
    unlisted: bool,
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
        number_of_places_win: 1.min(seats),
        total_allowed_entries: seats,
        entry_fee: config.entry_fee,
        coordinator_fee_basis_points: COORDINATOR_FEE_BASIS_POINTS,
        coordinator_fee_percentage: COORDINATOR_FEE_BASIS_POINTS / 100,
        total_competition_pool: config.entry_fee * seats,
        unlisted,
    };

    let resp = client.create_competition(&competition).await?;
    Ok(resp.id)
}

/// The coordinator's cut of each entry that synth's competitions ask for: 3%.
pub(super) const COORDINATOR_FEE_BASIS_POINTS: u32 = 300;

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
    expected_stake: u64,
    max_ticket_fees: u64,
}

pub(super) async fn request_entry(
    client: &CoordinatorClient,
    user: &SynthUser,
    competition_id: &Uuid,
    lightning_address: Option<&str>,
    step: &str,
    trace: &mut EntryTrace,
) -> Result<RequestedEntry> {
    let prior: Option<EntryTrace> = crate::runner::prior_step(step)
        .and_then(|row| row.details_json)
        .and_then(|json| serde_json::from_str(&json).ok());
    let entry_id = trace
        .key_derivation_id
        .or_else(|| {
            prior
                .as_ref()
                .and_then(|saved| saved.key_derivation_id.or(saved.entry_id))
        })
        .or(trace.entry_id)
        .unwrap_or_else(Uuid::now_v7);
    trace.key_derivation_id = Some(entry_id);
    trace.entry_id = Some(entry_id);
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
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
    trace.pending_submission = Some(entry.clone());
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    Ok(PreparedEntry {
        ticket,
        entry,
        queued: consent.is_some_and(|consent| consent.queued),
        expected_stake: u64::try_from(config.entry_fee).context("stake exceeds u64")?,
        max_ticket_fees: config.max_ticket_fees_sats,
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
    if let Payer::Lnd(lnd) = payer {
        trace.payment_intent = Some(prepared.ticket.payment_intent(
            prepared.expected_stake,
            prepared.max_ticket_fees,
            lnd.network().await?,
        )?);
    }
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
        Payer::Lnd(lnd) => match lnd
            .pay(
                trace
                    .payment_intent
                    .as_ref()
                    .context("validated payment intent missing")?,
            )
            .await
        {
            Ok(paid) => trace.payment = Some(entry_payment(lnd, paid).await),
            // The entry invoice is a hold invoice: ark-swapd holds the HTLC until it has paid the
            // escrow, which can outlast the stream. Follow the payment on the node until it ends.
            Err(e) if crate::lnd::stream_cut_short(&e) => {
                warn!(
                    "entry payment for ticket {} still in flight when its stream ended: {e:#}",
                    prepared.ticket.ticket_id
                );
                let paid = follow_held_payment(lnd, &prepared.ticket).await?;
                trace
                    .payment_intent
                    .as_ref()
                    .context("validated payment intent missing")?
                    .verify_paid(&paid)?;
                trace.payment = Some(entry_payment(lnd, paid).await);
            }
            Err(e) => return Err(e.context("Failed to pay the entry invoice")),
        },
    }
    trace.paid = true;
    crate::runner::step_progress(step, serde_json::to_value(&*trace)?).await?;
    // The coordinator sees the ticket paid once it finds the escrow VTXO, seconds after settling.
    for attempt in 0..=PAID_CHECKS {
        let status = client
            .check_ticket_status(&user.nostr_keys, competition_id, &prepared.ticket.ticket_id)
            .await
            .context("Failed to check ticket status")?;
        if status == TicketStatus::Paid || status == TicketStatus::Settled {
            return Ok(());
        }
        if attempt == PAID_CHECKS {
            anyhow::bail!(
                "Ticket {} still not paid after settle (status: {:?})",
                prepared.ticket.ticket_id,
                status
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    unreachable!()
}

/// Ticket status checks, a second apart, before a settled entry that is still not paid fails.
const PAID_CHECKS: u32 = 60;

/// How often a held entry payment is looked up once its stream has ended.
const HELD_PAYMENT_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// How long past its invoice's expiry a held entry payment is followed. ark-swapd cancels an
/// open invoice a minute after it expires.
const HELD_PAYMENT_GRACE_SECS: u64 = 90;

/// How long a held entry payment is followed when its invoice gives no expiry, and at most.
const HELD_PAYMENT_DEFAULT_SECS: u64 = 600;
const HELD_PAYMENT_MAX_SECS: u64 = 30 * 60;

/// Follow an entry payment on the paying node, after its stream ended, until it succeeds or
/// fails, and at the latest until its invoice has expired.
async fn follow_held_payment(
    lnd: &Lnd,
    ticket: &crate::client::entries::TicketResponse,
) -> Result<crate::lnd::Paid> {
    let until = std::time::Instant::now()
        + held_payment_budget(&ticket.payment_request, std::time::SystemTime::now());
    loop {
        // A lookup cut off by a restart of the node or a dropped connection is tried again.
        let tracked = match lnd.track(&ticket.payment_hash).await {
            Ok(tracked) => tracked,
            Err(e) if std::time::Instant::now() < until => {
                warn!(
                    "look up the entry payment for ticket {}: {e:#}",
                    ticket.ticket_id
                );
                tokio::time::sleep(HELD_PAYMENT_POLL).await;
                continue;
            }
            Err(e) => return Err(e.context("Failed to follow the entry payment")),
        };
        match tracked {
            Tracked::Succeeded(paid) => return Ok(paid),
            Tracked::Failed => anyhow::bail!(
                "entry payment for ticket {} failed after its stream ended",
                ticket.ticket_id
            ),
            _ if std::time::Instant::now() < until => tokio::time::sleep(HELD_PAYMENT_POLL).await,
            other => anyhow::bail!(
                "entry payment for ticket {} still {other:?} after its invoice expired",
                ticket.ticket_id
            ),
        }
    }
}

/// How long to follow a held payment of `payment_request` from `now`: until the invoice has
/// expired, with a grace for the service to cancel it.
fn held_payment_budget(payment_request: &str, now: std::time::SystemTime) -> std::time::Duration {
    let expires_at = payment_request
        .parse::<lightning_invoice::Bolt11Invoice>()
        .ok()
        .and_then(|invoice| invoice.expires_at());
    let left = match expires_at {
        Some(at) => at.saturating_sub(
            now.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default(),
        ),
        None => std::time::Duration::from_secs(HELD_PAYMENT_DEFAULT_SECS),
    };
    left.min(std::time::Duration::from_secs(HELD_PAYMENT_MAX_SECS))
        + std::time::Duration::from_secs(HELD_PAYMENT_GRACE_SECS)
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
    trace.pending_submission = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use dlctix::bitcoin::hashes::{sha256, Hash};
    use dlctix::bitcoin::secp256k1::{Secp256k1, SecretKey};
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    use std::time::{Duration, UNIX_EPOCH};

    /// A signed regtest invoice made at `created` (unix seconds) that expires `expiry` later.
    fn invoice(created: u64, expiry: u64) -> String {
        let secp = Secp256k1::new();
        let node_key = SecretKey::from_slice(&[0x11; 32]).unwrap();
        InvoiceBuilder::new(Currency::Regtest)
            .description("entry".into())
            .payment_hash(sha256::Hash::from_byte_array([3; 32]))
            .payment_secret(PaymentSecret([7; 32]))
            .duration_since_epoch(Duration::from_secs(created))
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(expiry))
            .amount_milli_satoshis(5_000_000)
            .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &node_key))
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_held_payment_is_followed_until_its_invoice_has_expired() {
        let created = 1_790_000_000;
        let at = |secs| UNIX_EPOCH + Duration::from_secs(created + secs);
        let grace = Duration::from_secs(HELD_PAYMENT_GRACE_SECS);
        assert_eq!(
            held_payment_budget(&invoice(created, 600), at(100)),
            Duration::from_secs(500) + grace
        );
        // Past its expiry, only the grace for the service to cancel it is left.
        assert_eq!(held_payment_budget(&invoice(created, 600), at(700)), grace);
        assert_eq!(
            held_payment_budget(&invoice(created, 86_400), at(0)),
            Duration::from_secs(HELD_PAYMENT_MAX_SECS) + grace
        );
        assert_eq!(
            held_payment_budget("not an invoice", at(0)),
            Duration::from_secs(HELD_PAYMENT_DEFAULT_SECS) + grace
        );
    }
}
