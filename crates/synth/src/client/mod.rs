pub mod admin;
pub mod auth;
pub mod competitions;
pub mod entries;
pub mod wallet;

use anyhow::{Context, Result};
use reqwest::{Client, RequestBuilder};
use std::{path::Path, sync::Arc};
use zeroize::Zeroizing;

/// HTTP client for the coordinator API
#[derive(Clone)]
pub struct CoordinatorClient {
    http: Client,
    base_url: String,
    admin_url: String,
    /// Operator bearer token for `admin_url`; never logged.
    admin_token: Option<Arc<Zeroizing<String>>>,
}

impl CoordinatorClient {
    pub fn new(base_url: &str, admin_url: Option<&str>) -> Self {
        Self {
            // A page or the tracker waiting on the coordinator must give up eventually.
            http: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_string(),
            admin_url: admin_url
                .unwrap_or(base_url)
                .trim_end_matches('/')
                .to_string(),
            admin_token: None,
        }
    }

    /// Read the operator token from `path` (surrounding whitespace ignored).
    pub fn with_admin_token_file(mut self, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read admin token file {}", path.display()))?,
        );
        let token = contents.trim();
        anyhow::ensure!(
            !token.is_empty(),
            "Admin token file {} is empty",
            path.display()
        );
        self.admin_token = Some(Arc::new(Zeroizing::new(token.to_owned())));
        Ok(self)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    /// A POST to the operator listener, carrying the bearer token when configured.
    pub fn admin_post(&self, url: &str) -> RequestBuilder {
        self.authorize_admin(self.http.post(url))
    }

    fn admin_get(&self, url: &str) -> RequestBuilder {
        self.authorize_admin(self.http.get(url))
    }

    fn authorize_admin(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.admin_token {
            Some(token) => request.bearer_auth(token.as_str()),
            None => request,
        }
    }
}

/// How long to wait before sending a request again after it failed to reach the coordinator,
/// times the attempts so far.
const TRANSPORT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

/// Send an idempotent request up to `attempts` times while it fails to reach the coordinator, as
/// right after either restarts. `send` builds the request afresh each time, so a signed auth
/// header is never replayed. A response, whatever its status, is returned as it is.
pub(crate) async fn retry_transport<F, Fut>(attempts: u32, mut send: F) -> Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response>>,
{
    let mut attempt = 1;
    loop {
        match send().await {
            Err(error) if attempt < attempts && transport_error(&error) => {
                log::warn!("request did not reach the coordinator, trying again: {error:#}");
                tokio::time::sleep(TRANSPORT_BACKOFF * attempt).await;
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// The request never got a response: the connection was refused or dropped.
fn transport_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|e| e.is_connect() || e.is_request())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::HeaderMap, routing::get, Json, Router};

    #[tokio::test]
    async fn a_request_that_never_reached_the_coordinator_is_sent_again() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().route("/", get(|| async { "up" }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let http = Client::new();
        let sent = std::sync::atomic::AtomicU32::new(0);
        // Refused twice, as by a restarting coordinator, then answered.
        let send = || async {
            let url = match sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 | 1 => "http://127.0.0.1:1/".to_owned(),
                _ => format!("{live}/"),
            };
            anyhow::Ok(http.get(url).send().await?)
        };
        let response = retry_transport(3, send).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "up");
        assert_eq!(sent.load(std::sync::atomic::Ordering::SeqCst), 3);

        // Three refusals are the limit, and an error that is not the transport's is final.
        sent.store(0, std::sync::atomic::Ordering::SeqCst);
        let refused = || async { anyhow::Ok(http.get("http://127.0.0.1:1/").send().await?) };
        assert!(retry_transport(3, refused).await.is_err());
        let failed = std::sync::atomic::AtomicU32::new(0);
        let not_transport = || async {
            failed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err::<reqwest::Response, _>(anyhow::anyhow!("refused to sign"))
        };
        assert!(retry_transport(3, not_transport).await.is_err());
        assert_eq!(failed.load(std::sync::atomic::Ordering::SeqCst), 1);

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn wallet_requests_use_the_authenticated_operator_listener() {
        const TOKEN: &str = "0123456789abcdef0123456789abcdef";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let admin_url = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .route(
                "/api/v1/wallet/balance",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
                    Json(serde_json::json!({"confirmed": 42, "unconfirmed": 3, "locked": 1}))
                }),
            )
            .route(
                "/api/v1/wallet/address",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
                    Json(serde_json::json!({"address": "bcrt1qtest"}))
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        // The public endpoint is deliberately unreachable. Wallet calls must use admin_url.
        let mut client = CoordinatorClient::new("http://127.0.0.1:1", Some(&admin_url));
        client.admin_token = Some(Arc::new(Zeroizing::new(TOKEN.to_owned())));

        assert_eq!(client.wallet_balance().await.unwrap().confirmed, 42);
        assert_eq!(client.wallet_address().await.unwrap().address, "bcrt1qtest");
        let public = client.http().get(client.base_url()).build().unwrap();
        assert!(!public.headers().contains_key("authorization"));

        server.abort();
        let _ = server.await;
    }
}
