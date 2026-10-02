//! Operator writes require a same-origin browser nonce or an explicit API credential.
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::config::ServerConfig;

const COOKIE: &str = "synth_csrf";
const CSRF_HEADER: &str = "x-synth-csrf";

#[derive(Clone)]
pub(super) struct OperatorAccess {
    nonce: Arc<str>,
    origins: Arc<[String]>,
    token_digest: Option<[u8; 32]>,
}

impl OperatorAccess {
    pub fn new(config: &ServerConfig) -> Result<Self> {
        for origin in &config.allowed_origins {
            let url = reqwest::Url::parse(origin).context("parse server.allowed_origins")?;
            anyhow::ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.origin().ascii_serialization() == *origin,
                "server.allowed_origins must contain exact HTTP origins without paths"
            );
        }
        let token_digest = config
            .operator_token_file
            .as_ref()
            .map(|path| {
                let secret = zeroize::Zeroizing::new(
                    std::fs::read_to_string(path).context("read server.operator_token_file")?,
                );
                anyhow::ensure!(
                    secret.trim().len() >= 32,
                    "operator token must have at least 32 characters"
                );
                Ok::<_, anyhow::Error>(Sha256::digest(secret.trim().as_bytes()).into())
            })
            .transpose()?;
        let mut bytes = [0; 32];
        rand::rng().fill_bytes(&mut bytes);
        Ok(Self {
            nonce: hex::encode(bytes).into(),
            origins: config.allowed_origins.clone().into(),
            token_digest,
        })
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        // A browser never bypasses origin validation by attaching an API credential.
        if headers.contains_key(header::ORIGIN) || headers.contains_key("sec-fetch-site") {
            return self.browser_origin(headers)
                && single_header(headers, CSRF_HEADER) == Some(self.nonce.as_ref());
        }
        let Some(expected) = self.token_digest else {
            return false;
        };
        let Some(token) = single_header(headers, header::AUTHORIZATION.as_str())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        // Compare fixed-size digests without an early return at the first unequal byte.
        actual
            .iter()
            .zip(expected)
            .fold(0u8, |different, (left, right)| different | (left ^ right))
            == 0
    }

    fn browser_origin(&self, headers: &HeaderMap) -> bool {
        if single_header(headers, "sec-fetch-site").is_some_and(|value| value != "same-origin") {
            return false;
        }
        let Some(origin) = single_header(headers, header::ORIGIN.as_str()) else {
            return false;
        };
        let Ok(url) = reqwest::Url::parse(origin) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https") || url.origin().ascii_serialization() != origin
        {
            return false;
        }
        if !self.origins.is_empty() {
            return self.origins.iter().any(|allowed| allowed == origin);
        }
        // Compatibility for a dashboard behind an existing Host-preserving proxy. Forwarded
        // headers are deliberately ignored; deployments can pin origins when Host is rewritten.
        let Some(host) = single_header(headers, header::HOST.as_str()) else {
            return false;
        };
        let Ok(expected) = reqwest::Url::parse(&format!("{}://{host}", url.scheme())) else {
            return false;
        };
        expected.origin() == url.origin() && expected.path() == "/"
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

pub(super) async fn authorize(
    State(access): State<OperatorAccess>,
    request: Request,
    next: Next,
) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if !safe && !access.authorized(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "Operator writes require a same-origin dashboard nonce or bearer token",
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    if safe
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.starts_with("text/html") || value.starts_with("text/event-stream")
            })
    {
        // Refresh on page loads and SSE reconnects so a server restart does not leave an
        // already-open dashboard with a stale nonce. Cross-origin forms cannot supply its
        // custom header. SameSite also prevents ambient use as a cross-site cookie.
        let secure = !access.origins.is_empty()
            && access
                .origins
                .iter()
                .all(|origin| origin.starts_with("https://"));
        let cookie = format!(
            "{COOKIE}={}; Path=/; SameSite=Strict{}",
            access.nonce,
            if secure { "; Secure" } else { "" }
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).expect("hex nonce"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access() -> OperatorAccess {
        OperatorAccess {
            nonce: "nonce".into(),
            origins: vec!["https://synth.example:9443".into()].into(),
            token_digest: Some(Sha256::digest(b"api secret").into()),
        }
    }

    #[test]
    fn cross_origin_and_ambient_posts_are_rejected() {
        let access = access();
        let mut headers = HeaderMap::new();
        assert!(!access.authorized(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        headers.insert(CSRF_HEADER, HeaderValue::from_static("nonce"));
        assert!(!access.authorized(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://synth.example:9443"),
        );
        assert!(access.authorized(&headers));
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!access.authorized(&headers));
    }

    #[test]
    fn api_calls_require_the_configured_bearer_token() {
        let access = access();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        assert!(!access.authorized(&headers));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer api secret"),
        );
        assert!(access.authorized(&headers));
        headers.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!access.authorized(&headers));
    }

    #[tokio::test]
    async fn rejected_post_never_reaches_the_operator_handler() {
        use axum::{body::Body, routing::post, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tower::ServiceExt;
        let starts = Arc::new(AtomicUsize::new(0));
        let handler_starts = starts.clone();
        let router = Router::new()
            .route(
                "/api/run",
                post(move || {
                    let starts = handler_starts.clone();
                    async move {
                        starts.fetch_add(1, Ordering::SeqCst);
                        StatusCode::ACCEPTED
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(access(), authorize));
        let attack = Request::builder()
            .method("POST")
            .uri("/api/run")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.clone().oneshot(attack).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        let operator = Request::builder()
            .method("POST")
            .uri("/api/run")
            .header(header::ORIGIN, "https://synth.example:9443")
            .header(CSRF_HEADER, "nonce")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.oneshot(operator).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }
}
