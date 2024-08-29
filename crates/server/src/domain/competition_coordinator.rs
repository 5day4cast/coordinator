use anyhow::anyhow;
use dlctix::{
    bitcoin::{
        key::{Keypair, Secp256k1},
        XOnlyPublicKey,
    },
    musig2::secp256k1::{schnorr::Signature, Message, PublicKey, SecretKey},
};
use futures::TryFutureExt;
use log::{debug, error, info};
use std::sync::Arc;
use thiserror::Error;

use crate::{get_key, OracleClient, OracleError};

use super::{AddEntry, AddEntryMessage, CompetitionData, SearchBy, SearchByMessage, UserEntry};

pub struct Coordinator {
    oracle_client: Arc<OracleClient>,
    competiton_data: Arc<CompetitionData>,
    private_key: SecretKey,
    public_key: PublicKey,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("item not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("problem querying db: {0}")]
    DbError(#[from] duckdb::Error),
    #[error("{0}")]
    OracleFailed(#[from] OracleError),
    #[error("invalid signature for request")]
    InvalidSignature(String),
    #[error("invalid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

impl Coordinator {
    pub async fn new(
        oracle_client: OracleClient,
        competition_data: CompetitionData,
        private_key_file_path: &str,
    ) -> Result<Self, anyhow::Error> {
        let secret_key = get_key(private_key_file_path)?;
        let secp = Secp256k1::new();
        let public_key = secret_key.public_key(&secp);
        let coordinator = Self {
            oracle_client: Arc::new(oracle_client),
            competiton_data: Arc::new(competition_data),
            private_key: secret_key,
            public_key,
        };
        coordinator.validate_coordinator_metadata().await?;
        Ok(coordinator)
    }

    pub fn public_key(&self) -> String {
        let key = self.public_key.x_only_public_key().0.serialize();
        hex::encode(key)
    }

    pub fn keypair(&self) -> Keypair {
        let secp = Secp256k1::new();
        self.private_key.keypair(&secp)
    }

    pub async fn validate_coordinator_metadata(&self) -> Result<(), anyhow::Error> {
        let stored_public_key = match self.competiton_data.get_stored_public_key().await {
            Ok(key) => key,
            Err(duckdb::Error::QueryReturnedNoRows) => {
                self.add_metadata().await?;
                return Ok(());
            }
            Err(e) => return Err(anyhow!("error getting stored public key: {}", e)),
        };
        if stored_public_key != self.public_key.x_only_public_key().0 {
            return Err(anyhow!(
                "stored_pubkey: {:?} pem_pubkey: {:?}",
                stored_public_key,
                self.public_key()
            ));
        }
        Ok(())
    }

    async fn add_metadata(&self) -> Result<(), anyhow::Error> {
        self.competiton_data
            .add_coordinator_metadata(self.public_key.x_only_public_key().0)
            .await
            .map_err(|e| anyhow!("failed to add coordinator metadata: {}", e))
    }

    // Becareful with these two operations, there's a possibility here of an
    // entry being added to the oracle but never saved to our local DB (low, but possible)
    pub async fn add_entry(&self, entry: AddEntry) -> Result<UserEntry, Error> {
        let add_entry_msg: AddEntryMessage = entry.clone().into();
        info!("add_entry: {:?}", add_entry_msg);
        validate(add_entry_msg.message()?, &entry.pubkey, &entry.signature)?;
        match self.oracle_client.submit_entry(entry.clone().into()).await {
            Ok(_) => Ok(()),
            Err(OracleError::NotFound(e)) => Err(Error::NotFound(e)),
            Err(OracleError::BadRequest(e)) => Err(Error::BadRequest(e)),
            Err(e) => Err(Error::OracleFailed(e)),
        }?;

        let user_entry = self
            .competiton_data
            .add_entry(entry.clone().into())
            .map_err(|e| {
                error!(
                    "entry added to oracle, but failed to be saved: entry_id {}, event_id {} {:?}",
                    entry.id, entry.event_id, e
                );
                Error::DbError(e)
            })
            .await?;

        Ok(user_entry)
    }

    pub async fn get_entries(&self, filter: SearchBy) -> Result<Vec<UserEntry>, Error> {
        let filter_msg: SearchByMessage = filter.clone().into();
        validate(filter_msg.message()?, &filter.pubkey, &filter.signature)?;
        //TODO add validation of the search using signature
        self.competiton_data
            .get_entries(filter)
            .map_err(Error::DbError)
            .await
    }
}

/// Validates the recieved messages was created by the provided pubkey
fn validate(message: Message, pubkey: &str, signature: &str) -> Result<(), Error> {
    debug!("pubkey: {} signature: {}", pubkey, signature);
    let raw_signature: Vec<u8> = hex::decode(signature).unwrap();
    let raw_pubkey: Vec<u8> = hex::decode(pubkey).unwrap();
    let sig: Signature = Signature::from_slice(raw_signature.as_slice())
        .map_err(|e| Error::InvalidSignature(format!("invalid signature: {}", e)))?;
    let xonly_pubkey: XOnlyPublicKey = XOnlyPublicKey::from_slice(raw_pubkey.as_slice())
        .map_err(|e| Error::InvalidSignature(format!("invalid pubkey: {}", e)))?;
    sig.verify(&message, &xonly_pubkey).map_err(|e| {
        Error::InvalidSignature(format!(
            "invalid signature {} for pubkey {} {}",
            signature, pubkey, e
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod test {
    use uuid::Uuid;

    use crate::domain::{AddEntryMessage, WeatherChoices};
    use crate::oracle_client::ValueOptions;

    use super::validate;
    #[test]
    fn can_validate_message() {
        let pubkey =
            String::from("321076277059068487c58580356bc2113bd9491ac59bcced58aae9ec7d3f5bf8");
        let signature = String::from("854b2baf621fd32ae9aad4ff4fcf3872ca58af4c220e198130d017388ca7e2c71039c8306450bef0ae9c4812552ee235661a68319270c603c3a925631cd53950");
        let payload = AddEntryMessage {
            id: Uuid::parse_str("0191856c-c0ba-79a1-803b-8befb97ba7b8").unwrap(),
            pubkey: String::from(
                "321076277059068487c58580356bc2113bd9491ac59bcced58aae9ec7d3f5bf8",
            ),
            event_id: Uuid::parse_str("019184f9-8e9c-7097-9516-47bb0f6d3767").unwrap(),
            expected_observations: vec![
                WeatherChoices {
                    stations: String::from("KPHL"),
                    temp_high: None,
                    temp_low: None,
                    wind_speed: None,
                },
                WeatherChoices {
                    stations: String::from("KIAD"),
                    temp_high: None,
                    temp_low: None,
                    wind_speed: None,
                },
                WeatherChoices {
                    stations: String::from("KBOS"),
                    temp_high: None,
                    temp_low: None,
                    wind_speed: None,
                },
                WeatherChoices {
                    stations: String::from("KEWR"),
                    temp_high: None,
                    temp_low: Some(ValueOptions::Over),
                    wind_speed: None,
                },
                WeatherChoices {
                    stations: String::from("KLGA"),
                    temp_high: None,
                    temp_low: None,
                    wind_speed: Some(ValueOptions::Par),
                },
            ],
        };
        let message = payload.message().unwrap();
        validate(message, &pubkey, &signature).unwrap();
    }
}
