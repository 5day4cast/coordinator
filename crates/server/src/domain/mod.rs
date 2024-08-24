mod competition_coordinator;
mod competition_data;
mod competition_db_migrations;

pub use competition_coordinator::*;
pub use competition_data::*;
pub use competition_db_migrations::*;
use dlctix::{bitcoin::hashes::sha256, musig2::secp256k1::Message};
use duckdb::{types::Type, Row};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::oracle_client::WeatherChoices;

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
