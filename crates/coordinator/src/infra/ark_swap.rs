//! The client for `ark-swapd`, which swaps a player's Lightning payment into their entry's escrow.
//!
//! The coordinator asks for a swap into a ticket's escrow address, shows the player its invoice,
//! and polls the swap until it settles. See `crates/coordinator-ark-swap`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where a swap is, as `ark-swapd` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapState {
    AwaitingPayment,
    PayingEscrow,
    EscrowPaid,
    Settled,
    Expired,
    Failed,
    Unsettled,
}

impl SwapState {
    /// The escrow holds the player's buy-in.
    pub fn escrow_funded(self) -> bool {
        matches!(
            self,
            SwapState::EscrowPaid | SwapState::Settled | SwapState::Unsettled
        )
    }

    /// The swap ended without funding the escrow; the player's payment failed back.
    pub fn abandoned(self) -> bool {
        matches!(self, SwapState::Expired | SwapState::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swap {
    pub id: Uuid,
    pub escrow_address: String,
    pub amount_sat: u64,
    pub payment_hash: String,
    pub invoice: String,
    pub state: SwapState,
    /// The escrow VTXO the swap paid, `txid:vout`.
    pub escrow_vtxo: Option<String>,
    pub ark_txid: Option<String>,
    pub error: Option<String>,
    pub expires_at: i64,
}

#[async_trait]
pub trait EscrowSwaps: Send + Sync {
    /// Start a swap of `amount_sat` into `escrow_address`, or return the open one for it.
    ///
    /// The invoice pays to `preimage`'s hash, so the player's proof of payment is the ticket preimage.
    async fn create_swap(
        &self,
        escrow_address: &str,
        amount_sat: u64,
        preimage: &[u8; 32],
    ) -> anyhow::Result<Swap>;

    async fn swap(&self, id: Uuid) -> anyhow::Result<Swap>;
}

pub struct SwapClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl SwapClient {
    pub fn new(base_url: &str, token: String) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
        })
    }

    async fn checked(response: reqwest::Response) -> anyhow::Result<Swap> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ark-swapd answered {status}: {body}");
        }
        Ok(response.json().await?)
    }
}

#[async_trait]
impl EscrowSwaps for SwapClient {
    async fn create_swap(
        &self,
        escrow_address: &str,
        amount_sat: u64,
        preimage: &[u8; 32],
    ) -> anyhow::Result<Swap> {
        let response = self
            .http
            .post(format!("{}/v1/swaps", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "escrow_address": escrow_address,
                "amount_sat": amount_sat,
                "preimage": hex::encode(preimage),
            }))
            .send()
            .await?;
        Self::checked(response).await
    }

    async fn swap(&self, id: Uuid) -> anyhow::Result<Swap> {
        let response = self
            .http
            .get(format!("{}/v1/swaps/{id}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::checked(response).await
    }
}
