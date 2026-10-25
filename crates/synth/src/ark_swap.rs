//! Reading ark-swapd's wallet, which funds every Arkade escrow a scenario's entries pay for.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use rand::Rng;
use serde::Deserialize;
use zeroize::Zeroizing;

/// Where ark-swapd's API is, and the token it wants.
#[derive(Debug, Clone, Deserialize)]
pub struct ArkSwapConfig {
    /// For example `http://10.90.9.1:9737`, the host's endpoint for ark-swapd on the lab.
    pub url: String,
    /// ark-swapd's API token, the one the coordinator uses.
    pub token_file: PathBuf,
}

/// ark-swapd's wallet, as `GET /v1/wallet` reports it.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ArkWallet {
    pub boarding_address: String,
    pub confirmed_sat: u64,
    pub pre_confirmed_sat: u64,
    /// Confirmed coins at the boarding address that no batch has taken yet. An ark-swapd older
    /// than this field reports none.
    #[serde(default)]
    pub boarding_sat: u64,
    /// The part of the spendable coins with enough life left to pay an escrow. An ark-swapd
    /// older than this field reports none, and [`ArkWallet::payable`] falls back to all of them.
    #[serde(default)]
    pub payable_sat: Option<u64>,
    #[serde(default)]
    pub expiring_sat: Option<u64>,
    #[serde(default)]
    pub recoverable_sat: Option<u64>,
    /// UNIX seconds.
    #[serde(default)]
    pub earliest_expiry: Option<i64>,
    /// UNIX seconds. When a batch last took one of ark-swapd's boards or renewals.
    #[serde(default)]
    pub last_board_success_at: Option<i64>,
}

impl ArkWallet {
    /// What the wallet can pay escrows from now.
    pub fn spendable_sat(&self) -> u64 {
        self.confirmed_sat + self.pre_confirmed_sat
    }

    /// What the wallet can pay new escrows from: its payable coins where ark-swapd reports them.
    pub fn payable(&self) -> u64 {
        self.payable_sat.unwrap_or_else(|| self.spendable_sat())
    }
}

/// A swap of a Lightning payment into an escrow, as `GET /v1/swaps` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Swap {
    pub id: uuid::Uuid,
    pub escrow_address: String,
    pub amount_sat: u64,
    pub payment_hash: String,
    pub state: String,
    #[serde(default)]
    pub escrow_vtxo: Option<String>,
    #[serde(default)]
    pub ark_txid: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// UNIX seconds.
    #[serde(default)]
    pub created_at: i64,
}

pub struct ArkSwap {
    client: reqwest::Client,
    url: String,
    token: Zeroizing<String>,
}

impl ArkSwap {
    pub fn new(config: &ArkSwapConfig) -> Result<Self> {
        let token = Zeroizing::new(
            std::fs::read_to_string(&config.token_file)
                .with_context(|| format!("read {}", config.token_file.display()))?
                .trim()
                .to_string(),
        );
        anyhow::ensure!(!token.is_empty(), "the ark-swapd token file is empty");
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            url: config.url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Where ark-swapd's API is, for naming lookups an operator can repeat.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The swap whose invoice pays to `payment_hash`. None if ark-swapd has none, or is too old
    /// to look swaps up by hash.
    pub async fn swap_for(&self, payment_hash: &str) -> Result<Option<Swap>> {
        let response = self
            .client
            .get(format!("{}/v1/swaps", self.url))
            .query(&[("payment_hash", payment_hash)])
            .bearer_auth(self.token.as_str())
            .send()
            .await
            .context("ask ark-swapd for the swap")?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            anyhow::bail!("this ark-swapd cannot look swaps up by payment hash");
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ark-swapd refused ({status}): {body}");
        }
        Ok(Some(response.json().await?))
    }

