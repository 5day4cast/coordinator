mod coordinator;
mod db_migrations;
mod store;
use crate::{oracle_client::WeatherChoices, AddEventEntry, Ln};
use anyhow::anyhow;
pub use coordinator::*;
pub use db_migrations::*;
use dlctix::{
    bitcoin::{hex::DisplayHex, OutPoint, Transaction},
    hashlock,
    musig2::{AggNonce, PartialSignature, PubNonce},
    secp::MaybeScalar,
    ContractParameters, EventLockingConditions, Outcome, SigMap, SignedContract,
};
use duckdb::{
    arrow::datatypes::ArrowNativeType,
    types::{Type, Value},
    Row,
};
use log::{debug, error};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};
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
    /// User's entry submission data (should be able to update until all entries have been collected)
    pub entry_submission: AddEventEntry,
    /// User provided private de-encrypted, only used during payout
    pub ephemeral_privatekey: Option<String>,
    /// User provided preimage de-encrypted, only used during payout
    pub payout_preimage: Option<String>,
    /// User provided lightning invoice, coordinator pays to user
    pub payout_ln_invoice: Option<String>,
    pub public_nonces: Option<SigMap<PubNonce>>,
    /// User signed funding psbt
    pub funding_psbt_base64: Option<String>,
    pub partial_signatures: Option<SigMap<PartialSignature>>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub signed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_out_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub sellback_broadcasted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub reclaimed_broadcasted_at: Option<OffsetDateTime>,
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

        let entry_submission: AddEventEntry = match row.get::<usize, Option<Value>>(10)? {
            Some(Value::Blob(blob_data)) => serde_json::from_slice(&blob_data).map_err(|e| {
                duckdb::Error::FromSqlConversionFailure(10, Type::Blob, Box::new(e))
            })?,
            Some(_) => {
                return Err(duckdb::Error::FromSqlConversionFailure(
                    10,
                    Type::Blob,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Expected blob",
                    )),
                ))
            }
            None => {
                return Err(duckdb::Error::FromSqlConversionFailure(
                    10,
                    Type::Blob,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Missing entry_submission",
                    )),
                ))
            }
        };

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
            funding_psbt_base64: row.get(9)?,
            entry_submission,
            payout_preimage_encrypted: row.get(11)?,
            payout_hash: row.get(12)?,
            payout_preimage: row.get::<usize, Option<String>>(13)?,
            payout_ln_invoice: row.get::<usize, Option<String>>(14)?,
            signed_at: parse_optional_timestamp(row.get::<usize, Option<String>>(15)?, 15)?,
            paid_at: parse_optional_timestamp(row.get::<usize, Option<String>>(16)?, 16)?,
            paid_out_at: parse_optional_timestamp(row.get::<usize, Option<String>>(17)?, 17)?,
            sellback_broadcasted_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(18)?,
                18,
            )?,
            reclaimed_broadcasted_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(19)?,
                19,
            )?,
        };
        Ok(user_entry)
    }
}

