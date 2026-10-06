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
    capacity::{MAX_COMPETITION_WINNING_PLACES, MAX_TWO_PLACE_PLAYERS},
    oracle_statement::ObservationTerms,
    pools::{PoolRules, MAX_POOL_PLAYERS},
    queued::{pool_places, QueuedTerms, MULTI_PLACE_MIN_PLAYERS},
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

use super::{ContractOptions, CoordinatorFee, CreateEvent};
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
/// Why a player holding [`MAX_UNPAID_TICKETS_PER_PLAYER`] unpaid tickets gets no other. The entry
/// form resumes one of them rather than asking, so a player sees this only from another client.
pub const TOO_MANY_UNPAID: &str =
    "You have unpaid entries waiting in this competition; open its entry form and press Pay to \
     pay one, or wait for its invoice to expire";
/// A queued ticket's id is its entry's id, which the player's wallet makes when it opens the
/// entry form. The oracle breaks an exact tie by entry id, so an id may not claim to be older
/// than this, nor from the future. The entry is finished within the same window: once its id is
/// older, the entry is refused and its paid ticket is refunded. A single competition's entry id,
/// which its payout authorization names, is held to the same window. A paid ticket left without
/// its entry past it no longer counts as its player's entry, nor takes a place in its competition.
pub const MAX_ENTRY_ID_AGE: Duration = Duration::hours(1);
pub const MAX_ENTRY_ID_SKEW: Duration = Duration::minutes(5);
/// The end of [`MAX_ENTRY_ID_AGE`] kept for paying a ticket's invoice and finishing its entry: an
/// invoice is issued, or an unpaid ticket handed back, only for an entry id younger than the rest.
pub const PAY_AND_ENTER_TIME: Duration = Duration::minutes(15);
/// Why a ticket is refused for an entry id outside its window. The entry form and Synth start the
/// entry again under a new id when they get it.
pub const STALE_ENTRY_ID: &str =
    "the entry id is too old or from the future; start the entry again for a new one";
/// Why a paid ticket's entry is refused once its entry id is older than [`MAX_ENTRY_ID_AGE`].
pub const ENTRY_WINDOW_PASSED: &str =
    "This entry was started over an hour ago, so it can no longer be finished; its entry fee \
     will be refunded";

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
    /// What counts against `max_entries`: paid tickets and tickets held for an unexpired
    /// invoice. When it reaches `max_entries`, no ticket is issued until a hold lapses.
    #[serde(default)]
    pub held: u64,
    /// Each player's share of a pool's funding value: the entry fee.
    pub stake_sats: u64,
    /// Hex of the digest of the terms every player consents to; key deposits are sealed under it.
    pub terms_digest: String,
    /// The pools formed when registration closed, by index. Empty until then.
    #[serde(default)]
    pub pools: Vec<PoolSummary>,
}

/// An unpaid ticket a player holds in a queued competition, which its Pay button resumes: the
/// ticket's id is its entry's, so asking for a ticket for that entry again gets this ticket and
/// its invoice back, and the wallet derives the same entry key from the id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnpaidTicket {
    pub ticket_id: Uuid,
    pub competition_id: Uuid,
    /// When its invoice stops being payable; `None` until the invoice is made.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub invoice_expires_at: Option<OffsetDateTime>,
}

impl UnpaidTicket {
    pub fn from_ticket(ticket: &super::Ticket) -> Self {
        Self {
            ticket_id: ticket.id,
            competition_id: ticket.competition_id,
            invoice_expires_at: ticket.invoice_expires_at,
        }
    }
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
/// It fixes what a single competition's `CreateEvent` fixes except the seat count: pools score
/// with lines, hold `min_players` to `max_pool_size` players, and pay `number_of_places_win`
/// places once they have `MULTI_PLACE_MIN_PLAYERS` players, one place below that.
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
    /// How many entries one player may make; one if unset.
    #[serde(default = "one_entry_per_player")]
    pub max_entries_per_player: u32,
    /// The places a pool of `MULTI_PLACE_MIN_PLAYERS` or more pays: 1, or 2 for pools of at most
    /// 20. One if unset.
    #[serde(default = "one_place")]
    pub number_of_places_win: usize,
}

fn one_entry_per_player() -> u32 {
    super::ONE_ENTRY_PER_PLAYER
}

