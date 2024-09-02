use anyhow::anyhow;
use dlctix::{secp::Scalar, EventLockingConditions};
use log::{debug, error};
use reqwest_middleware::{
    self,
    reqwest::{Method, StatusCode, Url},
    ClientWithMiddleware, RequestBuilder,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{AddEntry, CreateEvent};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub id: Uuid,
    /// Nonce the oracle committed to use as part of signing final results
    pub nonce: Scalar,
    /// Holds the predefined outcomes the oracle will attest to at event completes
    pub event_annoucement: EventLockingConditions,
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
}

#[derive(Clone)]
pub struct OracleClient {
    pub base_url: Url,
    pub client: ClientWithMiddleware,
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

impl OracleClient {
    pub fn new(base_url: &Url, client: ClientWithMiddleware) -> Self {
        Self {
            base_url: base_url.to_owned(),
            client,
        }
    }

    pub async fn create_event(&self, event: CreateEvent) -> Result<Event, Error> {
        debug!("event: {:?}", event);
        let url = self
            .base_url
            .join("/oracle/events")
            .map_err(|e| Error::Request(e.to_string()))?;
        let req = self.client.request(Method::POST, url).json(&event);
        self.send_request::<Event>(req, String::from("event not found"))
            .await
    }

    pub async fn submit_entry(&self, entry: AddEventEntry) -> Result<(), Error> {
        let url = self
            .base_url
            .join(&format!("/oracle/events/{}/entry", entry.id))
            .map_err(|e| Error::Request(e.to_string()))?;
        let req = self.client.request(Method::POST, url).json(&entry);
        self.send_request(req, String::from("event not found"))
            .await
    }

    async fn send_request<T>(
        &self,
        request: RequestBuilder,
        not_found_message: String,
    ) -> Result<T, Error>
    where
        T: DeserializeOwned,
    {
        let response = request.send().await.map_err(|e| {
            error!("error sending to oracle: {}", e);
            Error::SendRetry(e)
        })?;

        if response.status().is_success() {
            let body: T = response.json::<T>().await?;
            Ok(body)
        } else if response.status() == StatusCode::NOT_FOUND {
            Err(Error::NotFound(not_found_message))
        } else if response.status() == StatusCode::BAD_REQUEST {
            Err(Error::BadRequest(
                response
                    .text()
                    .await
                    .unwrap_or(String::from("bad request to oracle")),
            ))
        } else {
            let status = response.status();

            let body = response.text().await?;

            Err(Error::Request(format!(
                "error response from oracle with status {}: {:?}",
                status, body
            )))
        }
    }
}