    /// Swaps that paid an escrow, or say they did, without recording the escrow output they
    /// paid, oldest first. Fails on an ark-swapd too old to list them.
    pub async fn swaps_without_escrow_vtxo(&self) -> Result<Vec<Swap>> {
        let response = self
            .client
            .get(format!("{}/v1/swaps", self.url))
            .query(&[("without_escrow_vtxo", "true")])
            .bearer_auth(self.token.as_str())
            .send()
            .await
            .context("ask ark-swapd for swaps without an escrow output")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ark-swapd refused ({status}): {body}");
        }
        Ok(response.json().await?)
    }

    pub async fn wallet(&self) -> Result<ArkWallet> {
        // A locally deferred background read must not postpone a needed refill until the next
        // ten-minute rebalance. Respect swapd's Retry-After without consuming payment slots.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
        for attempt in 0..3 {
            let response = self
                .client
                .get(format!("{}/v1/wallet", self.url))
                .bearer_auth(self.token.as_str())
                .send()
                .await
                .context("ask ark-swapd for its wallet")?;
            let status = response.status();
            if status.is_success() {
                return Ok(response.json().await?);
            }
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let body = response.text().await.unwrap_or_default();
            if attempt < 2 && matches!(status.as_u16(), 429 | 503) {
                if let Some(seconds) = retry_after.filter(|seconds| *seconds <= 240) {
                    let jitter = rand::thread_rng().gen_range(250..=1000);
                    let delay = Duration::from_secs(seconds.max(1)) + Duration::from_millis(jitter);
                    // Leave the client's full request timeout inside the overall budget.
                    if tokio::time::Instant::now() + delay + Duration::from_secs(30) <= deadline {
                        log::info!(
                            "Ark wallet check deferred; retrying in {:.1}s",
                            delay.as_secs_f64()
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                }
            }
            anyhow::bail!("ark-swapd refused ({status}): {body}");
        }
        unreachable!("the final wallet attempt always returns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    async fn server(
        status: StatusCode,
        retry_after: Option<&'static str>,
        failures: usize,
    ) -> (
        ArkSwap,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let app = Router::new().route(
            "/v1/wallet",
            get(move || {
                let counted = counted.clone();
                async move {
                    if counted.fetch_add(1, Ordering::SeqCst) < failures {
                        let mut response = (status, "deferred").into_response();
                        if let Some(value) = retry_after {
                            response
                                .headers_mut()
                                .insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
                        }
                        response
                    } else {
                        axum::Json(serde_json::json!({
                            "boarding_address": "test-address", "confirmed_sat": 400000,
                            "pre_confirmed_sat": 0, "payable_sat": 400000
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("token");
        std::fs::write(&token_file, "test-token").unwrap();
        (
            ArkSwap::new(&ArkSwapConfig { url, token_file }).unwrap(),
            calls,
            task,
            directory,
        )
    }

    #[tokio::test]
    async fn a_quota_deferral_waits_then_reads_the_wallet_for_refill() {
        for status in [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let (swap, calls, task, _directory) = server(status, Some("1"), 1).await;
            let started = tokio::time::Instant::now();
            let wallet = swap.wallet().await.unwrap();
            assert_eq!(wallet.payable(), 400000);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert!(started.elapsed() >= Duration::from_secs(1));
            task.abort();
        }
    }

    #[tokio::test]
    async fn persistent_quota_deferrals_stop_after_three_attempts() {
        let (swap, calls, task, _directory) =
            server(StatusCode::SERVICE_UNAVAILABLE, Some("0"), usize::MAX).await;
        let started = tokio::time::Instant::now();
        assert!(swap.wallet().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(started.elapsed() >= Duration::from_secs(2));
        task.abort();
    }

    #[tokio::test]
    async fn other_errors_and_unbounded_waits_do_not_retry() {
        for (status, retry_after) in [
            (StatusCode::UNAUTHORIZED, Some("1")),
            (StatusCode::INTERNAL_SERVER_ERROR, Some("1")),
            (StatusCode::SERVICE_UNAVAILABLE, None),
            (StatusCode::SERVICE_UNAVAILABLE, Some("invalid")),
            (StatusCode::SERVICE_UNAVAILABLE, Some("210")),
            (StatusCode::SERVICE_UNAVAILABLE, Some("241")),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Some("18446744073709551615"),
            ),
        ] {
            let (swap, calls, task, _directory) = server(status, retry_after, usize::MAX).await;
            assert!(swap.wallet().await.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            task.abort();
        }
    }
}
