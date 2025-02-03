mod coordinator;
mod db_migrations;
mod store;

use std::sync::Arc;

use crate::{
    oracle_client::{Event, WeatherChoices},
    Ln,
};
pub use coordinator::*;
pub use db_migrations::*;
use dlctix::{
    bitcoin::{
        hex::{Case, DisplayHex},
        OutPoint, Transaction,
    },
    hashlock,
    musig2::{AggNonce, PartialSignature, PubNonce},
    ContractParameters, EventLockingConditions, SigMap, SignedContract,
};
use duckdb::{
    types::{Type, Value},
    Row,
};
use log::{debug, error};
use serde::{Deserialize, Serialize};
pub use store::*;
use time::{macros::format_description, Duration, OffsetDateTime};
use uuid::Uuid;

use super::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddEntry {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    /// ID that matches a ticket in this competition that the user paid
    pub ticket_id: Uuid,
    pub ephemeral_pubkey: String,
    /// User provided private encrypted to their nostr key, only stored for easier UX,
    /// backed up via dm to user
    pub ephemeral_privatekey_encrypted: String,
    /// A hash used for unlocking the split TX output early. To allow winning
    /// players to receive off-chain payouts, they must provide this `payout_hash`,
    /// for which they know the preimage. By selling the preimage to the market maker,
    /// they allow the market maker to reclaim the on-chain funds. Expects the hash in hex.
    pub payout_hash: String,
    /// User provided preimage encrypted to their nostr key, only stored for easier UX,
    /// backed up via dm to user
    pub payout_preimage_encrypted: String,
    pub event_id: Uuid,
    pub expected_observations: Vec<WeatherChoices>,
}

pub enum EntryStatus {
    Paid,
    Signed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEntry {
    pub id: Uuid,
    /// The id used by the oracle to assoicate the event with this entry
    pub event_id: Uuid,
    /// The id used for the ticket the user needs to have paid for entry to be valid
    pub ticket_id: Uuid,
    /// The user's nostr pubkey
    pub pubkey: String,
    /// Pubkey created for this entry for the user
    pub ephemeral_pubkey: String,
    /// User provided private encrypted to their nostr key, only stored for easier UX,
    /// backed up via dm to user
    pub ephemeral_privatekey_encrypted: String,
    /// A hash used for unlocking the split TX output early. To allow winning
    /// players to receive off-chain payouts, they must provide this `payout_hash`,
    /// for which they know the preimage. By selling the preimage to the market maker,
    /// they allow the market maker to reclaim the on-chain funds. Expects the hash in hex.
    pub payout_hash: String,
    /// User provided preimage encrypted to their nostr key, only stored for easier UX,
    /// backed up via dm to user
    pub payout_preimage_encrypted: String,
    /// User provided private de-encrypted, only used during payout
    pub ephemeral_privatekey: Option<String>,
    /// User provided preimage de-encrypted, only used during payout
    pub payout_preimage: Option<String>,
    pub public_nonces: Option<SigMap<PubNonce>>,
    pub partial_signatures: Option<SigMap<PartialSignature>>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub signed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_at: Option<OffsetDateTime>,
}

impl<'a> TryFrom<&Row<'a>> for UserEntry {
    type Error = duckdb::Error;

    fn try_from(row: &Row) -> Result<Self, Self::Error> {
        let public_nonces: Option<SigMap<PubNonce>> =
            row.get::<usize, Option<Value>>(7).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?;
        let partial_signatures: Option<SigMap<PartialSignature>> =
            row.get::<usize, Option<Value>>(8).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?;

        let user_entry = UserEntry {
            id: row
                .get::<usize, String>(0)
                .map(|val| Uuid::parse_str(&val))?
                .map_err(|e| duckdb::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e)))?,
            ticket_id: row
                .get::<usize, String>(1)
                .map(|val| Uuid::parse_str(&val))?
                .map_err(|e| duckdb::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e)))?,
            event_id: row
                .get::<usize, String>(2)
                .map(|val| Uuid::parse_str(&val))?
                .map_err(|e| duckdb::Error::FromSqlConversionFailure(1, Type::Text, Box::new(e)))?,
            pubkey: row.get::<usize, String>(3)?,
            ephemeral_pubkey: row.get::<usize, String>(4)?,
            ephemeral_privatekey_encrypted: row.get::<usize, String>(5)?,
            ephemeral_privatekey: row.get::<usize, Option<String>>(6)?,
            public_nonces,
            partial_signatures,
            payout_preimage_encrypted: row.get(9)?,
            payout_hash: row.get(10)?,
            payout_preimage: row.get::<usize, Option<String>>(11)?,
            signed_at: parse_optional_timestamp(row.get::<usize, Option<String>>(12)?, 12)?,
            paid_at: parse_optional_timestamp(row.get::<usize, Option<String>>(13)?, 13)?,
        };
        Ok(user_entry)
    }
}

