mod competition_coordinator;
mod competition_data;
mod competition_db_migrations;

use crate::oracle_client::WeatherChoices;
use crate::ser::utc_datetime;
pub use competition_coordinator::*;
pub use competition_data::*;
pub use competition_db_migrations::*;
use dlctix::{bitcoin::hashes::sha256, musig2::secp256k1::Message};
use duckdb::{types::Type, Row};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddEntry {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    /// User pubkey used to login
    pub pubkey: String,
    /// User signature using private key assoicated to pubkey
    pub signature: String,
    pub event_id: Uuid,
    pub expected_observations: Vec<WeatherChoices>,
}

impl From<AddEntry> for AddEntryMessage {
    fn from(value: AddEntry) -> Self {
        AddEntryMessage {
            id: value.id,
            pubkey: value.pubkey,
            event_id: value.event_id,
            expected_observations: value.expected_observations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddEntryMessage {
    pub id: Uuid,
    pub pubkey: String,
    pub event_id: Uuid,
    pub expected_observations: Vec<WeatherChoices>,
}

impl AddEntryMessage {
    pub fn message(&self) -> Result<Message, Error> {
        let message_str = serde_json::to_string(self)?;
        let message = Message::from_hashed_data::<sha256::Hash>(message_str.as_bytes());
        Ok(message)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEntry {
    pub id: Uuid,
    pub event_id: Uuid,
    pub pubkey: String,
}

impl<'a> TryFrom<&Row<'a>> for UserEntry {
    type Error = duckdb::Error;

    fn try_from(row: &Row) -> Result<Self, Self::Error> {
        let user_entry = UserEntry {
            id: row
                .get::<usize, String>(0)
                .map(|val| Uuid::parse_str(&val))?
                .map_err(|e| duckdb::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e)))?,
            event_id: row
                .get::<usize, String>(1)
                .map(|val| Uuid::parse_str(&val))?
                .map_err(|e| duckdb::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e)))?,
            pubkey: row.get::<usize, String>(2)?,
        };
        Ok(user_entry)
    }
}

impl From<AddEntry> for UserEntry {
    fn from(value: AddEntry) -> Self {
        Self {
            id: value.id,
            event_id: value.event_id,
            pubkey: value.pubkey,
        }
    }
}

//TODO: add pagination when it's needed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchBy {
    /// Optionally add event_ids
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_ids: Option<Vec<Uuid>>,
    /// User key to use for finding their events
    pub pubkey: String,
    /// Signed with user key, used to validate the payload
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SearchByMessage {
    /// Optionally add event_ids
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_ids: Option<Vec<Uuid>>,
    /// User key to use for finding their events
    pub pubkey: String,
}

impl From<SearchBy> for SearchByMessage {
    fn from(value: SearchBy) -> Self {
        SearchByMessage {
            event_ids: value.event_ids,
            pubkey: value.pubkey,
        }
    }
}

impl SearchByMessage {
    pub fn message(&self) -> Result<Message, Error> {
        let message_str = serde_json::to_string(self)?;
        let message = Message::from_hashed_data::<sha256::Hash>(message_str.as_bytes());
        Ok(message)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEvent {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    #[serde(with = "utc_datetime")]
    /// Time at which the attestation will be added to the event, needs to be after the observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "utc_datetime")]
    /// Date of when the weather observations occured (midnight UTC), all entries must be made before this time
    pub observation_date: OffsetDateTime,
    /// NOAA observation stations used in this event
    pub locations: Vec<String>,
    /// The number of values that can be selected per entry in the event (default to number_of_locations * 3, (temp_low, temp_high, wind_speed))
    pub number_of_values_per_entry: usize,
    /// Total number of allowed entries into the event
    pub total_allowed_entries: usize,
    /// Total amount of places that are part of the winnings split
    pub number_of_places_win: usize,
    /// Add a coordinator that will use the event entries in a competition
    pub coordinator: Option<CoordinatorInfo>,
}

impl CreateEvent {
    pub fn new(create_event: CreateEventMessage, coordinator_info: CoordinatorInfo) -> Self {
        Self {
            id: create_event.id,
            signing_date: create_event.signing_date,
            observation_date: create_event.observation_date,
            locations: create_event.locations,
            number_of_values_per_entry: create_event.number_of_values_per_entry,
            total_allowed_entries: create_event.total_allowed_entries,
            number_of_places_win: create_event.number_of_places_win,
            coordinator: Some(coordinator_info),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEventMessage {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    #[serde(with = "utc_datetime")]
    /// Time at which the attestation will be added to the event, needs to be after the observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "utc_datetime")]
    /// Date of when the weather observations occured (midnight UTC), all entries must be made before this time
    pub observation_date: OffsetDateTime,
    /// NOAA observation stations used in this event
    pub locations: Vec<String>,
    /// The number of values that can be selected per entry in the event (default to number_of_locations * 3, (temp_low, temp_high, wind_speed))
    pub number_of_values_per_entry: usize,
    /// Total number of allowed entries into the event
    pub total_allowed_entries: usize,
    /// Total amount of places that are part of the winnings split
    pub number_of_places_win: usize,
}

impl CreateEventMessage {
    pub fn message(&self) -> Result<Message, serde_json::Error> {
        let message_str = serde_json::to_string(self)?;
        let message = Message::from_hashed_data::<sha256::Hash>(message_str.as_bytes());
        Ok(message)
    }
}

impl From<CreateEvent> for CreateEventMessage {
    fn from(value: CreateEvent) -> Self {
        Self {
            id: value.id,
            signing_date: value.signing_date,
            observation_date: value.observation_date,
            locations: value.locations,
            number_of_values_per_entry: value.number_of_values_per_entry,
            total_allowed_entries: value.total_allowed_entries,
            number_of_places_win: value.number_of_places_win,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorInfo {
    /// The pubkey of the coordinator
    pub pubkey: String,
    /// The values of the payload signed by the coordinator
    pub signature: String,
}
