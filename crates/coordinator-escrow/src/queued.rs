//! Queued competitions: entries without a seat count, split into pools at kickoff.
//!
//! A player consents at entry to a template: the competition's [`QueuedTerms`], the same for every
//! player, and their own entry ([`QueuedEntryTerms`]). It names no pool, pool size, oracle event
//! or player slot, because none of them exist until kickoff. The entry key is deposited with
//! Keymeld under a registration scope named by the competition and the digest of its terms
//! ([`deposit_scope`]), and registered into its pool's session at kickoff.
//!
//! At kickoff the coordinator forms the pools ([`crate::pools`]), creates one oracle event per
//! pool, and hands the verifier each pool's formation ([`DepositEvidence`]) and the oracle's signed
//! statement of the pool's event. [`pool_authorization`] then derives the member's concrete
//! [`ContractAuthorization`], which the pool's contract must match exactly, as any other entry's.
//! See `docs/QUEUED_COMPETITIONS.md`.

use crate::{
    authorization::{authorization_digest, PayoutPolicy},
    capacity::{MAX_COMPETITION_PLAYERS, MAX_COMPETITION_WINNING_PLACES},
    oracle_statement::{ObservationTerms, Outcomes, SignedStatement, Terms},
    payout::{ContractAuthorization, MAX_CONTRACT_BYTES},
    pools::{self, PoolRules},
    SessionId,
};
use dlctix::{
    bitcoin::{Amount, BlockHash, FeeRate, Network},
    musig2::secp256k1::{Parity, XOnlyPublicKey},
    secp::Point,
    EventLockingConditions, MarketMaker, Outcome, PayoutWeights,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, str::FromStr};
use uuid::Uuid;

/// Most bytes of [`DepositEvidence`]; Keymeld caps a deposit scope's evidence at 64 KiB.
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueuedError {
    #[error("invalid queued terms: {0}")]
    InvalidTerms(String),
    #[error("invalid pool formation: {0}")]
    InvalidFormation(String),
    #[error("the pool's oracle statement differs from the terms: {0}")]
    StatementMismatch(String),
}

fn terms_error(message: impl Into<String>) -> QueuedError {
    QueuedError::InvalidTerms(message.into())
}
fn formation_error(message: impl Into<String>) -> QueuedError {
    QueuedError::InvalidFormation(message.into())
}
fn statement_error(message: impl Into<String>) -> QueuedError {
    QueuedError::StatementMismatch(message.into())
}

/// What every player of a queued competition consents to. Its digest is the deposit digest of
/// every player's Keymeld registration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedTerms {
    /// The queued competition, which owns registration. Pools get their own ids at kickoff.
    pub competition_id: Uuid,
    pub network: Network,
    pub market_maker: MarketMaker,
    /// The oracle's x-only public key, hex.
    pub oracle_pubkey: String,
    /// When the oracle attests every pool's event, UNIX seconds.
    pub signing_date: i64,
    /// Every pool event's DLC expiry, UNIX seconds.
    pub expiry: u32,
    /// What every pool's event measures and how it is judged, lines included.
    pub observation: ObservationTerms,
    pub number_of_places_win: u32,
    pub pool_rules: PoolRules,
    /// Each player's share of a pool's funding value: a pool of `n` funds `n * stake_sats`.
    pub stake_sats: u64,
    pub relative_locktime_block_delta: u16,
    pub max_fee_rate: FeeRate,
}

impl QueuedTerms {
    pub fn validate(&self) -> Result<(), QueuedError> {
        self.oracle_key()?;
        if self.stake_sats == 0 || self.max_fee_rate == FeeRate::ZERO {
            return Err(terms_error("stake and fee ceiling must be positive"));
        }
        if self.network == Network::Testnet4 {
            return Err(terms_error("BOLT11 has no Testnet4 currency"));
        }
        let places = self.number_of_places_win as usize;
        if places == 0 || places > MAX_COMPETITION_WINNING_PLACES {
            return Err(terms_error(format!(
                "a pool pays 1 to {MAX_COMPETITION_WINNING_PLACES} places"
            )));
        }
        if places >= self.pool_rules.min_players() {
            return Err(terms_error("a pool needs more players than places"));
        }
        if i64::from(self.expiry) <= self.signing_date
            || self.observation.end_observation_date > self.signing_date
            || self.observation.start_observation_date >= self.observation.end_observation_date
        {
            return Err(terms_error(
                "the window must end by the signing date, before the expiry",
            ));
        }
        self.stake_sats
            .checked_mul(self.pool_rules.max_players() as u64)
            .filter(|total| Amount::from_sat(*total) <= Amount::MAX_MONEY)
            .ok_or_else(|| terms_error("a full pool's funding value overflows"))?;
        Ok(())
    }

