//! Queued competitions: entries without a seat count, split into pools when registration closes.
//!
//! A queued competition is a `competitions` row of kind `queued`. It owns registration: tickets
//! are created when players ask for them, each buy-in waits in an Arkade escrow, and each
//! player's entry key is deposited with Keymeld under the competition's terms, not a session.
//! It creates a reference oracle event when it is created, which freezes the lines players pick
//! against, but never gives it entries.
//!
//! When registration closes it splits its complete tickets into even pools
//! (`coordinator_escrow::pools`), and each pool becomes a competition of kind `pool` that runs the
//! usual lifecycle from escrow confirmation on, with its own oracle event, Keymeld session and
//! Arkade batch. A queue too small for a pool is cancelled and refunded. See
//! `docs/QUEUED_COMPETITIONS.md`, `queued_coordinator.rs` and `queued_kickoff.rs`.

use coordinator_escrow::{
    oracle_statement::ObservationTerms,
    pools::{PoolRules, MAX_POOL_PLAYERS},
    queued::QueuedTerms,
};
use dlctix::{
    bitcoin::{FeeRate, Network},
    musig2::secp256k1::PublicKey,
    secp::Point,
    MarketMaker,
};
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqliteRow, Row};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{CoordinatorFee, CreateEvent};
use crate::infra::{
    bitcoin::BlockSummary,
    oracle::{OracleEventTerms, ScoringRules},
};

/// Players a pool needs unless the admin asks for more.
pub const DEFAULT_MIN_PLAYERS: usize = 2;
/// Entries a queued competition takes unless the admin sets another cap.
pub const DEFAULT_MAX_ENTRIES: u32 = 500;
/// The most entries a queued competition may take. Every ticket a kickoff places is listed in
/// each pool's Keymeld evidence, which holds at most 64 KiB.
pub const MAX_ENTRIES: u32 = 1_500;
/// How long registration may stay open. Escrow VTXOs expire seven days after they are created
/// and nothing renews them yet, so every escrow must be spent or refunded before then.
pub const MAX_REGISTRATION: Duration = Duration::days(6);
/// Unpaid tickets one player may hold in one queued competition at a time.
pub const MAX_UNPAID_TICKETS_PER_PLAYER: i64 = 3;
/// A queued ticket's id is its entry's id, which the player's wallet makes when it opens the
/// entry form. The oracle breaks an exact tie by entry id, so an id may not claim to be older
/// than this, nor from the future.
pub const MAX_ENTRY_ID_AGE: Duration = Duration::hours(1);
pub const MAX_ENTRY_ID_SKEW: Duration = Duration::minutes(5);

/// What a `competitions` row is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompetitionKind {
    /// A competition with a fixed seat count: the only kind before queued competitions.
    #[default]
    Single,
    /// Takes entries without a seat count and forms pools when registration closes.
    Queued,
    /// One pool of a queued competition.
    Pool,
}

impl CompetitionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Queued => "queued",
            Self::Pool => "pool",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::Single, Self::Queued, Self::Pool]
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }
}

/// A queued competition's settings, entries and pools, as the API shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSummary {
    pub pool_rules: PoolRules,
    /// Paid entries: those still in the queue and those in its pools.
    pub entries: u64,
    /// The most entries the queue takes.
    pub max_entries: u32,
    /// Each player's share of a pool's funding value: the entry fee.
    pub stake_sats: u64,
    /// Hex of the digest of the terms every player consents to; key deposits are sealed under it.
    pub terms_digest: String,
    /// The pools formed when registration closed, by index. Empty until then.
    #[serde(default)]
    pub pools: Vec<PoolSummary>,
}

/// One pool of a queued competition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSummary {
    /// The pool's competition, which is also its oracle event.
    pub competition_id: Uuid,
    pub pool_index: u32,
    pub players: usize,
}

