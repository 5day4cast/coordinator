//! Reserve from swapd's durable shared GetVtxos quota before contacting Arkade.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ark_grpc::{VtxoBudgetError, VtxoRequestBudget, VtxoRequestPriority};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;

pub struct RemoteIndexerBudget {
    client: reqwest::Client,
    url: String,
}

impl RemoteIndexerBudget {
    pub fn new(swap_url: &str, token: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(!token.is_empty(), "the swapd budget token is empty");
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .default_headers(headers)
                .build()?,
            url: format!("{}/v1/indexer/permit", swap_url.trim_end_matches('/')),
        })
    }

    async fn request(&self, priority: VtxoRequestPriority) -> anyhow::Result<Option<Duration>> {
        let response = self
            .client
            .post(&self.url)
            .query(&[("priority", priority.as_str())])
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            #[derive(Deserialize)]
            struct Exhausted {
                retry_after_ms: u64,
            }
            let exhausted: Exhausted = response.json().await?;
            // Zero/overflow values must not turn a denial into a busy loop.
            return Ok(Some(Duration::from_millis(
                exhausted.retry_after_ms.clamp(1_000, 300_000),
            )));
        }
        anyhow::bail!("swapd's indexer budget returned {}", response.status())
    }
}

impl VtxoRequestBudget for RemoteIndexerBudget {
    fn reserve(
        &self,
        priority: VtxoRequestPriority,
    ) -> Pin<Box<dyn Future<Output = Result<(), VtxoBudgetError>> + Send + '_>> {
        Box::pin(async move {
            match self.request(priority).await {
                Ok(None) => Ok(()),
                Ok(Some(retry_after)) => Err(VtxoBudgetError {
                    retry_after,
                    message: "shared GetVtxos budget exhausted or capacity reserved for payments"
                        .into(),
                }),
                Err(error) => Err(VtxoBudgetError {
                    retry_after: Duration::from_secs(60),
                    message: format!(
                        "shared GetVtxos budget unavailable; request deferred: {error}"
                    ),
                }),
            }
        })
    }
}

/// Payment confirmation, spending, and refund checks can use the payment reserve.
/// Ordinary monitoring and maintenance remain background work by default.
pub async fn payment<F: Future>(future: F) -> F::Output {
    ark_grpc::with_vtxo_request_priority(VtxoRequestPriority::Payment, future).await
}

pub async fn configure(indexer_url: &str, swap_url: &str, token: &str) -> anyhow::Result<()> {
    ark_grpc::set_vtxo_request_budget(
        indexer_url,
        Arc::new(RemoteIndexerBudget::new(swap_url, token)?),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, routing::post, Router};

    #[tokio::test]
    async fn only_a_grant_allows_a_request_and_all_other_responses_fail_closed() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let app = Router::new()
            .route(
                "/grant/v1/indexer/permit",
                post(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers.get("authorization").unwrap(), "Bearer test-token");
                    StatusCode::NO_CONTENT
                }),
            )
            .route(
                "/exhausted/v1/indexer/permit",
                post(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(serde_json::json!({"retry_after_ms": 239_000})),
                    )
                }),
            )
            .route(
                "/zero/v1/indexer/permit",
                post(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(serde_json::json!({"retry_after_ms": 0})),
                    )
                }),
            )
            .route(
                "/broken/v1/indexer/permit",
                post(|| async { (StatusCode::TOO_MANY_REQUESTS, "invalid json") }),
            )
            .route(
                "/unavailable/v1/indexer/permit",
                post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            )
            .route(
                "/redirect/v1/indexer/permit",
                post(|| async {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [("location", "/grant/v1/indexer/permit")],
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let budget =
            |path: &str| RemoteIndexerBudget::new(&format!("{url}/{path}"), "test-token").unwrap();
        budget("grant")
            .reserve(VtxoRequestPriority::Payment)
            .await
            .unwrap();
        assert_eq!(
            budget("exhausted")
                .reserve(VtxoRequestPriority::Payment)
                .await
                .unwrap_err()
                .retry_after,
            Duration::from_secs(239)
        );
        assert_eq!(
            budget("zero")
                .reserve(VtxoRequestPriority::Payment)
                .await
                .unwrap_err()
                .retry_after,
            Duration::from_secs(1)
        );
        for path in ["broken", "unavailable", "redirect", "missing"] {
            assert_eq!(
                budget(path)
                    .reserve(VtxoRequestPriority::Payment)
                    .await
                    .unwrap_err()
                    .retry_after,
                Duration::from_secs(60)
            );
        }
        server.abort();
        let _ = server.await;
        assert!(budget("grant")
            .reserve(VtxoRequestPriority::Payment)
            .await
            .is_err());
    }
}
