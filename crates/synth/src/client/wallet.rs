use super::CoordinatorClient;
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BalanceResponse {
    pub confirmed: u64,
    pub unconfirmed: u64,
    #[serde(default)]
    pub locked: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddressResponse {
    pub address: String,
}

impl CoordinatorClient {
    /// Get wallet balance
    pub async fn wallet_balance(&self) -> Result<BalanceResponse> {
        let url = format!("{}/api/v1/wallet/balance", self.admin_url());
        let resp = self
            .admin_get(&url)
            .send()
            .await
            .context("Failed to get wallet balance")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Get wallet balance failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse balance response")
    }

    /// Get a new wallet address
    pub async fn wallet_address(&self) -> Result<AddressResponse> {
        let url = format!("{}/api/v1/wallet/address", self.admin_url());
        let resp = self
            .admin_get(&url)
            .send()
            .await
            .context("Failed to get wallet address")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Get wallet address failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse address response")
    }
}