/// An admin's request for a queued competition.
///
/// It fixes what a single competition's `CreateEvent` fixes except the seat count: every pool
/// scores one winner, with lines, and pools hold `min_players` to `max_pool_size` players.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateQueuedCompetition {
    /// A UUIDv7; also the reference oracle event's id.
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub signing_date: OffsetDateTime,
    /// Registration closes, and pools form, when observation starts.
    #[serde(with = "time::serde::rfc3339")]
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end_observation_date: OffsetDateTime,
    pub locations: Vec<String>,
    pub number_of_values_per_entry: usize,
    /// Each player's stake, in sats: a pool of `n` funds `n` stakes.
    pub entry_fee: usize,
    #[serde(flatten)]
    pub coordinator_fee: CoordinatorFee,
    #[serde(default)]
    pub relative_locktime_block_delta: Option<u16>,
    #[serde(default = "default_min_players")]
    pub min_players: usize,
    #[serde(default = "default_max_pool_size")]
    pub max_pool_size: usize,
    /// The most entries the queue takes; `DEFAULT_MAX_ENTRIES` if unset.
    #[serde(default)]
    pub max_entries: Option<u32>,
}

fn default_min_players() -> usize {
    DEFAULT_MIN_PLAYERS
}

fn default_max_pool_size() -> usize {
    MAX_POOL_PLAYERS
}

impl CreateQueuedCompetition {
    /// The pool rules and entry cap, checked.
    pub fn settings(&self) -> Result<(PoolRules, u32), String> {
        let rules = PoolRules::new(self.min_players, self.max_pool_size)
            .map_err(|error| error.to_string())?;
        let max_entries = self.max_entries.unwrap_or(DEFAULT_MAX_ENTRIES);
        if max_entries < rules.min_players() as u32 || max_entries > MAX_ENTRIES {
            return Err(format!(
                "the entry cap must be between the minimum pool size and {MAX_ENTRIES}"
            ));
        }
        if self.entry_fee == 0 {
            return Err("the entry fee must be positive".into());
        }
        Ok((rules, max_entries))
    }

    /// The reference oracle event: the competition's own id, one winner, lines scoring, off the
    /// oracle's public list, and as many entries as the largest pool can hold. It never gets
    /// entries; it freezes the lines every pool copies.
    pub fn reference_event(&self) -> Result<CreateEvent, String> {
        let entries = MAX_POOL_PLAYERS;
        let total_competition_pool = self
            .entry_fee
            .checked_mul(entries)
            .ok_or("the entry fee is too large")?;
        let mut event = CreateEvent {
            id: self.id,
            signing_date: self.signing_date,
            start_observation_date: self.start_observation_date,
            end_observation_date: self.end_observation_date,
            locations: self.locations.clone(),
            number_of_values_per_entry: self.number_of_values_per_entry,
            number_of_places_win: 1,
            total_allowed_entries: entries,
            entry_fee: self.entry_fee,
            coordinator_fee: self.coordinator_fee,
            total_competition_pool,
            relative_locktime_block_delta: self.relative_locktime_block_delta,
            unlisted: true,
            scoring_rules: Some(ScoringRules::Lines),
            scoring_fields: None,
        };
        // Pools copy the reference event, so every pool scores the metrics its window holds.
        event.fix_window_metrics()?;
        Ok(event)
    }

    /// Registration may not outlast an escrow VTXO.
    pub fn check_registration(&self, now: OffsetDateTime) -> Result<(), String> {
        if self.start_observation_date - now > MAX_REGISTRATION {
            return Err(format!(
                "registration may stay open for at most {} days",
                MAX_REGISTRATION.whole_days()
            ));
        }
        Ok(())
    }
}

/// The event a pool of `players` players asks the oracle for: its queued competition's reference
/// event with the pool's id, seat count and funding value. Every other field is the reference
/// event's, so the pool's statement carries the terms its players consented to.
pub fn pool_event(
    reference: &CreateEvent,
    pool_id: Uuid,
    players: usize,
    stake_sats: u64,
) -> Result<CreateEvent, String> {
    let total_competition_pool = usize::try_from(stake_sats)
        .ok()
        .and_then(|stake| stake.checked_mul(players))
        .ok_or("a pool's funding value overflows")?;
    Ok(CreateEvent {
        id: pool_id,
        total_allowed_entries: players,
        total_competition_pool,
        ..reference.clone()
    })
}