fn one_place() -> usize {
    1
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
        if !(1..=MAX_COMPETITION_WINNING_PLACES).contains(&self.number_of_places_win)
            || (self.number_of_places_win > 1 && rules.max_players() > MAX_TWO_PLACE_PLAYERS)
        {
            return Err(format!(
                "pools pay one place, or two places in pools of at most {MAX_TWO_PLACE_PLAYERS}"
            ));
        }
        Ok((rules, max_entries))
    }

    /// The places the largest pool pays: what the confidential signing path must handle.
    pub fn largest_pool_places(&self) -> usize {
        pool_places(self.number_of_places_win as u32, self.max_pool_size) as usize
    }

    /// The reference oracle event: the competition's own id, its places, lines scoring, off the
    /// oracle's public list, and as many entries as its largest pool can hold. It never gets
    /// entries; it freezes the lines every pool copies.
    pub fn reference_event(&self) -> Result<CreateEvent, String> {
        let entries = self.max_pool_size;
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
            number_of_places_win: self.number_of_places_win,
            total_allowed_entries: entries,
            entry_fee: self.entry_fee,
            coordinator_fee: self.coordinator_fee,
            total_competition_pool,
            relative_locktime_block_delta: self.relative_locktime_block_delta,
            unlisted: true,
            scoring_rules: Some(ScoringRules::Lines),
            scoring_fields: None,
            // Pools copy it, so the limit holds in every pool as it did in the queue.
            max_entries_per_player: self.max_entries_per_player,
            // Pools copy it, so every pool's contract is built with the options new competitions
            // use.
            contract_options: Some(ContractOptions::NEW),
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
/// event with the pool's id, seat count, funding value and `places`, the places its size pays
/// (`QueuedTerms::pool_places`). Pools appear on the oracle's list; the reference event stays
/// off it because nobody enters that event. Every other field is the reference event's, so the
/// pool's statement carries the terms its players consented to.
pub fn pool_event(
    reference: &CreateEvent,
    pool_id: Uuid,
    players: usize,
    stake_sats: u64,
    places: u32,
) -> Result<CreateEvent, String> {
    let total_competition_pool = usize::try_from(stake_sats)
        .ok()
        .and_then(|stake| stake.checked_mul(players))
        .ok_or("a pool's funding value overflows")?;
    Ok(CreateEvent {
        id: pool_id,
        total_allowed_entries: players,
        total_competition_pool,
        number_of_places_win: places as usize,
        unlisted: false,
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
    let places = reference.number_of_places_win;
    if !(1..=MAX_COMPETITION_WINNING_PLACES as u32).contains(&places) {
        return Err("a queued competition's pools pay one or two places".into());
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
        number_of_places_win: places,
        multi_place_min_players: (places > 1).then_some(MULTI_PLACE_MIN_PLAYERS),
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

/// Whether an entry's id is a UUIDv7 made recently enough to be the time its player started
/// entering, so that the entry may still be made at `now`: at most [`MAX_ENTRY_ID_AGE`] ago.
pub fn check_entry_id(entry_id: Uuid, now: OffsetDateTime) -> Result<(), String> {
    check_entry_id_age(entry_id, now, MAX_ENTRY_ID_AGE)
}

/// Whether a ticket may be issued at `now` for an entry under `entry_id`, or handed back unpaid:
/// as [`check_entry_id`], with [`PAY_AND_ENTER_TIME`] of the window left to pay its invoice and
/// finish the entry.
pub fn check_ticket_entry_id(entry_id: Uuid, now: OffsetDateTime) -> Result<(), String> {
    check_entry_id_age(entry_id, now, MAX_ENTRY_ID_AGE - PAY_AND_ENTER_TIME)
}

/// When an entry under `entry_id` can no longer be finished: [`MAX_ENTRY_ID_AGE`] after the
/// player's wallet made the id.
pub fn entry_finish_by(entry_id: Uuid) -> Result<OffsetDateTime, String> {
    Ok(entry_id_time(entry_id)? + MAX_ENTRY_ID_AGE)
}

/// Whether the entry under `entry_id` can no longer be made at `now`: its id is older than
/// [`MAX_ENTRY_ID_AGE`], so [`check_entry_id`] refuses it. A paid ticket still without its entry
/// by then has lapsed (see `ticket_registration::LapsedTicket`).
pub fn entry_window_passed(entry_id: Uuid, now: OffsetDateTime) -> bool {
    entry_id_time(entry_id).is_ok_and(|made| now - made > MAX_ENTRY_ID_AGE)
}

fn check_entry_id_age(
    entry_id: Uuid,
    now: OffsetDateTime,
    max_age: Duration,
) -> Result<(), String> {
    let made = entry_id_time(entry_id)?;
    if made > now + MAX_ENTRY_ID_SKEW || now - made > max_age {
        return Err(STALE_ENTRY_ID.into());
    }
    Ok(())
}

/// When the player's wallet made `entry_id`, a UUIDv7.
fn entry_id_time(entry_id: Uuid) -> Result<OffsetDateTime, String> {
    if entry_id.get_version_num() != 7 {
        return Err("an entry's id must be a UUIDv7".into());
    }
    let (seconds, nanos) = entry_id
        .get_timestamp()
        .ok_or("an entry's id has no time")?
        .to_unix();
    Ok(OffsetDateTime::from_unix_timestamp(seconds as i64)
        .map_err(|_| "an entry's id has an invalid time")?
        + Duration::nanoseconds(i64::from(nanos)))
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
