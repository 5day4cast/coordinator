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
            http: Client::new(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::HeaderMap, routing::get, Json, Router};

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
