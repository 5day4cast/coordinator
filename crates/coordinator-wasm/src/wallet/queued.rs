//! Consent to an entry in a queued competition.
//!
//! A queued entry names no pool, pool size or oracle event: none of them exist until kickoff.
//! The player consents to the competition's terms ([`queued::QueuedTerms`]) and to their own
//! entry, and Keymeld's verifier later derives their pool's contract from exactly those terms.
//! This check is the only one the player's own device makes, so nothing in the terms is taken
//! from the coordinator unchecked. The wallet rebuilds what they must say from the oracle's
//! reference event and key, which the page fetches from the oracle itself, and from the prices
//! and pool sizes the entry form showed.

use super::{keys::EntryKey, WalletError};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use coordinator_core::{
    keymeld::{
        ark,
        oracle_statement::{LineTerms, ObservationTerms, ScoringRules},
        pools::PoolRules,
        queued::{self, QueuedEntryTerms},
        PayoutPolicy,
    },
    RegistrationAssignment,
};
use dlctix::{
    bitcoin::{hashes::Hash, FeeRate, Network},
    secp::Point,
};
use lightning_invoice::Bolt11Invoice;
use serde::Deserialize;
use std::{str::FromStr, time::Duration};
use time::OffsetDateTime;
use uuid::Uuid;

/// Most bytes of the oracle's reference event the wallet reads.
const MAX_REFERENCE_EVENT_BYTES: usize = 1024 * 1024;
/// Most bytes of a ticket invoice the wallet parses.
const MAX_INVOICE_BYTES: usize = 16 * 1024;
/// Each pool of a queued competition pays one winner.
const PLACES: u32 = 1;

/// The player's choices and what the entry form showed, collected before the ticket's invoice is
/// displayed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedConsent {
    /// The queued competition the form is for.
    pub competition_id: Uuid,
    pub lightning_address: Option<String>,
    pub allow_invoice_fallback: bool,
    pub release_entry_key_after_payment: bool,
    pub ticket_invoice: String,
    /// The ticket price the form showed: the entry fee and the coordinator fee.
    pub ticket_amount_sats: u64,
    /// The entry fee the form showed: the player's stake in their pool's pot.
    pub entry_fee_sats: u64,
    /// The pool sizes the form showed.
    pub pool_rules: PoolRules,
    pub expected_relative_locktime_delta: u16,
    pub max_fee_rate_sat_vb: u64,
    /// `key` from the oracle's `GET /oracle/pubkey`: base64 of its compressed SEC1 public key.
    pub oracle_pubkey: String,
    /// The body of the oracle's `GET /oracle/events/{competition_id}`, as received. It stays text
    /// so each line reaches the wallet as the oracle wrote it (JavaScript would turn `-0.0`
    /// into `0`, and the terms compare lines bit for bit).
    pub reference_event: String,
}

/// What the wallet reads of a queued competition's reference event, the oracle event that fixes
/// the terms and lines every pool's event copies.
#[derive(Deserialize)]
struct ReferenceEvent {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    end_observation_date: OffsetDateTime,
    locations: Vec<String>,
    number_of_values_per_entry: u32,
    number_of_places_win: u32,
    source: String,
    scoring_fields: Vec<String>,
    scoring_rules: ScoringRules,
    #[serde(default)]
    lines: Vec<ReferenceLine>,
    event_announcement: ReferenceAnnouncement,
}

#[derive(Deserialize)]
struct ReferenceLine {
    target: String,
    metric: String,
    lower: f64,
    upper: f64,
    window_hours: u32,
}

#[derive(Deserialize)]
struct ReferenceAnnouncement {
    expiry: Option<u32>,
}

impl ReferenceEvent {
    /// The observation terms the oracle will state for a pool copied from this event, built as
    /// the oracle builds a statement: dates in UNIX seconds rounded down, the locations as
    /// targets, and the lines sorted by target, then metric.
    fn observation(&self) -> ObservationTerms {
        let mut lines: Vec<LineTerms> = self
            .lines
            .iter()
            .map(|line| LineTerms {
                target: line.target.clone(),
                metric: line.metric.clone(),
                lower: line.lower,
                upper: line.upper,
                window_hours: line.window_hours,
            })
            .collect();
        lines.sort_by(|a, b| (&a.target, &a.metric).cmp(&(&b.target, &b.metric)));
        ObservationTerms {
            source: self.source.clone(),
            start_observation_date: self.start_observation_date.unix_timestamp(),
            end_observation_date: self.end_observation_date.unix_timestamp(),
            targets: self.locations.clone(),
            scoring_fields: self.scoring_fields.clone(),
            number_of_values_per_entry: self.number_of_values_per_entry,
            scoring_rules: self.scoring_rules,
            lines,
        }
    }
}

/// The x-only key in the oracle's `GET /oracle/pubkey` answer: base64 of a compressed SEC1 key.
fn oracle_xonly_key(base64_sec1: &str) -> Option<[u8; 32]> {
    let bytes = BASE64.decode(base64_sec1).ok()?;
    if bytes.len() != 33 {
        return None;
    }
    Some(Point::from_slice(&bytes).ok()?.serialize_xonly())
}

