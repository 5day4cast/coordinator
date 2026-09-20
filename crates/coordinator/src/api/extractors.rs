use axum::{
    body::to_bytes,
    extract::{FromRequest, FromRequestParts, OriginalUri, Request},
    http::request::Parts,
    response::IntoResponse,
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hyper::{header::AUTHORIZATION, StatusCode};
use log::{debug, warn};
use nostr_sdk::{
    hashes::{sha256::Hash as Sha256Hash, Hash},
    nips::nip98::{HttpData, HttpMethod},
    Event, EventBuilder, Keys, Kind, PublicKey, Url,
};
use serde::{de::DeserializeOwned, ser::SerializeStruct, Serialize, Serializer};
use serde_json::json;
use std::{str::FromStr, sync::Arc};
use time::OffsetDateTime;

use super::nip98_origins::Nip98Origins;
use super::nip98_replay::{Nip98ReplayGuard, ReplayRejection, MAX_EVENT_SKEW_SECS};

pub async fn create_auth_event(
    method: &str,
    url: &str,
    payload_hash: Option<Sha256Hash>,
    keys: &Keys,
) -> Event {
    let http_method = HttpMethod::from_str(method).unwrap();
    let http_url = Url::from_str(url).unwrap();
    let mut http_data = HttpData::new(http_url, http_method);

    if let Some(hash) = payload_hash {
        http_data = http_data.payload(hash);
    }

    EventBuilder::http_auth(http_data)
        .sign_with_keys(keys)
        .expect("Failed to sign event")
}

#[derive(Clone, Debug)]
pub struct NostrAuth {
    pub pubkey: PublicKey,
    pub event: Event,
    pub http_data: HttpData,
}

impl<S> FromRequestParts<S> for NostrAuth
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .ok_or(AuthError::NoAuthHeader)?;

        let original_uri = parts
            .extensions
            .get::<OriginalUri>()
            .map(|OriginalUri(uri)| uri.clone())
            .unwrap_or_else(|| parts.uri.clone());

        let event_json = auth_header
            .strip_prefix("Nostr ")
            .ok_or(AuthError::InvalidAuthFormat)?;

        let event_bytes = BASE64
            .decode(event_json)
            .map_err(|e| AuthError::InvalidBase64(e.to_string()))?;

        let event: Event = serde_json::from_slice(&event_bytes)
            .map_err(|e| AuthError::InvalidEventJson(e.to_string()))?;

        if event.kind != Kind::HttpAuth {
            return Err(AuthError::InvalidEventKind);
        }

        let now = OffsetDateTime::now_utc().unix_timestamp();
        let created_at = i64::try_from(event.created_at.as_u64()).unwrap_or(i64::MAX);
        if (now - created_at).abs() > MAX_EVENT_SKEW_SECS {
            return Err(AuthError::ExpiredTimestamp);
        }

        let tags = event.tags.clone().to_vec();
        let http_data =
            HttpData::try_from(tags).map_err(|e| AuthError::InvalidHttpData(e.to_string()))?;
        // The origin of the signed URL comes from configuration, never from
        // the request's own headers. A router without the origins fails closed.
        let origins = parts
            .extensions
            .get::<Arc<Nip98Origins>>()
            .ok_or(AuthError::OriginsMissing)?;
        let method = HttpMethod::from_str(parts.method.as_str())
            .map_err(|e| AuthError::InvalidMethod(e.to_string()))?;
        if !origins.accepts(&http_data.url, &original_uri) || http_data.method != method {
            debug!(
                "NIP-98 event names {} {}, request is {} {}",
                http_data.method, http_data.url, parts.method, original_uri
            );
            return Err(AuthError::UrlMethodMismatch);
        }

        if !event.content.is_empty() {
            return Err(AuthError::NonEmptyContent);
        }

        event
            .verify()
            .map_err(|e| AuthError::InvalidSignature(e.to_string()))?;

        // Claim only after the signature verifies, so unsigned junk cannot
        // fill the guard. A router without the guard fails closed.
        parts
            .extensions
            .get::<Arc<Nip98ReplayGuard>>()
            .ok_or(AuthError::ReplayGuardMissing)?
            .claim(event.id, created_at, now)
            .map_err(|rejection| match rejection {
                ReplayRejection::Replayed => AuthError::Replayed,
                ReplayRejection::Full => AuthError::ReplayGuardFull,
            })?;

        Ok(Self {
            pubkey: event.pubkey,
            event,
            http_data,
        })
    }
}

/// Upper bound on an authenticated JSON request body.
const MAX_AUTHED_BODY_BYTES: usize = 256 * 1024;