    pub fn oracle_key(&self) -> Result<XOnlyPublicKey, QueuedError> {
        XOnlyPublicKey::from_str(&self.oracle_pubkey)
            .map_err(|_| terms_error("invalid oracle public key"))
    }

    /// The oracle key as the point its locking points are computed with. Attestations are
    /// BIP340-style, so the x-only key's even point gives the same locking points as either
    /// parity of the full key.
    pub fn oracle_point(&self) -> Result<Point, QueuedError> {
        Ok(Point::from((self.oracle_key()?, Parity::Even)))
    }

    /// The deposit digest: what each deposit is sealed under instead of a session manifest.
    pub fn digest(&self) -> Result<[u8; 32], QueuedError> {
        authorization_digest("5day4cast/queued-terms/v1", self)
            .map_err(|error| terms_error(error.to_string()))
    }
}

/// The Keymeld registration scope a queued competition's deposits are sealed under: the
/// competition id in place of a session id, and the terms' digest in place of a manifest digest.
pub fn deposit_scope(terms: &QueuedTerms) -> Result<(SessionId, [u8; 32]), QueuedError> {
    Ok((SessionId::from(terms.competition_id), terms.digest()?))
}

/// One player's consent to a queued competition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedEntryTerms {
    pub terms: QueuedTerms,
    /// The entry, which is also its ticket: the oracle ranks a pool's entries in this id order.
    pub entry_id: Uuid,
    pub ticket_hash: [u8; 32],
    pub payout_hash: [u8; 32],
}

impl QueuedEntryTerms {
    /// The queued entry a payout policy consents to, or `None` for a policy with concrete
    /// contract terms.
    pub fn from_policy(policy: &PayoutPolicy) -> Result<Option<Self>, QueuedError> {
        let Some(json) = &policy.queued_entry else {
            return Ok(None);
        };
        if json.len() > MAX_CONTRACT_BYTES {
            return Err(terms_error("queued entry terms too large"));
        }
        if !policy.contract_terms.is_empty() {
            return Err(terms_error(
                "a queued entry has no contract terms until its pool forms",
            ));
        }
        if !policy.release_entry_key_after_payment
            || (policy.automatic_lightning_address.is_none() && !policy.allow_invoice_fallback)
        {
            return Err(terms_error(
                "a payout method and explicit sellback-key consent are required",
            ));
        }
        if policy.ark_escrow.is_none() {
            return Err(terms_error("a queued entry is held in an Arkade escrow"));
        }
        let entry: Self =
            serde_json::from_str(json).map_err(|error| terms_error(error.to_string()))?;
        entry.terms.validate()?;
        if entry.entry_id.get_version_num() != 7 {
            return Err(terms_error("entry ids are UUIDv7"));
        }
        Ok(Some(entry))
    }

    pub fn to_json(&self) -> Result<String, QueuedError> {
        serde_json::to_string(self).map_err(|error| terms_error(error.to_string()))
    }
}

/// What a payout policy consents to: a concrete contract, or a queued entry whose contract is
/// derived when its pool forms.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryConsent {
    Contract(ContractAuthorization),
    Queued(QueuedEntryTerms),
}

impl EntryConsent {
    pub fn from_policy(policy: &PayoutPolicy) -> Result<Self, QueuedError> {
        match QueuedEntryTerms::from_policy(policy)? {
            Some(entry) => Ok(Self::Queued(entry)),
            None => ContractAuthorization::from_policy(policy)
                .map(Self::Contract)
                .map_err(|error| terms_error(error.to_string())),
        }
    }