/// Check a queued entry's payout policy against the competition, its oracle and the ticket
/// before anything is sealed to Keymeld or the invoice is shown.
pub(super) fn validate_registration(
    network: Network,
    key: &EntryKey,
    entry_id: Uuid,
    assignment: &RegistrationAssignment,
    consent: &QueuedConsent,
) -> Result<(), WalletError> {
    let reject = || {
        WalletError::Keymeld(
            "The entry's terms differ from the competition, its oracle event or the ticket".into(),
        )
    };
    let policy: PayoutPolicy =
        serde_json::from_str(assignment.payout_policy.as_deref().ok_or_else(reject)?)
            .map_err(|_| reject())?;
    // Refuses concrete contract terms, a missing escrow or sellback consent, and invalid terms.
    let entry = QueuedEntryTerms::from_policy(&policy)
        .ok()
        .flatten()
        .ok_or_else(reject)?;
    let terms = &entry.terms;
    let escrow = policy.ark_escrow.as_ref().ok_or_else(reject)?;
    if consent.ticket_invoice.len() > MAX_INVOICE_BYTES
        || consent.reference_event.len() > MAX_REFERENCE_EVENT_BYTES
    {
        return Err(reject());
    }
    let invoice = Bolt11Invoice::from_str(&consent.ticket_invoice).map_err(|_| reject())?;
    let event: ReferenceEvent =
        serde_json::from_str(&consent.reference_event).map_err(|_| reject())?;
    let oracle = oracle_xonly_key(&consent.oracle_pubkey).ok_or_else(reject)?;
    let max_fee = FeeRate::from_sat_per_vb(consent.max_fee_rate_sat_vb).ok_or_else(reject)?;
    let (deposit_session, deposit_digest) = queued::deposit_scope(terms).map_err(|_| reject())?;
    let now = Duration::from_secs(::nostr::Timestamp::now().as_secs());
    // The competition, and the terms its oracle event fixes for every pool.
    let competition = terms.competition_id == consent.competition_id
        && event.id == consent.competition_id
        && terms.network == network
        && terms
            .oracle_key()
            .is_ok_and(|terms_oracle| terms_oracle.serialize() == oracle)
        && terms.observation.same_as(&event.observation())
        && terms.signing_date == event.signing_date.unix_timestamp()
        && event.event_announcement.expiry == Some(terms.expiry)
        && terms.number_of_places_win == PLACES
        && event.number_of_places_win == PLACES;
    // What the form showed.
    let shown = terms.stake_sats == consent.entry_fee_sats
        && terms.pool_rules == consent.pool_rules
        && terms.relative_locktime_block_delta == consent.expected_relative_locktime_delta
        && max_fee != FeeRate::ZERO
        && terms.max_fee_rate <= max_fee;
    // This wallet's entry, and the ticket it pays for at the price shown.
    let ticket = entry.entry_id == entry_id
        && entry.payout_hash == key.payout_hash()
        && entry.ticket_hash == invoice.payment_hash().to_byte_array()
        && invoice.network() == network
        && !invoice.would_expire(now)
        && consent.ticket_amount_sats > 0
        && invoice
            .amount_milli_satoshis()
            .is_some_and(|msat| Some(msat) == consent.ticket_amount_sats.checked_mul(1000));
    // The player's choices.
    let choices = consent.release_entry_key_after_payment
        && policy.release_entry_key_after_payment
        && policy.automatic_lightning_address == consent.lightning_address
        && policy.allow_invoice_fallback == consent.allow_invoice_fallback;
    // The Keymeld deposit: sealed under the competition and these terms' digest, for this entry's
    // ticket, whose id is the entry's.
    let deposit = assignment.session_id == deposit_session.as_string()
        && assignment.manifest_hash == deposit_digest
        && assignment.user_id == entry_id;
    if !(competition && shown && ticket && choices && deposit) {
        return Err(reject());
    }
    // The escrow holds this entry's key, funds only the market maker's pools, and refunds by the
    // latest expiry any pool may have.
    let xonly = |point: Point| {
        ark::XOnlyPublicKey::from_slice(&point.serialize_xonly()).map_err(|_| reject())
    };
    ark::consented_escrow(
        escrow,
        xonly(key.point())?,
        xonly(terms.market_maker.pubkey)?,
        Some(terms.expiry),
    )
    .map_err(|_| reject())?;
    // The escrow pays the stake into the pool and at most the coordinator's fee, together no more
    // than the ticket. A refund costs no more than entering did, no more than the stake, and never
    // the whole ticket.
    let spent = escrow
        .max_fee_sats
        .checked_add(terms.stake_sats)
        .ok_or_else(reject)?;
    if spent > consent.ticket_amount_sats
        || escrow.max_refund_fee_sats > escrow.max_fee_sats
        || escrow.max_refund_fee_sats > terms.stake_sats
        || escrow.max_refund_fee_sats >= consent.ticket_amount_sats
    {
        return Err(reject());
    }
    Ok(())
}

#[cfg(test)]
#[path = "queued_tests.rs"]
mod tests;