/// A NIP-98 authenticated JSON body.
///
/// The auth event must carry a `payload` tag equal to the SHA-256 of the exact
/// body bytes. Without that check, a captured `Authorization` header could be
/// replayed within its 60 second window with a different body.
pub struct AuthedJson<T> {
    pub auth: NostrAuth,
    pub body: T,
}

impl<S, T> FromRequest<S> for AuthedJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AuthError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (mut parts, body) = request.into_parts();
        let auth = NostrAuth::from_request_parts(&mut parts, state).await?;
        let bytes = to_bytes(body, MAX_AUTHED_BODY_BYTES)
            .await
            .map_err(|_| AuthError::BodyTooLarge)?;
        if auth.http_data.payload != Some(Sha256Hash::hash(&bytes)) {
            return Err(AuthError::PayloadMismatch);
        }
        let body =
            serde_json::from_slice(&bytes).map_err(|e| AuthError::InvalidBody(e.to_string()))?;
        Ok(Self { auth, body })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    #[error("No authorization header found")]
    NoAuthHeader,
    #[error("Invalid login")]
    InvalidLogin,
    #[error("Invalid authorization format")]
    InvalidAuthFormat,
    #[error("Invalid base64 encoding: {0}")]
    InvalidBase64(String),
    #[error("Invalid event JSON: {0}")]
    InvalidEventJson(String),
    #[error("Invalid event kind")]
    InvalidEventKind,
    #[error("Event timestamp expired")]
    ExpiredTimestamp,
    #[error("Invalid HTTP data: {0}")]
    InvalidHttpData(String),
    #[error("URL or method mismatch")]
    UrlMethodMismatch,
    #[error("Invalid URL format: {0}")]
    InvalidUrl(String),
    #[error("Invalid method format: {0}")]
    InvalidMethod(String),
    #[error("Invalid signature: {0}")]
    InvalidSignature(String),
    #[error("Event content must be empty")]
    NonEmptyContent,
    #[error("Payload hash does not match the request body")]
    PayloadMismatch,
    #[error("Request body too large")]
    BodyTooLarge,
    #[error("Invalid request body: {0}")]
    InvalidBody(String),
    #[error("Auth event was already used")]
    Replayed,
    #[error("Too many recent authenticated requests")]
    ReplayGuardFull,
    #[error("NIP-98 replay guard is not configured")]
    ReplayGuardMissing,
    #[error("NIP-98 origins are not configured")]
    OriginsMissing,
}

impl From<nostr_sdk::types::ParseError> for AuthError {
    fn from(err: nostr_sdk::types::ParseError) -> Self {
        AuthError::InvalidUrl(err.to_string())
    }
}

