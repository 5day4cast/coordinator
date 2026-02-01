use anyhow::anyhow;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use dlctix::{
    secp::{MaybeScalar, Scalar},
    EventLockingConditions,
};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE};
use log::{debug, error};
use mime::APPLICATION_JSON;
use nostr_sdk::{
    hashes::Hash as Sha256Hash, secp256k1::SecretKey as Secp256k1SecretKey, Keys,
    SecretKey as NostrSecretKey,
};
use reqwest_middleware::{
    self,
    reqwest::{Method, Response, StatusCode, Url},
    ClientWithMiddleware,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    api::extractors::create_auth_event,
    domain::{AddEntry, CreateEvent},
    infra::secrets::get_key,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub id: Uuid,
    /// Nonce the oracle committed to use as part of signing final results
    pub nonce: Scalar,
    /// Holds the predefined outcomes the oracle will attest to at event completes
    pub event_announcement: EventLockingConditions,
    /// When added it means the oracle has signed that the current data is the final result
    pub attestation: Option<MaybeScalar>,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("item not found: {0}")]
    NotFound(String),
    #[error("problem sending request to oracle: {0}")]
    Send(#[from] reqwest_middleware::reqwest::Error),
    #[error("problem sending request to oracle: {0}")]
    SendRetry(#[from] reqwest_middleware::Error),
    #[error("problem requesting oracle: {0}")]
    Request(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("oracle temporarily unavailable: {0}")]
    Transient(String),
}

impl Error {
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Transient(_))
    }
}

#[derive(Clone)]
pub struct OracleClient {
    pub base_url: Url,
    pub client: ClientWithMiddleware,
    nostr_keys: Keys,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddEventEntries {
    /// Client needs to provide a valid Uuidv7
    pub event_id: Uuid,
    pub entries: Vec<AddEventEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddEventEntry {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    pub event_id: Uuid,
    pub expected_observations: Vec<WeatherChoices>,
}

impl From<AddEntry> for AddEventEntry {
    fn from(value: AddEntry) -> Self {
        Self {
            id: value.id,
            event_id: value.event_id,
            expected_observations: value.expected_observations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeatherChoices {
    // NOAA weather stations
    pub stations: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_speed: Option<ValueOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_high: Option<ValueOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_low: Option<ValueOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ValueOptions {
    Over,
    // Par is what was forecasted for this value
    Par,
    Under,
}

impl std::fmt::Display for ValueOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Over => write!(f, "over"),
            Self::Par => write!(f, "par"),
            Self::Under => write!(f, "under"),
        }
    }
}

impl TryFrom<&str> for ValueOptions {
    type Error = anyhow::Error;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "over" => Ok(ValueOptions::Over),
            "par" => Ok(ValueOptions::Par),
            "under" => Ok(ValueOptions::Under),
            val => Err(anyhow!("invalid option: {}", val)),
        }
    }
}

impl TryFrom<String> for ValueOptions {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_str() {
            "over" => Ok(ValueOptions::Over),
            "par" => Ok(ValueOptions::Par),
            "under" => Ok(ValueOptions::Under),
            val => Err(anyhow!("invalid option: {}", val)),
        }
    }
}

fn secp256k1_to_nostr_keys(secp_key: &Secp256k1SecretKey) -> Result<Keys, &'static str> {
    let key_bytes = secp_key.secret_bytes();

    let secret_key =
        NostrSecretKey::from_slice(&key_bytes).map_err(|_| "Failed to convert secret key")?;
    Ok(Keys::new(secret_key))
}

#[async_trait::async_trait]
pub trait Oracle: Send + Sync {
    async fn create_event(&self, event: CreateEvent) -> Result<Event, Error>;
    async fn get_event(&self, event_id: &Uuid) -> Result<Event, Error>;
    async fn submit_entries(&self, event_entries: AddEventEntries) -> Result<(), Error>;
}

