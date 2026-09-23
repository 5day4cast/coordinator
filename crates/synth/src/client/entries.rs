use super::auth::create_auth_header;
use super::CoordinatorClient;
use anyhow::{Context, Result};
use coordinator_core::RegistrationAssignment;
use keymeld_sdk::types::RegistrationContext;
use nostr::Keys;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Request body for requesting a competition ticket
#[derive(Debug, Clone, Serialize)]
pub struct TicketRequest {
    pub btc_pubkey: String,
    pub payout: Option<coordinator_core::PayoutRegistrationRequest>,
}

/// A ticket's refund, as the coordinator reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketRefund {
    /// minted, submitting, submitted, paid, or settled.
    pub state: String,
    pub paid_sats: u64,
    pub ark_txid: Option<String>,
    pub updated_at: i64,
}

/// Response from requesting a ticket
#[derive(Debug, Clone, Deserialize)]
pub struct TicketResponse {
    pub ticket_id: Uuid,
    pub payment_request: String,
    pub payment_hash: String,
    pub amount_sats: u64,
    pub keymeld_user_id: Uuid,
    pub keymeld_gateway_url: Option<String>,
    pub keymeld_session_id: Option<String>,
    pub keymeld_enclave_public_key: Option<String>,
    pub keymeld_registration: Option<RegistrationAssignment>,
}

/// Weather prediction choices for an entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherChoices {
    pub stations: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_speed: Option<ValueOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_high: Option<ValueOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_low: Option<ValueOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ValueOption {
    Over,
    Par,
    Under,
}

/// Request body for submitting an entry
#[derive(Debug, Clone, Serialize)]
pub struct AddEntry {
    pub id: Uuid,
    pub ticket_id: Uuid,
    pub ephemeral_pubkey: String,
    pub payout_hash: String,
    pub event_id: Uuid,
    pub expected_observations: Vec<WeatherChoices>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_keymeld_private_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keymeld_auth_pubkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keymeld_registration_context: Option<RegistrationContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keymeld_escrow_policy: Option<coordinator_core::keymeld::SignedEscrowPolicy>,
}

/// Entry response from the API
#[derive(Debug, Clone, Deserialize)]
pub struct EntryResponse {
    pub id: Uuid,
    pub event_id: Uuid,
    pub ticket_id: Uuid,
    pub pubkey: String,
    pub ephemeral_pubkey: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub signed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_at: Option<OffsetDateTime>,
    /// When the latest payout was sent, or completed; the coordinator does not say which.
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_out_at: Option<OffsetDateTime>,
    /// The invoice the latest payout paid.
    #[serde(default)]
    pub payout_ln_invoice: Option<String>,
}

/// Ticket status from the coordinator API
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub enum TicketStatus {
    Created,
    Reserved,
    Paid,
    Settled,
    Used,
    Expired,
    Cancelled,
}

impl CoordinatorClient {
    /// Request a competition ticket (requires Nostr auth)
    pub async fn request_ticket(
        &self,
        keys: &Keys,
        competition_id: &Uuid,
        btc_pubkey: &str,
        payout: Option<coordinator_core::PayoutRegistrationRequest>,
    ) -> Result<TicketResponse> {
        let url = format!(
            "{}/api/v1/competitions/{}/ticket",
            self.base_url(),
            competition_id
        );

        let body = TicketRequest {
            btc_pubkey: btc_pubkey.to_string(),
            payout,
        };

        let body = serde_json::to_vec(&body)?;
        let auth = create_auth_header(keys, "POST", &url, Some(&body)).await?;

        let resp = self
            .http()
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .context("Failed to request ticket")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Request ticket failed ({}): {}", status, body);
        }

        resp.json().await.context("Failed to parse ticket response")
    }

    /// Check ticket payment status
    pub async fn check_ticket_status(
        &self,
        keys: &Keys,
        competition_id: &Uuid,
        ticket_id: &Uuid,
    ) -> Result<TicketStatus> {
        let url = format!(
            "{}/api/v1/competitions/{}/tickets/{}/status",
            self.base_url(),
            competition_id,
            ticket_id
        );

        let auth = create_auth_header(keys, "GET", &url, None).await?;

        let resp = self
            .http()
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .context("Failed to check ticket status")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Check ticket status failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse ticket status response")
    }

    /// Where a ticket's refund has got to, if its competition never kicked off.
    pub async fn check_ticket_refund(
        &self,
        keys: &Keys,
        competition_id: &Uuid,
        ticket_id: &Uuid,
    ) -> Result<Option<TicketRefund>> {
        let url = format!(
            "{}/api/v1/competitions/{}/tickets/{}/refund",
            self.base_url(),
            competition_id,
            ticket_id
        );

        let auth = create_auth_header(keys, "GET", &url, None).await?;

        let resp = self
            .http()
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .context("Failed to check ticket refund")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Check ticket refund failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse ticket refund response")
    }

    /// Submit an entry (requires Nostr auth)
    pub async fn submit_entry(&self, keys: &Keys, entry: &AddEntry) -> Result<EntryResponse> {
        let url = format!("{}/api/v1/entries", self.base_url());

        let body = serde_json::to_vec(entry)?;
        let auth = create_auth_header(keys, "POST", &url, Some(&body)).await?;

        let resp = self
            .http()
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .context("Failed to submit entry")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Submit entry failed ({}): {}", status, body);
        }

        resp.json().await.context("Failed to parse entry response")
    }

    /// List entries for a user (requires Nostr auth)
    pub async fn list_entries(
        &self,
        keys: &Keys,
        competition_id: Option<&Uuid>,
    ) -> Result<Vec<EntryResponse>> {
        let mut url = format!("{}/api/v1/entries", self.base_url());
        if let Some(id) = competition_id {
            url = format!("{}?event_id={}", url, id);
        }

        let auth = create_auth_header(keys, "GET", &url, None).await?;

        let resp = self
            .http()
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .context("Failed to list entries")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("List entries failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse entries response")
    }
}