impl Serialize for AuthError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("AuthError", 2)?;

        let type_str = match self {
            Self::NoAuthHeader => "no_auth_header",
            Self::InvalidLogin => "invalid_login",
            Self::InvalidAuthFormat => "invalid_auth_format",
            Self::InvalidBase64(_) => "invalid_base_64",
            Self::InvalidEventJson(_) => "invalid_event_json",
            Self::InvalidEventKind => "invalid_event_kind",
            Self::InvalidUrl(_) => "invalid_url",
            Self::InvalidMethod(_) => "invalid_method",
            Self::ExpiredTimestamp => "expired_timestamp",
            Self::InvalidHttpData(_) => "invalid_http_data",
            Self::UrlMethodMismatch => "url_method_mismatch",
            Self::InvalidSignature(_) => "invalid_signature",
            Self::NonEmptyContent => "non_empty_content",
            Self::PayloadMismatch => "payload_mismatch",
            Self::BodyTooLarge => "body_too_large",
            Self::InvalidBody(_) => "invalid_body",
            Self::Replayed => "replayed",
            Self::ReplayGuardFull => "replay_guard_full",
            Self::ReplayGuardMissing => "replay_guard_missing",
            Self::OriginsMissing => "origins_missing",
        };

        state.serialize_field("type", type_str)?;
        state.serialize_field("detail", &self.to_string())?;
        state.end()
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> axum::response::Response {
        let (body, code) = match &self {
            Self::InvalidSignature(_) => {
                warn!("{}", self);
                (json!({ "error": self }), StatusCode::FORBIDDEN)
            }
            Self::NoAuthHeader
            | Self::InvalidEventKind
            | Self::ExpiredTimestamp
            | Self::UrlMethodMismatch
            | Self::InvalidUrl(_)
            | Self::InvalidLogin
            | Self::PayloadMismatch
            | Self::Replayed
            | Self::InvalidMethod(_) => {
                warn!("{}", self);
                (json!({ "error": self }), StatusCode::UNAUTHORIZED)
            }
            Self::ReplayGuardFull => {
                warn!("{}", self);
                (json!({ "error": self }), StatusCode::SERVICE_UNAVAILABLE)
            }
            Self::ReplayGuardMissing => {
                log::error!("{}", self);
                (json!({ "error": self }), StatusCode::INTERNAL_SERVER_ERROR)
            }
            _ => {
                warn!("{}", self);
                (json!({ "error": self }), StatusCode::BAD_REQUEST)
            }
        };

        (code, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use nostr_sdk::{
        hashes::{sha256::Hash as Sha256Hash, Hash},
        Alphabet, EventBuilder, Keys, SingleLetterTag, Tag, TagKind, Timestamp,
    };
    use std::{str::FromStr, sync::Arc};
    #[derive(Clone)]
    pub struct AppState;

    #[tokio::test]
    async fn test_valid_get_request() {
        let keys = Keys::generate();
        let state = AppState;

        let event = create_auth_event("GET", "http://localhost/test", None, &keys).await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(result.is_ok());
        let auth = result.unwrap();
        assert_eq!(auth.pubkey, keys.public_key());
        assert_eq!(auth.http_data.method, HttpMethod::GET);
    }

    async fn authed_json(
        signed_body: Option<&str>,
        sent_body: &str,
    ) -> Result<AuthedJson<serde_json::Value>, AuthError> {
        let keys = Keys::generate();
        let payload = signed_body.map(|body| Sha256Hash::hash(body.as_bytes()));
        let event = create_auth_event("POST", "http://localhost/test", payload, &keys).await;
        let request = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("POST")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(
                AUTHORIZATION,
                format!(
                    "Nostr {}",
                    BASE64.encode(serde_json::to_string(&event).unwrap())
                ),
            )
            .body(axum::body::Body::from(sent_body.to_owned()))
            .unwrap();
        AuthedJson::from_request(request, &AppState).await
    }

    #[tokio::test]
    async fn the_same_auth_header_is_accepted_only_once() {
        let keys = Keys::generate();
        let event = create_auth_event("GET", "http://localhost/test", None, &keys).await;
        let header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );
        let guard = Arc::new(Nip98ReplayGuard::new(16));
        let attempt = || {
            Request::builder()
                .method("GET")
                .uri("/test")
                .header("host", "localhost")
                .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
                .header(AUTHORIZATION, header.clone())
                .extension(guard.clone())
                .body(())
                .unwrap()
                .into_parts()
                .0
        };

        assert!(NostrAuth::from_request_parts(&mut attempt(), &AppState)
            .await
            .is_ok());
        assert!(matches!(
            NostrAuth::from_request_parts(&mut attempt(), &AppState).await,
            Err(AuthError::Replayed)
        ));
    }

    #[tokio::test]
    async fn missing_replay_guard_fails_closed() {
        let keys = Keys::generate();
        let event = create_auth_event("GET", "http://localhost/test", None, &keys).await;
        let mut parts = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(
                AUTHORIZATION,
                format!(
                    "Nostr {}",
                    BASE64.encode(serde_json::to_string(&event).unwrap())
                ),
            )
            .body(())
            .unwrap()
            .into_parts()
            .0;
        assert!(matches!(
            NostrAuth::from_request_parts(&mut parts, &AppState).await,
            Err(AuthError::ReplayGuardMissing)
        ));
    }

    #[tokio::test]
    async fn authed_json_accepts_body_matching_signed_payload() {
        let body = r#"{"amount":1}"#;
        let authed = authed_json(Some(body), body).await.unwrap();
        assert_eq!(authed.body["amount"], 1);
    }

    #[tokio::test]
    async fn authed_json_rejects_replayed_header_with_different_body() {
        let result = authed_json(Some(r#"{"amount":1}"#), r#"{"amount":1000}"#).await;
        assert!(matches!(result, Err(AuthError::PayloadMismatch)));
    }

    #[tokio::test]
    async fn authed_json_requires_a_payload_tag() {
        let result = authed_json(None, r#"{"amount":1}"#).await;
        assert!(matches!(result, Err(AuthError::PayloadMismatch)));
    }

    #[tokio::test]
    async fn authed_json_bounds_the_body() {
        let body = "x".repeat(MAX_AUTHED_BODY_BYTES + 1);
        let result = authed_json(Some(&body), &body).await;
        assert!(matches!(result, Err(AuthError::BodyTooLarge)));
    }

    #[tokio::test]
    async fn test_valid_post_with_payload() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let body = r#"{"test": "data"}"#;
        let payload_hash = Sha256Hash::hash(body.as_bytes());

        let event =
            create_auth_event("POST", "http://localhost/test", Some(payload_hash), &keys).await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("POST")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(result.is_ok());
        let auth = result.unwrap();
        assert_eq!(auth.pubkey, keys.public_key());
        assert_eq!(auth.http_data.method, HttpMethod::POST);
        assert_eq!(auth.http_data.payload, Some(payload_hash));
    }

    #[tokio::test]
    async fn test_missing_auth_header() {
        let state = Arc::new(AppState);

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::NoAuthHeader)));
    }

    #[tokio::test]
    async fn test_invalid_auth_format() {
        let state = Arc::new(AppState);

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, "InvalidFormat")
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::InvalidAuthFormat)));
    }

    #[tokio::test]
    async fn test_invalid_base64() {
        let state = Arc::new(AppState);

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, "Nostr invalid-base64!")
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::InvalidBase64(_))));
    }

    #[tokio::test]
    async fn test_invalid_event_json() {
        let state = Arc::new(AppState);

        let invalid_json = BASE64.encode("not valid json");
        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, format!("Nostr {invalid_json}"))
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::InvalidEventJson(_))));
    }

    /// The signed URL's origin must be a configured one; the `Host` header
    /// a client sends cannot make another host acceptable.
    #[tokio::test]
    async fn host_header_cannot_choose_the_origin() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);
        let http_method = HttpMethod::from_str("GET").unwrap();
        let http_url = Url::from_str("http://evil.example/test").unwrap();
        let tags = vec![
            Tag::custom(TagKind::Method, [http_method.to_string()]),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U)),
                [http_url.to_string()],
            ),
        ];
        let created_at = OffsetDateTime::now_utc().unix_timestamp() as u64;
        let event = EventBuilder::new(Kind::HttpAuth, "")
            .custom_created_at(Timestamp::from(created_at))
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("Failed to sign event");
        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );
        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .method("GET")
            .uri("/test")
            .header("host", "evil.example")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();
        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;
        assert!(matches!(result, Err(AuthError::UrlMethodMismatch)));
    }

    #[tokio::test]
    async fn test_non_auth_event_kind() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);
        let http_method = HttpMethod::from_str("GET").unwrap();
        let http_url = Url::from_str("http://localhost/test").unwrap();

        let tags = vec![
            Tag::custom(TagKind::Method, [http_method.to_string()]),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U)),
                [http_url.to_string()],
            ),
        ];

        let created_at = OffsetDateTime::now_utc().unix_timestamp() as u64;

        // Create a regular text note instead of auth event
        let event = EventBuilder::new(Kind::TextNote, "")
            .custom_created_at(Timestamp::from(created_at))
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("Failed to sign event");

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::InvalidEventKind)));
    }

    #[tokio::test]
    async fn test_expired_timestamp() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let expired_time =
            (OffsetDateTime::now_utc() - time::Duration::hours(1)).unix_timestamp() as u64;
        let http_data = HttpData::new(
            Url::from_str("http://localhost/test").unwrap(),
            HttpMethod::GET,
        );

        let event = EventBuilder::http_auth(http_data)
            .custom_created_at(Timestamp::from(expired_time))
            .sign_with_keys(&keys)
            .unwrap();

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::ExpiredTimestamp)));
    }

    #[tokio::test]
    async fn test_url_mismatch() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let event = create_auth_event("GET", "http://localhost/different-path", None, &keys).await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::UrlMethodMismatch)));
    }

    #[tokio::test]
    async fn test_method_mismatch() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let event = create_auth_event("POST", "http://localhost/test", None, &keys).await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET") // Different method from event
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::UrlMethodMismatch)));
    }

    #[tokio::test]
    async fn test_non_empty_content() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let http_method = HttpMethod::from_str("GET").unwrap();
        let http_url = Url::from_str("http://localhost/test").unwrap();

        let tags = vec![
            Tag::custom(TagKind::Method, [http_method.to_string()]),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U)),
                [http_url.to_string()],
            ),
        ];

        let created_at = OffsetDateTime::now_utc().unix_timestamp() as u64;

        // Create event with non-empty content (invalid per NIP-98)
        let event = EventBuilder::new(Kind::HttpAuth, "non-empty content")
            .custom_created_at(Timestamp::from(created_at))
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("Failed to sign event");

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["http://localhost"]).unwrap()))
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::NonEmptyContent)));
    }

    #[tokio::test]
    async fn test_forwarded_proto() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let event = create_auth_event(
            "GET",
            "https://localhost/test", // Note https
            None,
            &keys,
        )
        .await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .extension(Arc::new(Nip98ReplayGuard::new(16)))
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .extension(Arc::new(Nip98Origins::new(["https://localhost"]).unwrap()))
            .header("x-forwarded-proto", "https")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(result.is_ok());
    }
}