/// What `build_terms` needs besides the reference event.
pub struct TermsInputs {
    pub competition_id: Uuid,
    pub network: Network,
    pub market_maker: Point,
    /// The oracle's key, as `GET /oracle/pubkey` gives it.
    pub oracle_key: PublicKey,
    pub pool_rules: PoolRules,
    pub stake_sats: u64,
    pub relative_locktime_block_delta: u16,
    pub max_fee_rate: FeeRate,
}

/// The terms every player of a queued competition consents to, from its reference event as the
/// oracle created it: the signing date, expiry and observation terms that every pool's signed
/// statement must repeat.
pub fn build_terms(
    inputs: TermsInputs,
    reference: &OracleEventTerms,
) -> Result<QueuedTerms, String> {
    if reference.event.id != inputs.competition_id {
        return Err("the reference event has another id".into());
    }
    if reference.number_of_places_win != 1 {
        return Err("a queued competition's pools pay one winner".into());
    }
    let observation: ObservationTerms =
        reference.observation().map_err(|error| error.to_string())?;
    if observation.scoring_rules != coordinator_escrow::oracle_statement::ScoringRules::Lines
        || observation.lines.is_empty()
    {
        return Err("the oracle did not freeze lines for the reference event".into());
    }
    let expiry = reference
        .event
        .event_announcement
        .expiry
        .ok_or("the reference event has no expiry")?;
    let terms = QueuedTerms {
        competition_id: inputs.competition_id,
        network: inputs.network,
        market_maker: MarketMaker {
            pubkey: inputs.market_maker,
        },
        oracle_pubkey: oracle_pubkey_text(&inputs.oracle_key),
        signing_date: reference.signing_date.unix_timestamp(),
        expiry,
        observation,
        number_of_places_win: 1,
        pool_rules: inputs.pool_rules,
        stake_sats: inputs.stake_sats,
        relative_locktime_block_delta: inputs.relative_locktime_block_delta,
        max_fee_rate: inputs.max_fee_rate,
    };
    terms.validate().map_err(|error| error.to_string())?;
    Ok(terms)
}

/// The oracle key as `QueuedTerms` carries it: x-only hex.
pub fn oracle_pubkey_text(key: &PublicKey) -> String {
    key.x_only_public_key().0.to_string()
}

/// Whether a queued ticket's id, its entry's id, is a UUIDv7 made recently enough to be the
/// time its player started entering.
pub fn check_entry_id(entry_id: Uuid, now: OffsetDateTime) -> Result<(), String> {
    if entry_id.get_version_num() != 7 {
        return Err("a queued entry's id must be a UUIDv7".into());
    }
    let (seconds, nanos) = entry_id
        .get_timestamp()
        .ok_or("a queued entry's id has no time")?
        .to_unix();
    let made = OffsetDateTime::from_unix_timestamp(seconds as i64)
        .map_err(|_| "a queued entry's id has an invalid time")?
        + Duration::nanoseconds(i64::from(nanos));
    if made > now + MAX_ENTRY_ID_SKEW || now - made > MAX_ENTRY_ID_AGE {
        return Err(
            "the entry id is too old or from the future; start the entry again for a new one"
                .into(),
        );
    }
    Ok(())
}

/// Header times may run ahead of the clock by up to two hours, so a run of headers is known to
/// start before the closing block only once its first header is this much older than the close.
const HEADER_TIME_DRIFT: i64 = 2 * 60 * 60;

/// The lowest height whose header time is at least `close`, among `blocks` in height order, if
/// the blocks reach far enough back: the earliest must be well before `close`, or an earlier
/// block might be the first.
pub(crate) fn first_block_at_or_after(blocks: &[BlockSummary], close: i64) -> Option<BlockSummary> {
    let first = blocks.first()?;
    if i64::from(first.time) >= close - HEADER_TIME_DRIFT {
        return None;
    }
    blocks
        .iter()
        .find(|block| i64::from(block.time) >= close)
        .copied()
}

/// A column that queries written before queued competitions do not select reads as absent.
pub(super) fn optional_column<T>(row: &SqliteRow, column: &str) -> Result<Option<T>, sqlx::Error>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    match row.try_get::<Option<T>, _>(column) {
        Ok(value) => Ok(value),
        Err(sqlx::Error::ColumnNotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}
