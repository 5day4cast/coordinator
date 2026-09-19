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
use nostr::{
    hashes::Hash as Sha256Hash, secp256k1::SecretKey as Secp256k1SecretKey, Keys,
    SecretKey as NostrSecretKey, ToBech32,
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

/// Kept in request extensions, so the retry middleware repeats reconciliation
/// before a write. It also handles a retry after a coordinator restart.
#[derive(Clone)]
enum OracleWrite {
    Create(CreateEvent),
    Entries(AddEventEntries),
}

#[derive(Deserialize)]
struct StoredEvent {
    #[serde(flatten)]
    event: Event,
    #[serde(with = "time::serde::rfc3339")]
    signing_date: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    start_observation_date: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    end_observation_date: time::OffsetDateTime,
    locations: Vec<String>,
    number_of_values_per_entry: usize,
    total_allowed_entries: usize,
    number_of_places_win: usize,
    source: String,
    scoring_fields: Vec<String>,
    coordinator_pubkey: String,
    entries: Vec<StoredEntry>,
}

#[derive(Deserialize)]
struct StoredEntry {
    id: Uuid,
    event_id: Uuid,
    picks: Vec<StoredPick>,
}

#[derive(Deserialize, PartialEq, Eq)]
struct StoredPick {
    target: String,
    metric: String,
    prediction: ValueOptions,
}

impl OracleWrite {
    fn event_id(&self) -> Uuid {
        match self {
            Self::Create(event) => event.id,
            Self::Entries(entries) => entries.event_id,
        }
    }

    fn matches(&self, stored: &StoredEvent) -> bool {
        match self {
            Self::Create(event) => {
                // Fees and Bitcoin locktimes are coordinator-only fields; NOAA
                // stores the event dates, locations, limits, source and metrics.
                stored.signing_date == event.signing_date
                    && stored.start_observation_date == event.start_observation_date
                    && stored.end_observation_date == event.end_observation_date
                    && stored.locations == event.locations
                    && stored.number_of_values_per_entry == event.number_of_values_per_entry
                    && stored.total_allowed_entries == event.total_allowed_entries
                    && stored.number_of_places_win == event.number_of_places_win
                    && stored.source == "noaa_weather"
                    && stored.scoring_fields == ["temp_high", "temp_low", "wind_speed"]
            }
            Self::Entries(submission) => {
                let ids: std::collections::HashSet<_> =
                    submission.entries.iter().map(|entry| entry.id).collect();
                if ids.len() != submission.entries.len()
                    || stored.entries.len() != submission.entries.len()
                    || stored.entries.len() != stored.total_allowed_entries
                {
                    return false;
                }
                submission.entries.iter().all(|entry| {
                    let Some(existing) = stored
                        .entries
                        .iter()
                        .find(|existing| existing.id == entry.id)
                    else {
                        return false;
                    };
                    if entry.event_id != submission.event_id
                        || existing.event_id != submission.event_id
                    {
                        return false;
                    }
                    let wanted: Vec<_> = entry
                        .expected_observations
                        .iter()
                        .flat_map(|choices| {
                            [
                                ("temp_high", &choices.temp_high),
                                ("temp_low", &choices.temp_low),
                                ("wind_speed", &choices.wind_speed),
                            ]
                            .into_iter()
                            .filter_map(move |(metric, prediction)| {
                                prediction.as_ref().map(|prediction| StoredPick {
                                    target: choices.stations.clone(),
                                    metric: metric.to_owned(),
                                    prediction: prediction.clone(),
                                })
                            })
                        })
                        .collect();
                    // Picks are authoritative in NOAA v2. Checking the complete
                    // set also rejects extra metrics missing from the legacy DTO.
                    let distinct: std::collections::HashSet<_> = wanted
                        .iter()
                        .map(|pick| (&pick.target, &pick.metric))
                        .collect();
                    distinct.len() == wanted.len()
                        && existing.picks.len() == wanted.len()
                        && wanted.iter().all(|pick| existing.picks.contains(pick))
                })
            }
        }
    }
}

impl OracleAuth {
    async fn sign(&self, request: &mut reqwest::Request) -> reqwest_middleware::Result<()> {
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
        Ok(())
    }

    async fn recover(
        &self,
        request: &reqwest::Request,
        write: &OracleWrite,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Option<Response>> {
        let url = request
            .url()
            .join(&format!("/oracle/events/{}", write.event_id()))
            .map_err(|error| reqwest_middleware::Error::Middleware(error.into()))?;
        let mut lookup = reqwest::Request::new(Method::GET, url);
        *lookup.timeout_mut() = Some(std::time::Duration::from_secs(30));
        self.sign(&mut lookup).await?;
        let mut lookup_extensions = Extensions::new();
        let response = next.run(lookup, &mut lookup_extensions).await?;
        if response.status() == StatusCode::NOT_FOUND && matches!(write, OracleWrite::Create(_)) {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Ok(Some(response));
        }
        let bytes = read_oracle_body(response).await?;
        let stored: StoredEvent = serde_json::from_slice(&bytes)
            .map_err(|error| reqwest_middleware::Error::Middleware(error.into()))?;
        let owner = self
            .0
            .public_key()
            .to_bech32()
            .map_err(|error| reqwest_middleware::Error::Middleware(error.into()))?;
        if stored.event.id != write.event_id() || stored.coordinator_pubkey != owner {
            return Err(reqwest_middleware::Error::Middleware(anyhow!(
                "oracle event belongs to a different coordinator or event"
            )));
        }
        if stored.source != "noaa_weather"
            || stored.scoring_fields != ["temp_high", "temp_low", "wind_speed"]
        {
            return Err(reqwest_middleware::Error::Middleware(anyhow!(
                "oracle source or scoring fields do not match NOAA defaults"
            )));
        }
        if matches!(write, OracleWrite::Entries(_)) && stored.entries.is_empty() {
            return Ok(None);
        }
        if !write.matches(&stored) {
            return Err(reqwest_middleware::Error::Middleware(anyhow!(
                "existing oracle event or entries do not match the requested write"
            )));
        }
        Ok(Some(hyper::http::Response::new(bytes).into()))
    }
}

#[async_trait::async_trait]
impl Middleware for OracleAuth {
    async fn handle(
        &self,
        mut request: reqwest::Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let write = extensions.get::<OracleWrite>().cloned();
        if let Some(write) = &write {
            let limit = std::time::Duration::from_secs(30);
            let timeout = request.timeout().copied().unwrap_or(limit).min(limit);
            *request.timeout_mut() = Some(timeout);
            if let Some(response) = self.recover(&request, write, next.clone()).await? {
                return Ok(response);
            }
        }
        self.sign(&mut request).await?;
        let retry_request = request.try_clone();
        let response = next.clone().run(request, extensions).await;
        // Read successful write responses inside the retry boundary. Otherwise a
        // truncated JSON body would fail only after the middleware had returned.
        let response = match response {
            Ok(response) if write.is_some() && response.status().is_success() => {
                read_oracle_body(response)
                    .await
                    .map(|bytes| hyper::http::Response::new(bytes).into())
            }
            other => other,
        };
        if response
            .as_ref()
            .map_or(true, |response| !response.status().is_success())
        {
            if let (Some(write), Some(request)) = (&write, retry_request) {
                if let Some(recovered) = self.recover(&request, write, next).await? {
                    return Ok(recovered);
                }
            }
        }
        response
    }
}

/// The largest NOAA announcement is bounded to 20k outcomes. Allow room for
/// its points and entries, but never buffer an unbounded or stalled response.
async fn read_oracle_body(mut response: Response) -> reqwest_middleware::Result<Vec<u8>> {
    const MAX_BYTES: usize = 16 * 1024 * 1024;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > MAX_BYTES {
                return Err(reqwest_middleware::Error::Middleware(anyhow!(
                    "oracle response exceeds 16 MiB"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    })
    .await
    .map_err(|_| reqwest_middleware::Error::Middleware(anyhow!("oracle response body timed out")))?
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
        recovery: Option<OracleWrite>,
    ) -> Result<Response, Error> {
        let mut request = self
            .client
            .request(method, url)
            .header(CONTENT_TYPE, APPLICATION_JSON.to_string());

        if let Some(b) = body {
            request = request.body(b);
        }
        if let Some(recovery) = recovery {
            request = request.with_extension(recovery);
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
            .send_authenticated_request_internal(method, url, body, not_found_message, None)
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
        self.send_authenticated_request_internal(method, url, body, not_found_message, None)
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

        self.send_authenticated_request_internal(
            Method::POST,
            url,
            Some(body),
            String::from("event not found"),
            Some(OracleWrite::Create(event)),
        )
        .await?
        .json()
        .await
        .map_err(Into::into)
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

        self.send_authenticated_request_internal(
            Method::POST,
            url,
            Some(body),
            String::from("event not found"),
            Some(OracleWrite::Entries(event_entries)),
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, Bytes},
        extract::State,
        http::HeaderMap,
        routing::{get, post},
        Json, Router,
    };
    use nostr::nips::nip98::HttpData;
    use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
    use std::{
        collections::HashSet,
        sync::{
            atomic::{AtomicU16, AtomicUsize, Ordering},
            Arc, Mutex,
        },
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
        signer: nostr::PublicKey,
        seen: Arc<Mutex<HashSet<nostr::EventId>>>,
        response: serde_json::Value,
    }

    async fn retry_server(
        State(state): State<RetryServer>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let header = headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
        let auth: nostr::Event = serde_json::from_slice(
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

    #[derive(Clone)]
    struct RecoveryServer {
        saved: Arc<Mutex<Option<serde_json::Value>>>,
        created: serde_json::Value,
        get_status: Arc<AtomicU16>,
        create_posts: Arc<AtomicUsize>,
        entry_posts: Arc<AtomicUsize>,
    }

    async fn recovery_get(
        State(state): State<RecoveryServer>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let status = StatusCode::from_u16(state.get_status.load(Ordering::SeqCst)).unwrap();
        if status != StatusCode::OK {
            return (status, Json(serde_json::json!({"error": "lookup failed"})));
        }
        let saved = state.saved.lock().unwrap().clone();
        match saved {
            Some(saved) => (StatusCode::OK, Json(saved)),
            None => (StatusCode::NOT_FOUND, Json(serde_json::Value::Null)),
        }
    }

    fn lost_response() -> axum::response::Response {
        // The write has committed, but the connection drops in its response body.
        let stream = futures::stream::iter([
            Ok(Bytes::from_static(b"{\"id\":")),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "response lost",
            )),
        ]);
        hyper::http::Response::builder()
            .status(StatusCode::CREATED)
            .body(Body::from_stream(stream))
            .unwrap()
    }

    async fn recovery_create(State(state): State<RecoveryServer>) -> axum::response::Response {
        state.create_posts.fetch_add(1, Ordering::SeqCst);
        *state.saved.lock().unwrap() = Some(state.created);
        lost_response()
    }

    async fn recovery_entries(
        State(state): State<RecoveryServer>,
        Json(body): Json<AddEventEntries>,
    ) -> axum::response::Response {
        state.entry_posts.fetch_add(1, Ordering::SeqCst);
        state.saved.lock().unwrap().as_mut().unwrap()["entries"] = serde_json::json!(body
            .entries
            .iter()
            .map(|entry| serde_json::json!({
                "id": entry.id, "event_id": body.event_id,
                "picks": [{"target": "KORD", "metric": "temp_high", "prediction": "Over"}]
            }))
            .collect::<Vec<_>>());
        lost_response()
    }

    async fn recovery_fixture(
        config: &CreateEvent,
    ) -> (
        OracleClient,
        RecoveryServer,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let key = directory.path().join("coordinator.pem");
        let secret: Secp256k1SecretKey = get_key(key.to_str().unwrap()).unwrap();
        let mut created = serde_json::to_value(config).unwrap();
        created
            .as_object_mut()
            .unwrap()
            .extend(event_response(config.id).as_object().unwrap().clone());
        created["coordinator_pubkey"] = serde_json::json!(secp256k1_to_nostr_keys(&secret)
            .unwrap()
            .public_key()
            .to_bech32()
            .unwrap());
        created["scoring_fields"] = serde_json::json!(["temp_high", "temp_low", "wind_speed"]);
        created["entries"] = serde_json::json!([]);
        let state = RecoveryServer {
            saved: Arc::default(),
            created,
            get_status: Arc::new(AtomicU16::new(200)),
            create_posts: Arc::default(),
            entry_posts: Arc::default(),
        };
        let app = Router::new()
            .route("/oracle/events", post(recovery_create))
            .route("/oracle/events/{id}", get(recovery_get))
            .route("/oracle/events/{id}/entries", post(recovery_entries))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = OracleClient::new(
            ClientBuilder::new(reqwest::Client::new()).build(),
            &base,
            key.to_str().unwrap(),
        )
        .unwrap();
        (client, state, server, directory)
    }

    #[tokio::test]
    async fn committed_writes_with_lost_responses_are_recovered_without_duplicate_posts() {
        let config = event_config();
        let (client, state, server, directory) = recovery_fixture(&config).await;
        assert_eq!(
            client.create_event(config.clone()).await.unwrap().id,
            config.id
        );
        // A new client has no process-local history, as after a restart.
        let restarted = OracleClient::new(
            ClientBuilder::new(reqwest::Client::new()).build(),
            &client.base_url,
            directory.path().join("coordinator.pem").to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            restarted.create_event(config.clone()).await.unwrap().id,
            config.id
        );
        assert_eq!(state.create_posts.load(Ordering::SeqCst), 1);

        let submission = AddEventEntries {
            event_id: config.id,
            entries: (0..2)
                .map(|_| AddEventEntry {
                    id: Uuid::now_v7(),
                    event_id: config.id,
                    expected_observations: vec![WeatherChoices {
                        stations: "KORD".into(),
                        temp_high: Some(ValueOptions::Over),
                        temp_low: None,
                        wind_speed: None,
                    }],
                })
                .collect(),
        };
        client.submit_entries(submission.clone()).await.unwrap();
        restarted.submit_entries(submission.clone()).await.unwrap();
        assert_eq!(state.entry_posts.load(Ordering::SeqCst), 1);

        let saved = state.saved.lock().unwrap().clone().unwrap();
        for (field, value) in [
            ("id", serde_json::json!(Uuid::now_v7())),
            ("event_id", serde_json::json!(Uuid::now_v7())),
        ] {
            let mut changed = saved.clone();
            changed["entries"][0][field] = value;
            *state.saved.lock().unwrap() = Some(changed);
            assert!(client.submit_entries(submission.clone()).await.is_err());
        }
        let mut changed = saved.clone();
        changed["entries"][0]["picks"][0]["prediction"] = serde_json::json!("Under");
        *state.saved.lock().unwrap() = Some(changed);
        assert!(client.submit_entries(submission.clone()).await.is_err());
        let mut foreign = saved.clone();
        foreign["coordinator_pubkey"] =
            serde_json::json!(Keys::generate().public_key().to_bech32().unwrap());
        *state.saved.lock().unwrap() = Some(foreign);
        assert!(client.submit_entries(submission.clone()).await.is_err());
        *state.saved.lock().unwrap() = Some(saved);

        // Complete picks must match, including metrics absent from our legacy DTO.
        state.saved.lock().unwrap().as_mut().unwrap()["entries"][0]["picks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"target": "KORD", "metric": "humidity", "prediction": "Par"}));
        assert!(client.submit_entries(submission).await.is_err());
        assert_eq!(state.entry_posts.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn recovery_rejects_foreign_or_changed_events_and_unavailable_lookups() {
        let config = event_config();
        let (client, state, server, _directory) = recovery_fixture(&config).await;
        for (field, value) in [
            ("id", serde_json::json!(Uuid::now_v7())),
            (
                "coordinator_pubkey",
                serde_json::json!(Keys::generate().public_key().to_bech32().unwrap()),
            ),
            ("source", serde_json::json!("other_source")),
            ("scoring_fields", serde_json::json!(["temp_high"])),
            ("locations", serde_json::json!(["KJFK"])),
            ("number_of_values_per_entry", serde_json::json!(2)),
            ("number_of_places_win", serde_json::json!(2)),
            ("total_allowed_entries", serde_json::json!(3)),
            ("signing_date", serde_json::json!("2030-01-01T00:00:00Z")),
            (
                "start_observation_date",
                serde_json::json!("2030-01-01T00:00:00Z"),
            ),
            (
                "end_observation_date",
                serde_json::json!("2030-01-01T00:00:00Z"),
            ),
        ] {
            let mut changed = state.created.clone();
            changed[field] = value;
            *state.saved.lock().unwrap() = Some(changed);
            assert!(
                client.create_event(config.clone()).await.is_err(),
                "{field}"
            );
        }
        *state.saved.lock().unwrap() = Some(serde_json::json!({"id": config.id}));
        assert!(client.create_event(config.clone()).await.is_err());
        for status in [401, 503] {
            state.get_status.store(status, Ordering::SeqCst);
            assert!(client.create_event(config.clone()).await.is_err());
        }
        assert_eq!(state.create_posts.load(Ordering::SeqCst), 0);
        assert_eq!(state.entry_posts.load(Ordering::SeqCst), 0);
        server.abort();
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
