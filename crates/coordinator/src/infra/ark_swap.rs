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

/// Where a refund swap is, as `ark-swapd` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefundState {
    Minted,
    Paid,
    Claimed,
    Reclaimable,
}

/// The swap an unused escrow's refund pays, on its way to the player's Lightning Address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefundSwap {
    pub id: Uuid,
    pub payment_hash: String,
    pub amount_sat: u64,
    /// The swap's PSBT `TapTree` field, hex, and its Ark address.
    pub swap_tap_tree: String,
    pub swap_address: String,
    pub state: RefundState,
    pub swap_vtxo: Option<String>,
    pub claim_txid: Option<String>,
    pub error: Option<String>,
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

    /// Mint the swap an unused escrow's refund pays, or return the one minted for this invoice.
    ///
    /// The swap's claim leaf commits to `payment_hash`, so the service is paid for the coins it
    /// claims. After `deadline`, UNIX seconds, the player's own entry key takes the swap back.
    async fn mint_refund(
        &self,
        payment_hash: &str,
        amount_sat: u64,
        player_key: &str,
        deadline: u32,
    ) -> anyhow::Result<RefundSwap>;

    /// Report the preimage that paying the player's invoice revealed, so the swap can be claimed.
    async fn refund_paid(&self, id: Uuid, preimage: &[u8; 32]) -> anyhow::Result<RefundSwap>;
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

    async fn checked<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> anyhow::Result<T> {
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

    async fn mint_refund(
        &self,
        payment_hash: &str,
        amount_sat: u64,
        player_key: &str,
        deadline: u32,
    ) -> anyhow::Result<RefundSwap> {
        let response = self
            .http
            .post(format!("{}/v1/refunds", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "payment_hash": payment_hash,
                "amount_sat": amount_sat,
                "player_key": player_key,
                "deadline": deadline,
            }))
            .send()
            .await?;
        Self::checked(response).await
    }

    async fn refund_paid(&self, id: Uuid, preimage: &[u8; 32]) -> anyhow::Result<RefundSwap> {
        let response = self
            .http
            .post(format!("{}/v1/refunds/{id}/paid", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "preimage": hex::encode(preimage) }))
            .send()
            .await?;
        Self::checked(response).await
    }
}