impl OracleClient {
    pub fn new(
        client: ClientWithMiddleware,
        base_url: &Url,
        nostr_key_path: &str,
    ) -> Result<Self, Error> {
        let secret_key: Secp256k1SecretKey = get_key(nostr_key_path)
            .map_err(|e| Error::Request(format!("Failed to load nostr key: {}", e)))?;

        let nostr_keys = secp256k1_to_nostr_keys(&secret_key)
            .map_err(|e| Error::Request(format!("Invalid nostr key: {}", e)))?;

        Ok(Self {
            base_url: base_url.to_owned(),
            client,
            nostr_keys,
        })
    }

    async fn send_authenticated_request_internal(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        not_found_message: String,
    ) -> Result<Response, Error> {
        let payload_hash = body.as_deref().map(Sha256Hash::hash);

        let event = create_auth_event(
            method.as_str(),
            url.as_str(),
            payload_hash,
            &self.nostr_keys,
        )
        .await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(
                serde_json::to_string(&event)
                    .map_err(|e| Error::Request(format!("Failed to serialize event: {}", e)))?
            )
        );

        let mut request = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, auth_header)
            .header(CONTENT_TYPE, APPLICATION_JSON.to_string());

        if let Some(b) = body {
            request = request.body(b);
        }

        let response = request.send().await.map_err(|e| {
            error!("error sending to oracle: {}", e);
            Error::SendRetry(e)
        })?;

        if response.status().is_success() {
            Ok(response)
        } else if response.status() == StatusCode::NOT_FOUND {
            Err(Error::NotFound(not_found_message))
        } else if response.status() == StatusCode::BAD_REQUEST {
            Err(Error::BadRequest(
                response
                    .text()
                    .await
                    .unwrap_or(String::from("bad request to oracle")),
            ))
        } else if response.status() == StatusCode::SERVICE_UNAVAILABLE
            || response.status() == StatusCode::BAD_GATEWAY
            || response.status() == StatusCode::GATEWAY_TIMEOUT
            || response.status() == StatusCode::TOO_MANY_REQUESTS
        {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(Error::Transient(format!(
                "error response from oracle with status {}: {:?}",
                status, body
            )))
        } else {
            let status = response.status();
            let body = response.text().await?;
            Err(Error::Request(format!(
                "error response from oracle with status {}: {:?}",
                status, body
            )))
        }
    }

    pub async fn send_authenticated_request<T>(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        not_found_message: String,
    ) -> Result<T, Error>
    where
        T: DeserializeOwned,
    {
        let response = self
            .send_authenticated_request_internal(method, url, body, not_found_message)
            .await?;

        response.json::<T>().await.map_err(Into::into)
    }

    pub async fn send_authenticated_request_ignore_body(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        not_found_message: String,
    ) -> Result<(), Error> {
        self.send_authenticated_request_internal(method, url, body, not_found_message)
            .await?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl Oracle for OracleClient {
    async fn create_event(&self, event: CreateEvent) -> Result<Event, Error> {
        debug!("event: {:?}", event);
        let url = self
            .base_url
            .join("/oracle/events")
            .map_err(|e| Error::Request(e.to_string()))?;

        let body = serde_json::to_vec(&event)
            .map_err(|e| Error::Request(format!("Failed to serialize event: {}", e)))?;

        self.send_authenticated_request::<Event>(
            Method::POST,
            url,
            Some(body),
            String::from("event not found"),
        )
        .await
    }

    async fn get_event(&self, id: &Uuid) -> Result<Event, Error> {
        let url = self
            .base_url
            .join(&format!("/oracle/events/{}", id))
            .map_err(|e| Error::Request(e.to_string()))?;

        self.send_authenticated_request::<Event>(
            Method::GET,
            url,
            None,
            format!("event with id {} not found", id),
        )
        .await
    }

    async fn submit_entries(&self, event_entries: AddEventEntries) -> Result<(), Error> {
        let url = self
            .base_url
            .join(&format!(
                "/oracle/events/{}/entries",
                event_entries.event_id
            ))
            .map_err(|e| Error::Request(e.to_string()))?;

        let body = serde_json::to_vec(&event_entries)
            .map_err(|e| Error::Request(format!("Failed to serialize entries: {}", e)))?;

        self.send_authenticated_request_ignore_body(
            Method::POST,
            url,
            Some(body),
            String::from("event not found"),
        )
        .await
    }
}