impl AddEntry {
    fn into_user_entry(self, pubkey: String) -> UserEntry {
        UserEntry {
            id: self.id,
            event_id: self.event_id,
            ticket_id: self.ticket_id,
            pubkey,
            ephemeral_pubkey: self.ephemeral_pubkey,
            ephemeral_privatekey_encrypted: self.ephemeral_privatekey_encrypted,
            payout_hash: self.payout_hash,
            payout_preimage_encrypted: self.payout_preimage_encrypted,
            signed_at: None,
            partial_signatures: None,
            public_nonces: None,
            ephemeral_privatekey: None,
            payout_preimage: None,
            paid_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub id: Uuid,
    pub competition_id: Uuid,
    pub entry_id: Option<Uuid>,
    pub encrypted_preimage: String,
    pub hash: String,
    pub payment_request: String,
    pub expiry: OffsetDateTime,
    pub reserved_by: Option<String>,
    pub reserved_at: Option<OffsetDateTime>,
    pub paid_at: Option<OffsetDateTime>,
}

impl Ticket {
    pub fn get_status(&self) -> TicketStatus {
        let now = OffsetDateTime::now_utc();

        // Check if ticket has been used for an entry
        if self.entry_id.is_some() {
            return TicketStatus::Used;
        }

        // Check if ticket has expired
        if now > self.expiry {
            return TicketStatus::Expired;
        }

        // Check if ticket has been paid
        if self.paid_at.is_some() {
            return TicketStatus::Paid;
        }

        // Check if ticket is reserved
        if let Some(reserved_at) = self.reserved_at {
            // If reservation is older than 10 minutes and not paid, consider it expired
            if now - reserved_at > Duration::minutes(10) {
                return TicketStatus::Expired;
            }
            return TicketStatus::Reserved;
        }

        TicketStatus::Created
    }

    pub fn is_paid(&self) -> bool {
        matches!(self.get_status(), TicketStatus::Paid)
    }

    pub fn is_reserved(&self) -> bool {
        matches!(self.get_status(), TicketStatus::Reserved)
    }

    pub fn is_used(&self) -> bool {
        matches!(self.get_status(), TicketStatus::Used)
    }

    pub fn is_expired(&self) -> bool {
        matches!(self.get_status(), TicketStatus::Expired)
    }

    pub fn can_be_used(&self) -> bool {
        matches!(self.get_status(), TicketStatus::Paid)
    }

    pub fn can_be_reserved(&self) -> bool {
        matches!(self.get_status(), TicketStatus::Created)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TicketStatus {
    Created,   // Initial state
    Reserved,  // Payment request generated
    Paid,      // HODL invoice accepted
    Used,      // Entry created with this ticket
    Expired,   // Payment time window expired
    Cancelled, // Competition cancelled
}

//TODO: add pagination when it's needed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchBy {
    /// Optionally add event_ids
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_ids: Option<Vec<Uuid>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEvent {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    /// Time at which the attestation will be added to the event, needs to be after the observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Date of when the weather observations occured (midnight UTC), all entries must be made before this time
    pub observation_date: OffsetDateTime,
    /// NOAA observation stations used in this event
    pub locations: Vec<String>,
    /// The number of values that can be selected per entry in the event (default to number_of_locations * 3, (temp_low, temp_high, wind_speed))
    pub number_of_values_per_entry: usize,
    /// The number of ranks that can win, 1st -> 40%, 2nd -> 35%, 3rd -> 25% (something like that from the prize pool)
    pub number_of_places_win: usize,
    /// Total number of allowed entries into the event
    pub total_allowed_entries: usize,
    /// Total sats required per entry for ticket
    pub entry_fee: usize,
    /// Total sats in competition pool to be won
    pub total_competition_pool: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorInfo {
    /// The pubkey of the coordinator
    pub pubkey: String,
    /// The values of the payload signed by the coordinator
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Competition {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub total_competition_pool: u64,
    pub total_allowed_entries: u64,
    pub entry_fee: u64,
    pub event_announcement: EventLockingConditions,
    pub total_entries: u64,
    pub total_entry_nonces: u64,
    pub total_signed_entries: u64,
    pub total_paid_entries: u64,
    pub number_of_places_win: usize,
    pub funding_transaction: Option<Transaction>,
    pub funding_outpoint: Option<OutPoint>,
    pub contract_parameters: Option<ContractParameters>,
    pub public_nonces: Option<SigMap<PubNonce>>,
    pub aggregated_nonces: Option<SigMap<AggNonce>>,
    pub partial_signatures: Option<SigMap<PartialSignature>>,
    pub signed_contract: Option<SignedContract>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub contracted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub signed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_broadcasted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub failed_at: Option<OffsetDateTime>,
    pub errors: Vec<CompetitionError>,
}

impl Competition {
    async fn generate_competition_tickets(
        &self,
        total_tickets: usize,
        ln_client: &Arc<dyn Ln>,
    ) -> Result<Vec<Ticket>, Error> {
        let (total_expiry_secs, expiry_time) = self.calculate_ticket_expiry()?;
        let mut tickets = Vec::with_capacity(total_tickets);

        for i in 0..total_tickets {
            let ticket = self
                .create_ticket(total_expiry_secs, expiry_time, ln_client)
                .await?;

            tickets.push(ticket);

            if i < total_tickets - 1 {
                tokio::time::sleep(std::time::Duration::from_micros(1)).await;
            }
        }

        Ok(tickets)
    }

    fn calculate_ticket_expiry(&self) -> Result<(u64, OffsetDateTime), Error> {
        let signing_window = Duration::hours(3); //TODO: making signing window configurable
        let now = OffsetDateTime::now_utc();
        let total_expiry_secs = signing_window.whole_seconds() as u64;

        Ok((
            total_expiry_secs,
            now + Duration::seconds(total_expiry_secs as i64),
        ))
    }

    async fn create_ticket(
        &self,
        expiry_secs: u64,
        expiry_time: OffsetDateTime,
        ln_client: &Arc<dyn Ln>,
    ) -> Result<Ticket, Error> {
        let ticket_preimage = hashlock::preimage_random(&mut rand::thread_rng());
        let ticket_hash = hashlock::sha256(&ticket_preimage);

        let invoice = ln_client
            .add_hold_invoice(
                self.entry_fee,
                expiry_secs,
                ticket_hash.to_hex_string(Case::Lower),
                self.id,
            )
            .await
            .map_err(Error::HoldError)?;

        Ok(Ticket {
            id: Uuid::now_v7(),
            competition_id: self.id,
            entry_id: None,
            encrypted_preimage: ticket_preimage.to_lower_hex_string(), // TODO: encrypt this
            hash: ticket_hash.to_lower_hex_string(),
            payment_request: invoice.payment_request,
            expiry: expiry_time,
            reserved_by: None,
            reserved_at: None,
            paid_at: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundedContract {
    pub contract_params: ContractParameters,
    pub funding_outpoint: OutPoint,
}

#[derive(Debug, PartialEq, PartialOrd, Eq)]
pub enum CompetitionState {
    Created,
    EntriesCollected,
    ContractCreated,
    NoncesCollected,
    AggregateNoncesGenerated,
    PartialSignaturesCollected,
    SigningComplete,
    Broadcasted,
    Failed,
    Cancelled,
}

impl Competition {
    pub fn new(create_event: &CreateEvent, oracle_event: &Event) -> Self {
        Self {
            id: create_event.id,
            created_at: OffsetDateTime::now_utc(),
            total_competition_pool: create_event.total_competition_pool as u64,
            total_allowed_entries: create_event.total_allowed_entries as u64,
            entry_fee: create_event.entry_fee as u64,
            event_announcement: oracle_event.event_announcement.clone(),
            total_entries: 0,
            total_entry_nonces: 0,
            total_signed_entries: 0,
            total_paid_entries: 0,
            number_of_places_win: create_event.number_of_places_win,
            funding_transaction: None,
            funding_outpoint: None,
            contract_parameters: None,
            public_nonces: None,
            aggregated_nonces: None,
            cancelled_at: None,
            contracted_at: None,
            signed_at: None,
            signed_contract: None,
            partial_signatures: None,
            funding_broadcasted_at: None,
            failed_at: None,
            errors: vec![],
        }
    }
    pub fn has_full_entries(&self) -> bool {
        (self.total_entries > 0) && self.total_entries >= self.total_allowed_entries
    }

    pub fn is_contract_created(&self) -> bool {
        self.contracted_at.is_some()
    }

    pub fn has_all_entry_nonces(&self) -> bool {
        (self.total_entries > 0) && (self.total_entry_nonces >= self.total_entries)
    }

    pub fn has_all_entry_partial_signatures(&self) -> bool {
        (self.total_entries > 0) && (self.total_signed_entries >= self.total_entries)
    }

    pub fn has_all_entries_paid(&self) -> bool {
        (self.total_entries > 0) && (self.total_paid_entries >= self.total_entries)
    }

    pub fn is_contract_signed(&self) -> bool {
        self.signed_at.is_some()
    }

    pub fn is_funding_tx_broadcasted(&self) -> bool {
        self.funding_broadcasted_at.is_some()
    }

    pub fn skip_competition(&self) -> bool {
        self.is_failed() || self.is_cancelled() || self.is_funding_tx_broadcasted()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled_at.is_some()
    }

    pub fn is_failed(&self) -> bool {
        //NOTE: may want to use to enable retry logic, check if error > n
        self.failed_at.is_some()
    }

    pub fn is_expired(&self) -> bool {
        let now = OffsetDateTime::now_utc();

        // Competition should expire if not enough entries collected before observation date
        if let Some(observation_date) = self.event_announcement.expiry {
            if now.unix_timestamp() as u32 >= observation_date && !self.has_full_entries() {
                return true;
            }
        }

        // Add timeouts for different stages
        match self.get_state() {
            CompetitionState::ContractCreated => {
                // Give users 1 hour to submit nonces after contract creation
                self.contracted_at
                    .map(|t| now - t > Duration::hours(1))
                    .unwrap_or(false)
            }
            CompetitionState::AggregateNoncesGenerated => {
                // Give users 1 hour to submit partial signatures
                self.aggregated_nonces.is_some()
                    && self
                        .contracted_at
                        .map(|t| now - t > Duration::hours(2))
                        .unwrap_or(false)
            }
            _ => false,
        }
    }

    pub fn get_state(&self) -> CompetitionState {
        if self.is_cancelled() {
            return CompetitionState::Cancelled;
        }
        if self.is_failed() {
            return CompetitionState::Failed;
        }
        if self.funding_broadcasted_at.is_some() {
            return CompetitionState::Broadcasted;
        }
        if self.signed_at.is_some() {
            return CompetitionState::SigningComplete;
        }
        if self.has_all_entry_partial_signatures() {
            debug!(
                "All signatures collected: {}/{}",
                self.total_signed_entries, self.total_entries
            );
            return CompetitionState::PartialSignaturesCollected;
        }
        if self.aggregated_nonces.is_some() {
            return CompetitionState::AggregateNoncesGenerated;
        }
        if self.has_all_entry_nonces() {
            debug!(
                "All nonces collected: {}/{}",
                self.total_entry_nonces, self.total_entries
            );
            return CompetitionState::NoncesCollected;
        }
        if self.has_full_entries() && self.has_all_entries_paid() {
            return CompetitionState::EntriesCollected;
        }
        if self.contract_parameters.is_some() {
            return CompetitionState::ContractCreated;
        }
        CompetitionState::Created
    }
}

impl<'a> TryFrom<&Row<'a>> for Competition {
    type Error = duckdb::Error;

    fn try_from(row: &Row) -> Result<Self, Self::Error> {
        let competition = Competition {
            id: row
                .get::<usize, String>(0)
                .map(|val| Uuid::parse_str(&val))?
                .map_err(|e| duckdb::Error::FromSqlConversionFailure(0, Type::Any, Box::new(e)))?,
            created_at: parse_timestamp_or_error(row.get::<usize, String>(1)?, 1)?,
            total_competition_pool: row.get::<usize, u64>(2)?,
            total_allowed_entries: row.get::<usize, u64>(3)?,
            number_of_places_win: row.get(4)?,
            entry_fee: row.get::<usize, u64>(5)?,
            event_announcement: row.get::<usize, Option<Value>>(6).map(|opt| match opt {
                Some(Value::Blob(val)) => serde_json::from_slice::<EventLockingConditions>(&val)
                    .map_err(|e| {
                        duckdb::Error::FromSqlConversionFailure(6, Type::Any, Box::new(e))
                    }),
                _ => Err(duckdb::Error::FromSqlConversionFailure(
                    6,
                    Type::Any,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Missing required event announcement data",
                    )),
                )),
            })??,

            total_entries: row.get::<usize, u64>(7)?,
            total_entry_nonces: row.get::<usize, u64>(8)?,
            total_signed_entries: row.get::<usize, u64>(9)?,
            total_paid_entries: row.get::<usize, u64>(10)?,

            funding_transaction: row.get::<usize, Option<Value>>(11).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            funding_outpoint: row.get::<usize, Option<Value>>(12).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            contract_parameters: row.get::<usize, Option<Value>>(13).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            public_nonces: row.get::<usize, Option<Value>>(14).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            aggregated_nonces: row.get::<usize, Option<Value>>(15).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            partial_signatures: row.get::<usize, Option<Value>>(16).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            signed_contract: row.get::<usize, Option<Value>>(17).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            cancelled_at: parse_optional_timestamp(row.get::<usize, Option<String>>(18)?, 18)?,
            contracted_at: parse_optional_timestamp(row.get::<usize, Option<String>>(19)?, 19)?,
            signed_at: parse_optional_timestamp(row.get::<usize, Option<String>>(20)?, 20)?,
            funding_broadcasted_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(21)?,
                21,
            )?,
            failed_at: parse_optional_timestamp(row.get::<usize, Option<String>>(22)?, 22)?,

            errors: row
                .get::<usize, Option<Value>>(23)
                .map(|opt| {
                    opt.and_then(|raw| match raw {
                        Value::Blob(val) => {
                            serde_json::from_slice::<Vec<CompetitionError>>(&val).ok()
                        }
                        _ => Some(Vec::new()),
                    })
                })
                .unwrap_or_else(|_| Some(Vec::new()))
                .unwrap_or_default(),
        };
        Ok(competition)
    }
}

#[derive(thiserror::Error, Debug, Serialize, Clone, Deserialize)]
pub enum CompetitionError {
    #[error("Failed to create transaction: {0}")]
    FailedCreateTransaction(String),
    #[error("Failed to broadcast error: {0}")]
    FailedBroadcast(String),
    #[error("Failed to aggregate nonces: {0}")]
    FailedNonceAggregation(String),
    #[error("Competition expired: {0}")]
    Expired(String),
    #[error("Invalid state transition: {0}")]
    InvalidStateTransition(String),
}

impl<'a> TryFrom<&Row<'a>> for Ticket {
    type Error = duckdb::Error;

    fn try_from(row: &Row) -> Result<Self, Self::Error> {
        Ok(Ticket {
            id: Uuid::parse_str(&row.get::<usize, String>(0)?)
                .map_err(|e| duckdb::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e)))?,
            competition_id: Uuid::parse_str(&row.get::<usize, String>(1)?)
                .map_err(|e| duckdb::Error::FromSqlConversionFailure(1, Type::Text, Box::new(e)))?,
            entry_id: row
                .get::<usize, Option<String>>(2)?
                .map(|s| {
                    Uuid::parse_str(&s).map_err(|e| {
                        duckdb::Error::FromSqlConversionFailure(2, Type::Text, Box::new(e))
                    })
                })
                .transpose()?,
            encrypted_preimage: row.get(3)?,
            hash: row.get(4)?,
            payment_request: row.get(5)?,
            expiry: parse_timestamp_or_error(row.get::<usize, String>(6)?, 6)?,
            reserved_by: row.get(7)?,
            reserved_at: parse_optional_timestamp(row.get::<usize, Option<String>>(8)?, 8)?,
            paid_at: parse_optional_timestamp(row.get::<usize, Option<String>>(9)?, 9)?,
        })
    }
}

fn parse_timestamp_or_error(
    timestamp: String,
    position: usize,
) -> Result<OffsetDateTime, duckdb::Error> {
    //raw date format 2024-08-11 00:27:39.013046-04
    let sql_time_format = format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second][optional [.[subsecond]]][offset_hour]"
    );

    OffsetDateTime::parse(&timestamp, &sql_time_format)
        .map_err(|e| duckdb::Error::FromSqlConversionFailure(position, Type::Text, Box::new(e)))
}

fn parse_optional_timestamp(
    timestamp: Option<String>,
    position: usize,
) -> Result<Option<OffsetDateTime>, duckdb::Error> {
    //raw date format 2024-08-11 00:27:39.013046-04
    let sql_time_format = format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second][optional [.[subsecond]]][offset_hour]"
    );

    match timestamp {
        Some(ts) => OffsetDateTime::parse(&ts, &sql_time_format)
            .map(Some)
            .map_err(|e| {
                duckdb::Error::FromSqlConversionFailure(position, Type::Text, Box::new(e))
            }),
        None => Ok(None),
    }
}