    pub fn entry_id(&self) -> Uuid {
        match self {
            Self::Contract(terms) => terms.entry_id,
            Self::Queued(entry) => entry.entry_id,
        }
    }

    /// The competition the player entered: for a queued entry, the queue, not its pool.
    pub fn competition_id(&self) -> Uuid {
        match self {
            Self::Contract(terms) => terms.competition_id,
            Self::Queued(entry) => entry.terms.competition_id,
        }
    }

    pub fn network(&self) -> Network {
        match self {
            Self::Contract(terms) => terms.network,
            Self::Queued(entry) => entry.terms.network,
        }
    }

    pub fn market_maker(&self) -> &MarketMaker {
        match self {
            Self::Contract(terms) => &terms.market_maker,
            Self::Queued(entry) => &entry.terms.market_maker,
        }
    }

    pub fn ticket_hash(&self) -> [u8; 32] {
        match self {
            Self::Contract(terms) => terms.ticket_hash,
            Self::Queued(entry) => entry.ticket_hash,
        }
    }

    pub fn payout_hash(&self) -> [u8; 32] {
        match self {
            Self::Contract(terms) => terms.payout_hash,
            Self::Queued(entry) => entry.payout_hash,
        }
    }

    pub fn verify_preimage(&self, preimage: &[u8]) -> Result<(), QueuedError> {
        if preimage.len() != 32 || crate::payout::sha256(preimage) != self.payout_hash() {
            return Err(terms_error("payout preimage differs from its hash"));
        }
        Ok(())
    }

    /// The latest time the entry's contract may expire: the concrete event's expiry, or every
    /// pool event's.
    pub fn expiry(&self) -> Option<u32> {
        match self {
            Self::Contract(terms) => terms.event.expiry,
            Self::Queued(entry) => Some(entry.terms.expiry),
        }
    }
}

/// How a session's members were chosen, carried in its Keymeld manifest's deposit scope for the
/// verifier to check at enrollment and binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DepositEvidence {
    /// One pool of a kickoff. The verifier recomputes every pool from the seed inputs.
    Pool {
        competition_id: Uuid,
        /// Every ticket the kickoff placed, in any order.
        tickets: Vec<Uuid>,
        /// The first block at or after registration closed.
        block_hash: BlockHash,
        pool_index: usize,
    },
    /// Deposits registered only to be refunded: a queue too small for a pool, or tickets no pool
    /// took. Nothing can be bound under it, so only the escrow refund can act.
    Refund { competition_id: Uuid },
}