impl AddEntry {
    fn into_user_entry(self, pubkey: String) -> UserEntry {
        let entry_submission = self.clone().into();
        UserEntry {
            id: self.id,
            event_id: self.event_id,
            ticket_id: self.ticket_id,
            pubkey,
            ephemeral_pubkey: self.ephemeral_pubkey,
            ephemeral_privatekey_encrypted: self.ephemeral_privatekey_encrypted,
            payout_hash: self.payout_hash,
            payout_preimage_encrypted: self.payout_preimage_encrypted,
            entry_submission,
            signed_at: None,
            funding_psbt_base64: None,
            partial_signatures: None,
            public_nonces: None,
            ephemeral_privatekey: None,
            payout_preimage: None,
            payout_ln_invoice: None,
            paid_at: None,
            paid_out_at: None,
            sellback_broadcasted_at: None,
            reclaimed_broadcasted_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayoutInfo {
    pub ticket_id: Uuid,
    /// We are providing the payout preimage & ephemeral private key before being paid,
    /// At this point we are trusting that the coordinator will not sweep our funds
    /// prior to paying us out via the ln invoice (the funds are still controlled by us,
    /// but now we're also giving access to the coordinator). If the coordinator is
    /// malicious we would need to broadcast the split transaction before them to get paid.
    /// The coordinator is incentivized to wait until all users have paid out as their reclaim/closing
    /// transaction becomes much cheaper. In a perfect world we would encrypt this preimage & ephemeral private key
    /// via AES with the ln_invoice's preimage and add a zkproof to allow the coordinator to
    /// validate the data before paying the invoice. We are skipping that here for simplicity
    /// and leaning on the incentives of the coordinator to run a fair game and not steal the funds.
    /// This allows us to support far more payment methods for playing in the game and seems "good enough".
    /// If we find in actual practice that this is not the case, we can implement the more secure solution.
    pub payout_preimage: String,
    pub ephemeral_private_key: String,
    pub ln_invoice: String,
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
    /// Pubkey created for this entry for the user
    pub ephemeral_pubkey: Option<String>,
    pub reserved_by: Option<String>,
    pub reserved_at: Option<OffsetDateTime>,
    pub paid_at: Option<OffsetDateTime>,
    pub settled_at: Option<OffsetDateTime>,
    pub escrow_transaction: Option<String>, // Hex-encoded escrow transaction
}

impl Ticket {
    pub fn get_status(&self) -> TicketStatus {
        let now = OffsetDateTime::now_utc();

        if self.entry_id.is_some() {
            return TicketStatus::Used;
        }

        if now > self.expiry {
            return TicketStatus::Expired;
        }

        if self.settled_at.is_some() {
            return TicketStatus::Settled;
        }

        if self.paid_at.is_some() {
            return TicketStatus::Paid;
        }

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
    Settled,   // HODL invoice settled
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
    /// Time at which the attestation will be added to the event, needs to be after the end observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations start, all entries must be made before this time, must be before the end observation date
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations ends, must be before the signing date
    pub end_observation_date: OffsetDateTime,
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
    /// Percentage of entry fee that goes to the coordinator
    pub coordinator_fee_percentage: usize,
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

#[derive(Debug, Clone, Deserialize)]
pub struct Competition {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub event_submission: CreateEvent,
    pub total_entries: u64,
    pub total_entry_nonces: u64,
    pub total_signed_entries: u64,
    pub total_paid_entries: u64,
    pub total_paid_out_entries: usize,
    pub event_announcement: Option<EventLockingConditions>,
    pub funding_outpoint: Option<OutPoint>,
    pub funding_psbt_base64: Option<String>,
    pub funding_transaction: Option<Transaction>,
    pub outcome_transaction: Option<Transaction>,
    pub contract_parameters: Option<ContractParameters>,
    pub public_nonces: Option<SigMap<PubNonce>>,
    pub aggregated_nonces: Option<SigMap<AggNonce>>,
    pub partial_signatures: Option<SigMap<PartialSignature>>,
    pub signed_contract: Option<SignedContract>,
    /// Oracle attestation checked for and added once oracle announces
    pub attestation: Option<MaybeScalar>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub contracted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub signed_at: Option<OffsetDateTime>,
    /// Escrow transactions are considered settled after 1 confirmation by default
    #[serde(with = "time::serde::rfc3339::option")]
    pub escrow_funds_confirmed_at: Option<OffsetDateTime>,
    /// When the coordinator successfully creates event on the oracle
    #[serde(with = "time::serde::rfc3339::option")]
    pub event_created_at: Option<OffsetDateTime>,
    /// When the coordinator successfully batch sends all entries to the oracle
    #[serde(with = "time::serde::rfc3339::option")]
    pub entries_submitted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_broadcasted_at: Option<OffsetDateTime>,
    /// Funding transaction is considered settled after 1 confirmation by default
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_confirmed_at: Option<OffsetDateTime>,
    /// Funding transaction is considered settled after all hold invoices have been closed
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_settled_at: Option<OffsetDateTime>,
    /// Expiry transaction is broadcasted after event has expired
    #[serde(with = "time::serde::rfc3339::option")]
    pub expiry_broadcasted_at: Option<OffsetDateTime>,
    /// Outcome transaction is broadcasted after the attestation is provided
    #[serde(with = "time::serde::rfc3339::option")]
    pub outcome_broadcasted_at: Option<OffsetDateTime>,
    /// First delta transactions have been broadcasted via the coordinator
    #[serde(with = "time::serde::rfc3339::option")]
    pub delta_broadcasted_at: Option<OffsetDateTime>,
    /// All reclaim transaction have been broadcasted if needed, otherwise marked as completed
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub failed_at: Option<OffsetDateTime>,
    pub errors: Vec<CompetitionError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendCompetition {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub event_submission: CreateEvent,
    pub total_entries: u64,
    pub total_entry_nonces: u64,
    pub total_signed_entries: u64,
    pub total_paid_entries: u64,
    pub total_paid_out_entries: usize,
    pub event_announcement: Option<EventLockingConditions>,
    pub funding_transaction: Option<Transaction>,
    pub funding_outpoint: Option<OutPoint>,
    pub funding_psbt_base64: Option<String>,
    pub outcome_transaction: Option<Transaction>,
    pub contract_parameters: Option<ContractParameters>,
    pub public_nonces: Option<SigMap<PubNonce>>,
    pub aggregated_nonces: Option<SigMap<AggNonce>>,
    pub partial_signatures: Option<SigMap<PartialSignature>>,
    pub signed_contract: Option<SignedContract>,
    /// Oracle attestation checked for and added once oracle announces
    pub attestation: Option<MaybeScalar>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub contracted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub signed_at: Option<OffsetDateTime>,
    /// When the coordinator successfully creates event on the oracle
    #[serde(with = "time::serde::rfc3339::option")]
    pub event_created_at: Option<OffsetDateTime>,
    /// When the coordinator successfully batch sends all entries to the oracle
    #[serde(with = "time::serde::rfc3339::option")]
    pub entries_submitted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub escrow_funds_confirmed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_broadcasted_at: Option<OffsetDateTime>,
    /// Funding transaction is considered settled after 1 confirmation by default
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_confirmed_at: Option<OffsetDateTime>,
    /// Funding transaction is considered settled after all hold invoices have been closed
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_settled_at: Option<OffsetDateTime>,
    /// Expiry transaction is broadcasted after event has expired
    #[serde(with = "time::serde::rfc3339::option")]
    pub expiry_broadcasted_at: Option<OffsetDateTime>,
    /// Outcome transaction is broadcasted after the attestation is provided
    #[serde(with = "time::serde::rfc3339::option")]
    pub outcome_broadcasted_at: Option<OffsetDateTime>,
    /// First delta transactions have been broadcasted via the coordinator
    #[serde(with = "time::serde::rfc3339::option")]
    pub delta_broadcasted_at: Option<OffsetDateTime>,
    /// All reclaim transaction have been broadcasted if needed, otherwise marked as completed
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub failed_at: Option<OffsetDateTime>,
    pub errors: Vec<CompetitionError>,
    pub state: String,
}

impl From<Competition> for ExtendCompetition {
    fn from(competition: Competition) -> Self {
        let state = competition.get_state().to_string();
        Self {
            id: competition.id,
            created_at: competition.created_at,
            event_submission: competition.event_submission,
            event_announcement: competition.event_announcement,
            total_entries: competition.total_entries,
            total_entry_nonces: competition.total_entry_nonces,
            total_signed_entries: competition.total_signed_entries,
            total_paid_entries: competition.total_paid_entries,
            total_paid_out_entries: competition.total_paid_out_entries,
            funding_transaction: competition.funding_transaction,
            funding_outpoint: competition.funding_outpoint,
            outcome_transaction: competition.outcome_transaction,
            contract_parameters: competition.contract_parameters,
            public_nonces: competition.public_nonces,
            aggregated_nonces: competition.aggregated_nonces,
            partial_signatures: competition.partial_signatures,
            signed_contract: competition.signed_contract,
            attestation: competition.attestation,
            cancelled_at: competition.cancelled_at,
            contracted_at: competition.contracted_at,
            signed_at: competition.signed_at,
            event_created_at: competition.event_created_at,
            entries_submitted_at: competition.entries_submitted_at,
            funding_psbt_base64: competition.funding_psbt_base64,
            escrow_funds_confirmed_at: competition.escrow_funds_confirmed_at,
            funding_broadcasted_at: competition.funding_broadcasted_at,
            funding_confirmed_at: competition.funding_confirmed_at,
            funding_settled_at: competition.funding_settled_at,
            expiry_broadcasted_at: competition.expiry_broadcasted_at,
            outcome_broadcasted_at: competition.outcome_broadcasted_at,
            delta_broadcasted_at: competition.delta_broadcasted_at,
            completed_at: competition.completed_at,
            failed_at: competition.failed_at,
            errors: competition.errors,
            state,
        }
    }
}

impl Serialize for Competition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let ext: ExtendCompetition = self.to_owned().into();
        // Serialize the modified map
        ext.serialize(serializer)
    }
}

const TICKET_EXPIRY_BUFFER: Duration = Duration::minutes(1);

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
        let now = OffsetDateTime::now_utc();

        let latest_signing_end =
            self.event_submission.start_observation_date - TICKET_EXPIRY_BUFFER;

        if now >= latest_signing_end {
            return Err(Error::TooLateToSign(latest_signing_end, now));
        }

        let signing_window = latest_signing_end - now;
        let total_expiry_secs = signing_window.whole_seconds() as u64;

        Ok((total_expiry_secs, latest_signing_end))
    }

    fn get_current_outcome(&self) -> Result<Outcome, anyhow::Error> {
        let Some(attestation) = self.attestation else {
            return Err(anyhow!("No attestation found for competition {}", self.id));
        };
        debug!("Found attestation: {:?}", attestation);

        let Some(ref event_announcement) = self.event_announcement else {
            return Err(anyhow!(
                "Event announcement not found for competition {}",
                self.id,
            ));
        };

        let locking_point = attestation.base_point_mul();
        debug!("Found event_announcement: {:?}", event_announcement);

        let outcome = event_announcement
            .all_outcomes()
            .into_iter()
            .find(|outcome| {
                match outcome {
                    Outcome::Attestation(i) => {
                        // Check if this outcome's locking point matches our attestation point
                        event_announcement.locking_points[*i] == locking_point
                    }
                    Outcome::Expiry => false,
                }
            })
            .ok_or_else(|| anyhow!("No matching outcome found for attestation"))?;
        debug!("Found outcome: {:?}", outcome);
        Ok(outcome)
    }

    pub fn verify_event_attestation(&self, attestation: &MaybeScalar) -> Result<Outcome, Error> {
        let Some(ref event_announcement) = self.event_announcement else {
            return Err(Error::BadRequest("Event announcement not found".into()));
        };

        let Some(_) = self.signed_contract else {
            return Err(Error::BadRequest("Signed contract not found".into()));
        };

        if let Some(expiry) = event_announcement.expiry {
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| Error::BadRequest(format!("Failed to get current time: {}", e)))?
                .as_secs() as u32;

            if current_time >= expiry {
                debug!("Contract has expired, returning Expiry outcome");
                return Ok(Outcome::Expiry);
            }
        }

        let attestation_point = attestation.base_point_mul();
        debug!("Calculated attestation point: {:?}", attestation_point);

        // Log all locking points for debugging
        debug!(
            "Event announcement has {} locking points",
            event_announcement.locking_points.len()
        );
        for (idx, locking_point) in event_announcement.locking_points.iter().enumerate() {
            debug!("Locking point[{}]: {:?}", idx, locking_point);
            debug!(
                "Match with attestation: {}",
                locking_point == &attestation_point
            );
        }

        // Get all outcomes and log them
        let outcome_count = event_announcement.locking_points.len()
            + if event_announcement.expiry.is_some() {
                1
            } else {
                0
            };
        debug!("Total outcomes: {}", outcome_count);

        let all_outcomes = event_announcement.all_outcomes();
        // Find which outcome this attestation corresponds to
        let outcome = all_outcomes
            .into_iter()
            .find(|outcome| {
                match outcome {
                    Outcome::Attestation(i) => {
                        debug!("Checking outcome index {}", i);
                        // Check bounds first
                        if *i >= event_announcement.locking_points.len() {
                            error!(
                                "Outcome index {} is out of bounds for locking_points (len: {})",
                                i,
                                event_announcement.locking_points.len()
                            );
                            return false;
                        }

                        let matches = event_announcement.locking_points[*i] == attestation_point;
                        debug!("Outcome {} matches attestation: {}", i, matches);
                        matches
                    }
                    Outcome::Expiry => {
                        debug!("Skipping expiry outcome");
                        false
                    }
                }
            })
            .ok_or_else(|| {
                error!("No outcome matched the attestation point");
                Error::BadRequest("Attestation doesn't match any valid outcome".into())
            })?;

        if !event_announcement.is_valid_outcome(&outcome) {
            return Err(Error::BadRequest("Invalid outcome for this event".into()));
        }

        debug!("Found matching outcome: {:?}", outcome);
        Ok(outcome)
    }

    pub fn calculate_invoice_amount(&self) -> u64 {
        let fee_multiplier = self.event_submission.coordinator_fee_percentage as f64 / 100.0;
        let coordinator_fee =
            (self.event_submission.entry_fee as f64 * fee_multiplier).round() as u64;

        (self.event_submission.entry_fee as u64) + coordinator_fee
    }

    // We add the fee for the coordinator's service at this point in the process,
    // A user can not enter into the competition without paying the fee
    async fn create_ticket(
        &self,
        expiry_secs: u64,
        expiry_time: OffsetDateTime,
        ln_client: &Arc<dyn Ln>,
    ) -> Result<Ticket, Error> {
        let ticket_preimage = hashlock::preimage_random(&mut rand::thread_rng());
        let ticket_hash = hashlock::sha256(&ticket_preimage);
        let invoice_amount = self.calculate_invoice_amount();

        // Create a regular invoice instead of a HODL invoice
        let invoice = ln_client
            .add_invoice(
                invoice_amount,
                expiry_secs,
                format!("DLC Ticket for competition {}", self.id),
                self.id,
            )
            .await
            .map_err(Error::LnError)?;

        Ok(Ticket {
            id: Uuid::now_v7(),
            competition_id: self.id,
            entry_id: None,
            ephemeral_pubkey: None,
            encrypted_preimage: ticket_preimage.to_lower_hex_string(), // TODO: encrypt this
            hash: ticket_hash.to_lower_hex_string(),
            payment_request: invoice.payment_request,
            expiry: expiry_time,
            reserved_by: None,
            reserved_at: None,
            paid_at: None,
            settled_at: None,
            escrow_transaction: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundedContract {
    pub contract_params: ContractParameters,
    pub funding_outpoint: OutPoint,
    pub funding_psbt_base64: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompetitionState {
    Created,
    EntriesCollected,
    /// Verify escrow transactions have confirmed at least once (default), needed to broadcast funding transaction
    EscrowFundsConfirmed,
    EventCreated,
    EntriesSubmitted,
    ContractCreated,
    NoncesCollected,
    AggregateNoncesGenerated,
    PartialSignaturesCollected,
    SigningComplete,
    FundingBroadcasted,
    /// Once funding transaction has been confirmed at least once (default)
    FundingConfirmed,
    /// Once all hold invoices have settled/tickets have been paid
    FundingSettled,
    /// Oracle has attested to the results
    Attested,
    /// Oracle event has expired & players refunded before an attestation was provided
    ExpiryBroadcasted,
    /// Outcome transaction has been broadcasted
    OutcomeBroadcasted,
    /// First Delta transactions have been broadcasted
    DeltaBroadcasted,
    /// Closing transactions (second delta) have been broadcasted
    Completed,
    Failed,
    Cancelled,
}

impl fmt::Display for CompetitionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompetitionState::Created => write!(f, "created"),
            CompetitionState::EventCreated => write!(f, "event_created"),
            CompetitionState::EntriesSubmitted => write!(f, "entries_submitted"),
            CompetitionState::EscrowFundsConfirmed => write!(f, "escrow_funds_confirmed"),
            CompetitionState::EntriesCollected => write!(f, "entries_collected"),
            CompetitionState::ContractCreated => write!(f, "contract_created"),
            CompetitionState::NoncesCollected => write!(f, "nonces_collected"),
            CompetitionState::AggregateNoncesGenerated => write!(f, "aggregate_nonces_generated"),
            CompetitionState::PartialSignaturesCollected => {
                write!(f, "partial_signatures_collected")
            }
            CompetitionState::SigningComplete => write!(f, "signing_complete"),
            CompetitionState::FundingBroadcasted => write!(f, "funding_broadcasted"),
            CompetitionState::FundingConfirmed => write!(f, "funding_confirmed"),
            CompetitionState::FundingSettled => write!(f, "funding_settled"),
            CompetitionState::Attested => write!(f, "attested"),
            CompetitionState::ExpiryBroadcasted => write!(f, "expiry_broadcasted"),
            CompetitionState::OutcomeBroadcasted => write!(f, "outcome_broadcasted"),
            CompetitionState::DeltaBroadcasted => write!(f, "delta_broadcasted"),
            CompetitionState::Completed => write!(f, "completed"),
            CompetitionState::Failed => write!(f, "failed"),
            CompetitionState::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl Competition {
    pub fn new(create_event: &CreateEvent) -> Self {
        Self {
            id: create_event.id,
            created_at: OffsetDateTime::now_utc(),
            event_submission: create_event.clone(),
            total_entries: 0,
            total_entry_nonces: 0,
            total_signed_entries: 0,
            total_paid_entries: 0,
            total_paid_out_entries: 0,
            event_announcement: None,
            funding_transaction: None,
            outcome_transaction: None,
            funding_outpoint: None,
            funding_psbt_base64: None,
            contract_parameters: None,
            public_nonces: None,
            aggregated_nonces: None,
            attestation: None,
            cancelled_at: None,
            contracted_at: None,
            signed_at: None,
            signed_contract: None,
            partial_signatures: None,
            event_created_at: None,
            entries_submitted_at: None,
            escrow_funds_confirmed_at: None,
            funding_broadcasted_at: None,
            funding_confirmed_at: None,
            funding_settled_at: None,
            expiry_broadcasted_at: None,
            outcome_broadcasted_at: None,
            delta_broadcasted_at: None,
            completed_at: None,
            failed_at: None,
            errors: vec![],
        }
    }
    pub fn has_full_entries(&self) -> bool {
        (self.total_entries > 0)
            && self.total_entries.as_usize() >= self.event_submission.total_allowed_entries
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

    pub fn is_delta_broadcasted(&self) -> bool {
        self.delta_broadcasted_at.is_some()
    }

    pub fn is_funding_broadcasted(&self) -> bool {
        self.funding_broadcasted_at.is_some()
    }

    pub fn skip_competition(&self) -> bool {
        self.is_failed() || self.is_cancelled() || self.is_completed()
    }

    pub fn is_expiry_broadcasted(&self) -> bool {
        self.expiry_broadcasted_at.is_some()
    }

    pub fn is_outcome_broadcasted(&self) -> bool {
        self.outcome_broadcasted_at.is_some()
    }

    pub fn is_completed(&self) -> bool {
        self.completed_at.is_some()
    }

    pub fn has_all_entries_paid_out(&self) -> bool {
        self.total_paid_out_entries >= self.event_submission.number_of_places_win
    }

    pub fn is_attested(&self) -> bool {
        self.attestation.is_some()
    }

    pub fn is_signed(&self) -> bool {
        self.signed_at.is_some()
    }

    pub fn is_entries_submitted(&self) -> bool {
        self.entries_submitted_at.is_some()
    }

    pub fn is_event_created(&self) -> bool {
        self.event_created_at.is_some()
    }

    pub fn is_funding_settled(&self) -> bool {
        self.funding_settled_at.is_some()
    }

    pub fn is_funding_confirmed(&self) -> bool {
        self.funding_confirmed_at.is_some()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled_at.is_some()
    }

    pub fn is_escrow_funds_confirmed(&self) -> bool {
        self.escrow_funds_confirmed_at.is_some()
    }

    pub fn is_failed(&self) -> bool {
        self.failed_at.is_some()
    }

    pub fn should_abort(&self) -> bool {
        self.errors.len() > 5
    }

    pub fn is_expired(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        let Some(ref event_announcement) = self.event_announcement else {
            return false;
        };

        // Competition should expire if not enough entries collected before observation date
        if let Some(expiry) = event_announcement.expiry {
            if now.unix_timestamp() as u32 >= expiry && !self.has_full_entries() {
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
    // States change bottom up, so a state that doesn't match any of the conditionals is the first state (ie. Created)
    pub fn get_state(&self) -> CompetitionState {
        if self.is_cancelled() {
            return CompetitionState::Cancelled;
        }
        if self.is_failed() {
            return CompetitionState::Failed;
        }
        if self.is_completed() {
            return CompetitionState::Completed;
        }
        if self.is_delta_broadcasted() {
            return CompetitionState::DeltaBroadcasted;
        }
        if self.is_outcome_broadcasted() {
            return CompetitionState::OutcomeBroadcasted;
        }
        if self.is_expiry_broadcasted() {
            return CompetitionState::ExpiryBroadcasted;
        }
        if self.is_attested() {
            return CompetitionState::Attested;
        }
        if self.is_funding_settled() {
            return CompetitionState::FundingSettled;
        }
        if self.is_funding_confirmed() {
            return CompetitionState::FundingConfirmed;
        }
        if self.is_funding_broadcasted() {
            return CompetitionState::FundingBroadcasted;
        }
        if self.is_signed() {
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
        if self.contract_parameters.is_some() {
            return CompetitionState::ContractCreated;
        }
        if self.is_entries_submitted() {
            return CompetitionState::EntriesSubmitted;
        }
        if self.is_event_created() {
            return CompetitionState::EventCreated;
        }
        if self.is_escrow_funds_confirmed() {
            return CompetitionState::EscrowFundsConfirmed;
        }
        if self.has_full_entries() && self.has_all_entries_paid() {
            return CompetitionState::EntriesCollected;
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
            event_submission: row.get::<usize, Option<Value>>(2).map(|opt| match opt {
                Some(Value::Blob(val)) => {
                    serde_json::from_slice::<CreateEvent>(&val).map_err(|e| {
                        duckdb::Error::FromSqlConversionFailure(2, Type::Any, Box::new(e))
                    })
                }
                _ => Err(duckdb::Error::FromSqlConversionFailure(
                    2,
                    Type::Any,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Missing required created event data",
                    )),
                )),
            })??,
            event_announcement: row.get::<usize, Option<Value>>(3).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,
            total_entries: row.get::<usize, u64>(4)?,
            total_entry_nonces: row.get::<usize, u64>(5)?,
            total_signed_entries: row.get::<usize, u64>(6)?,
            total_paid_entries: row.get::<usize, u64>(7)?,
            total_paid_out_entries: row.get(8)?,

            outcome_transaction: row.get::<usize, Option<Value>>(9).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,
            funding_psbt_base64: row.get(10)?,
            funding_outpoint: row.get::<usize, Option<Value>>(11).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            funding_transaction: row.get::<usize, Option<Value>>(12).map(|opt| {
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

            attestation: row.get::<usize, Option<Value>>(18).map(|opt| {
                opt.and_then(|raw| match raw {
                    Value::Blob(val) => serde_json::from_slice(&val).ok(),
                    _ => None,
                })
            })?,

            cancelled_at: parse_optional_timestamp(row.get::<usize, Option<String>>(19)?, 19)?,
            contracted_at: parse_optional_timestamp(row.get::<usize, Option<String>>(20)?, 20)?,
            signed_at: parse_optional_timestamp(row.get::<usize, Option<String>>(21)?, 21)?,
            escrow_funds_confirmed_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(22)?,
                22,
            )?,
            event_created_at: parse_optional_timestamp(row.get::<usize, Option<String>>(23)?, 23)?,
            entries_submitted_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(24)?,
                24,
            )?,
            funding_broadcasted_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(25)?,
                25,
            )?,
            funding_confirmed_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(26)?,
                26,
            )?,
            funding_settled_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(27)?,
                27,
            )?,
            expiry_broadcasted_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(28)?,
                28,
            )?,
            outcome_broadcasted_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(29)?,
                29,
            )?,
            delta_broadcasted_at: parse_optional_timestamp(
                row.get::<usize, Option<String>>(30)?,
                30,
            )?,
            completed_at: parse_optional_timestamp(row.get::<usize, Option<String>>(31)?, 31)?,
            failed_at: parse_optional_timestamp(row.get::<usize, Option<String>>(32)?, 32)?,

            errors: row
                .get::<usize, Option<Value>>(33)
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
    #[error("Failed to create event on oracle: {0}")]
    FailedCreateEvent(String),
    #[error("Failed to submit entries to oracle: {0}")]
    FailedSubmitEntries(String),
    #[error("Failed to check escrow transaction: {0}")]
    FailedEscrowConfirmation(String),
    #[error("Failed to broadcast error: {0}")]
    FailedBroadcast(String),
    #[error("Failed to check funding confirmation: {0}")]
    FailedFundingConfirmation(String),
    #[error("Failed to settled funding invoices: {0}")]
    FailedFundingSettled(String),
    #[error("Failed to aggregate nonces: {0}")]
    FailedNonceAggregation(String),
    #[error("Failed to check attestation: {0}")]
    FailedCheckingAttestation(String),
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
            ephemeral_pubkey: row.get(3)?,
            encrypted_preimage: row.get(4)?,
            hash: row.get(5)?,
            payment_request: row.get(6)?,
            expiry: parse_timestamp_or_error(row.get::<usize, String>(7)?, 7)?,
            reserved_by: row.get(8)?,
            reserved_at: parse_optional_timestamp(row.get::<usize, Option<String>>(9)?, 9)?,
            paid_at: parse_optional_timestamp(row.get::<usize, Option<String>>(10)?, 10)?,
            settled_at: parse_optional_timestamp(row.get::<usize, Option<String>>(11)?, 11)?,
            escrow_transaction: row.get::<usize, Option<String>>(12)?,
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
