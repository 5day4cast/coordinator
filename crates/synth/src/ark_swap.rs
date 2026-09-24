//! Reading ark-swapd's wallet, which funds every Arkade escrow a scenario's entries pay for.

use std::path::PathBuf;

use anyhow::{Context, Result};
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
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ArkWallet {
    pub boarding_address: String,
    pub confirmed_sat: u64,
    pub pre_confirmed_sat: u64,
}

impl ArkWallet {
    /// What the wallet can pay escrows from now.
    pub fn spendable_sat(&self) -> u64 {
        self.confirmed_sat + self.pre_confirmed_sat
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

    pub async fn wallet(&self) -> Result<ArkWallet> {
        let response = self
            .client
            .get(format!("{}/v1/wallet", self.url))
            .bearer_auth(self.token.as_str())
            .send()
            .await
            .context("ask ark-swapd for its wallet")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ark-swapd refused ({status}): {body}");
        }
        Ok(response.json().await?)
    }
}
