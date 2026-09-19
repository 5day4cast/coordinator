use anyhow::anyhow;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use dlctix::{
    secp::{MaybeScalar, Point},
    EventLockingConditions,
};
use hyper::{
    header::{AUTHORIZATION, CONTENT_TYPE},
    http::Extensions,
};
use log::{debug, error};
use mime::APPLICATION_JSON;
use nostr_sdk::{
    hashes::Hash as Sha256Hash, secp256k1::SecretKey as Secp256k1SecretKey, Keys,
    SecretKey as NostrSecretKey,
};
use reqwest_middleware::{
    self,
    reqwest::{Method, Response, StatusCode, Url},
    ClientBuilder, ClientWithMiddleware, Middleware, Next,
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
    /// Public nonce committed by NOAA Oracle 2.0; the nonce secret is never served.
    pub nonce_point: Point,
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
}

/// Runs after the retry middleware so every attempt has a fresh authorization.
/// Reusing an event after a 503 is a replay, even when the write did not commit.
struct OracleAuth(Keys);

#[async_trait::async_trait]
impl Middleware for OracleAuth {
    async fn handle(
        &self,
        mut request: reqwest::Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let payload_hash = request
            .body()
            .map(|body| {
                body.as_bytes()
                    .map(Sha256Hash::hash)
                    .ok_or_else(|| anyhow!("oracle authorization requires a buffered body"))
            })
            .transpose()
            .map_err(reqwest_middleware::Error::Middleware)?;
        let event = create_auth_event(
            request.method().as_str(),
            request.url().as_str(),
            payload_hash,
            &self.0,
        )
        .await;
        let encoded = serde_json::to_vec(&event)
            .map_err(|error| reqwest_middleware::Error::Middleware(error.into()))?;
        let authorization = format!("Nostr {}", BASE64.encode(encoded))
            .parse()
            .map_err(|error| {
                reqwest_middleware::Error::Middleware(anyhow!(
                    "invalid authorization header: {error}"
                ))
            })?;
        request.headers_mut().insert(AUTHORIZATION, authorization);
        next.run(request, extensions).await
    }
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
            client: ClientBuilder::from_client(client)
                .with(OracleAuth(nostr_keys))
                .build(),
        })
    }

    async fn send_authenticated_request_internal(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        not_found_message: String,
    ) -> Result<Response, Error> {
        let mut request = self
            .client
            .request(method, url)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, extract::State, http::HeaderMap, routing::post, Json, Router};
    use nostr_sdk::nips::nip98::HttpData;
    use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use time::OffsetDateTime;

    fn event_config() -> CreateEvent {
        let start = OffsetDateTime::now_utc() + time::Duration::hours(1);
        CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + time::Duration::days(1),
            start_observation_date: start,
            end_observation_date: start + time::Duration::hours(12),
            locations: vec!["KORD".into()],
            number_of_values_per_entry: 1,
            number_of_places_win: 1,
            total_allowed_entries: 2,
            entry_fee: 1_000,
            coordinator_fee_percentage: 10,
            total_competition_pool: 1_800,
            relative_locktime_block_delta: None,
        }
    }

    fn event_response(id: Uuid) -> serde_json::Value {
        // NOAA Oracle 2.0 publishes only the nonce point, never the secret scalar.
        serde_json::json!({
            "id": id,
            "nonce_point": "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            "event_announcement": {
                "locking_points": ["0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"],
                "expiry": 1_900_000_000
            },
            "attestation": null,
            "source": "noaa_weather"
        })
    }

    #[test]
    fn accepts_noaa_v2_public_nonce_response() {
        let id = Uuid::now_v7();
        let event: Event = serde_json::from_value(event_response(id)).unwrap();
        assert_eq!(event.id, id);
        assert_eq!(
            event.nonce_point,
            dlctix::secp::Scalar::one().base_point_mul()
        );
        let encoded = serde_json::to_value(event).unwrap();
        assert!(encoded.get("nonce").is_none());
        assert!(encoded.get("nonce_point").is_some());
    }

    #[derive(Clone)]
    struct RetryServer {
        url: String,
        body: Vec<u8>,
        signer: nostr_sdk::PublicKey,
        seen: Arc<Mutex<HashSet<nostr_sdk::EventId>>>,
        response: serde_json::Value,
    }

    async fn retry_server(
        State(state): State<RetryServer>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let header = headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
        let auth: nostr_sdk::Event = serde_json::from_slice(
            &BASE64
                .decode(header.strip_prefix("Nostr ").unwrap())
                .unwrap(),
        )
        .unwrap();
        auth.verify().unwrap();
        assert_eq!(auth.pubkey, state.signer);
        let signed = HttpData::try_from(auth.tags.to_vec()).unwrap();
        assert_eq!(signed.url.as_str(), state.url);
        assert_eq!(signed.method.to_string(), "POST");
        assert_eq!(signed.payload, Some(Sha256Hash::hash(&body)));
        assert_eq!(body.as_ref(), state.body);
        let mut seen = state.seen.lock().unwrap();
        if !seen.insert(auth.id) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error":"replay"})),
            );
        }
        let status = if seen.len() == 1 {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        };
        (status, Json(state.response))
    }

    #[tokio::test]
    async fn oracle_retries_sign_fresh_events_bound_to_the_same_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let config = event_config();
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("coordinator.pem");
        let secret: Secp256k1SecretKey = get_key(key_path.to_str().unwrap()).unwrap();
        let state = RetryServer {
            url: base.join("/oracle/events").unwrap().to_string(),
            body: serde_json::to_vec(&config).unwrap(),
            signer: secp256k1_to_nostr_keys(&secret).unwrap().public_key(),
            seen: Arc::default(),
            response: event_response(config.id),
        };
        let seen = state.seen.clone();
        let app = Router::new()
            .route("/oracle/events", post(retry_server))
            .with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let policy = ExponentialBackoff::builder()
            .retry_bounds(Duration::from_millis(1), Duration::from_millis(1))
            .build_with_max_retries(1);
        let http = ClientBuilder::new(reqwest::Client::new())
            .with(RetryTransientMiddleware::new_with_policy(policy))
            .build();
        let client = OracleClient::new(http, &base, key_path.to_str().unwrap()).unwrap();
        let result = client.create_event(config.clone()).await;
        server.abort();
        assert_eq!(result.unwrap().id, config.id);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// Run against a disposable NOAA Oracle 2.0 instance whose coordinator_pubkeys
    /// includes the key at COORDINATOR_TEST_ORACLE_KEY. The URL must match remote_url.
    #[tokio::test]
    #[ignore = "requires COORDINATOR_TEST_ORACLE_URL and COORDINATOR_TEST_ORACLE_KEY"]
    async fn live_noaa_v2_create_read_and_submit_entries() {
        let url = Url::parse(&std::env::var("COORDINATOR_TEST_ORACLE_URL").unwrap()).unwrap();
        let key = std::env::var("COORDINATOR_TEST_ORACLE_KEY").unwrap();
        let client = OracleClient::new(
            ClientBuilder::new(reqwest::Client::new()).build(),
            &url,
            &key,
        )
        .unwrap();
        let config = event_config();
        let created = client.create_event(config.clone()).await.unwrap();
        assert_eq!(created.id, config.id);
        assert_eq!(created.event_announcement.locking_points.len(), 3);
        let fetched = client.get_event(&config.id).await.unwrap();
        assert_eq!(fetched, created);
        let entries = (0..2)
            .map(|_| AddEventEntry {
                id: Uuid::now_v7(),
                event_id: config.id,
                expected_observations: vec![WeatherChoices {
                    stations: "KORD".into(),
                    wind_speed: None,
                    temp_high: Some(ValueOptions::Over),
                    temp_low: None,
                }],
            })
            .collect();
        client
            .submit_entries(AddEventEntries {
                event_id: config.id,
                entries,
            })
            .await
            .unwrap();
        assert_eq!(client.get_event(&config.id).await.unwrap(), created);
    }
}