impl DepositEvidence {
    pub fn encode(&self) -> Result<Vec<u8>, QueuedError> {
        let bytes = serde_json::to_vec(self).map_err(|error| formation_error(error.to_string()))?;
        if bytes.len() > MAX_EVIDENCE_BYTES {
            return Err(formation_error("too many tickets for one kickoff"));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, QueuedError> {
        if bytes.len() > MAX_EVIDENCE_BYTES {
            return Err(formation_error("evidence too large"));
        }
        serde_json::from_slice(bytes).map_err(|error| formation_error(error.to_string()))
    }

    pub fn competition_id(&self) -> Uuid {
        match self {
            Self::Pool { competition_id, .. } | Self::Refund { competition_id } => *competition_id,
        }
    }

    /// The members of the pool this evidence names, sorted by ticket id, recomputed from the seed.
    /// `None` for refund-only evidence.
    pub fn pool_members(&self, rules: &PoolRules) -> Result<Option<Vec<Uuid>>, QueuedError> {
        let Self::Pool {
            competition_id,
            tickets,
            block_hash,
            pool_index,
        } = self
        else {
            return Ok(None);
        };
        match pools::form(rules, *competition_id, tickets, block_hash)
            .map_err(|error| formation_error(error.to_string()))?
        {
            pools::Formation::TooFew => Err(formation_error("too few tickets for a pool")),
            pools::Formation::Pools { pools, .. } => {
                let mut members = pools
                    .into_iter()
                    .nth(*pool_index)
                    .ok_or_else(|| formation_error("no such pool"))?;
                members.sort_unstable();
                Ok(Some(members))
            }
        }
    }
}

/// The payout table of a pool of `players` paying `places`, keyed by the oracle's outcome order.
/// Each ranked outcome pays its winners by place; refund-all and expiry return equal shares.
pub fn pool_payouts(
    players: usize,
    places: usize,
) -> Result<BTreeMap<Outcome, PayoutWeights>, QueuedError> {
    if players == 0 || players > MAX_COMPETITION_PLAYERS || places == 0 || places >= players {
        return Err(terms_error("invalid pool player or winner count"));
    }
    // Weights are ratios, not percentages. Equal stakes must have exactly equal shares.
    let equal: PayoutWeights = (0..players).map(|index| (index, 1)).collect();
    let percentages = place_percentages(places);
    let mut payouts = BTreeMap::new();
    for (index, winners) in crate::oracle_statement::ranking_outcomes(players, places)
        .into_iter()
        .enumerate()
    {
        let weights = if winners.len() == players {
            equal.clone()
        } else {
            winners
                .into_iter()
                .enumerate()
                .map(|(rank, player)| (player, percentages[rank]))
                .collect()
        };
        payouts.insert(Outcome::Attestation(index), weights);
    }
    payouts.insert(Outcome::Expiry, equal);
    Ok(payouts)
}

/// Each paid place's share of the pot, in percent, first place first.
pub fn place_percentages(places: usize) -> Vec<u64> {
    match places {
        2 => vec![70, 30],
        3 => vec![45, 35, 20],
        4 => vec![42, 30, 18, 10],
        5 => vec![40, 27, 16, 9, 8],
        _ => vec![100],
    }
}

/// Derive a pool member's concrete contract authorization from their template, the pool's members
/// (ticket ids, as recomputed from the formation), and the oracle's signed statement of the pool's
/// event. The pool's contract must then match it exactly.
pub fn pool_authorization(
    entry: &QueuedEntryTerms,
    members: &[Uuid],
    statement: &SignedStatement,
) -> Result<ContractAuthorization, QueuedError> {
    let terms = &entry.terms;
    terms.validate()?;
    statement
        .verify(&terms.oracle_key()?)
        .map_err(|error| statement_error(error.to_string()))?;
    let core = &statement.statement;
    if core.event_id == terms.competition_id || core.event_id.get_version_num() != 7 {
        return Err(statement_error(
            "a pool's event is its own UUIDv7, not the competition's",
        ));
    }
    if core.signing_date != terms.signing_date || core.expiry != terms.expiry {
        return Err(statement_error("signing date or expiry"));
    }
    let Terms::Observation(observation) = &core.terms;
    if !observation.same_as(&terms.observation) {
        return Err(statement_error("observation terms or lines"));
    }
    let Outcomes::Ranking(ranking) = &core.outcomes;
    let mut sorted = members.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != members.len()
        || sorted.len() < terms.pool_rules.min_players()
        || sorted.len() > terms.pool_rules.max_players()
    {
        return Err(formation_error("pool size outside the pool rules"));
    }
    if ranking.number_of_places_win != terms.number_of_places_win || ranking.entry_ids != sorted {
        return Err(statement_error("places or entries differ from the pool"));
    }
    let player_index = sorted
        .iter()
        .position(|id| *id == entry.entry_id)
        .ok_or_else(|| formation_error("the entry is not in this pool"))?;
    let player_count = sorted.len();
    let funding_value = terms
        .stake_sats
        .checked_mul(player_count as u64)
        .map(Amount::from_sat)
        .ok_or_else(|| terms_error("funding value overflows"))?;
    Ok(ContractAuthorization {
        competition_id: core.event_id,
        entry_id: entry.entry_id,
        network: terms.network,
        player_index,
        player_count,
        ticket_hash: entry.ticket_hash,
        payout_hash: entry.payout_hash,
        market_maker: terms.market_maker.clone(),
        event: EventLockingConditions {
            locking_points: core.locking_points(terms.oracle_point()?),
            expiry: Some(core.expiry),
        },
        outcome_payouts: pool_payouts(player_count, terms.number_of_places_win as usize)?,
        funding_value,
        relative_locktime_block_delta: terms.relative_locktime_block_delta,
        max_fee_rate: terms.max_fee_rate,
    })
}

#[cfg(test)]
#[path = "queued_tests.rs"]
mod tests;
