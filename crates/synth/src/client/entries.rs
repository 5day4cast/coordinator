use super::auth::create_auth_header;
use super::CoordinatorClient;
use anyhow::{Context, Result};
use coordinator_core::RegistrationAssignment;
use keymeld_sdk::types::RegistrationContext;
use nostr::Keys;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A protocol rejection, distinct from a network, decoding, or server failure.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("HTTP {status}: {message}")]
pub struct ApiRejection {
    pub status: u16,
    pub message: String,
}

impl ApiRejection {
    fn new(status: reqwest::StatusCode, body: String) -> Self {
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or(body);
        Self {
            status: status.as_u16(),
            message,
        }
    }

    pub fn is_no_capacity(&self) -> bool {
        self.status == 400 && self.message == "No ticket available for competition"
    }
}

pub enum EntrySubmission {
    Accepted(EntryResponse),
    Rejected(ApiRejection),
}

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
    /// The invoice the refund pays, and its hash. Older coordinators leave them out.
    #[serde(default)]
    pub invoice: Option<String>,
    #[serde(default)]
    pub payment_hash: Option<String>,
    pub updated_at: i64,
}

/// Response from requesting a ticket
#[derive(Debug, Clone, Deserialize)]
pub struct TicketResponse {
    pub ticket_id: Uuid,
    pub payment_request: String,
    pub payment_hash: String,
    pub amount_sats: u64,
    /// The price line by line. Coordinators from before the network fee leave these out.
    #[serde(default)]
    pub entry_fee_sats: Option<u64>,
    #[serde(default)]
    pub coordinator_fee_sats: Option<u64>,
    #[serde(default)]
    pub network_fee_sats: Option<u64>,
    #[serde(default)]
    pub ticket_price_sats: Option<u64>,
    pub keymeld_user_id: Uuid,
    pub keymeld_gateway_url: Option<String>,
    pub keymeld_session_id: Option<String>,
    pub keymeld_enclave_public_key: Option<String>,
    pub keymeld_registration: Option<RegistrationAssignment>,
}

impl TicketResponse {
    /// Check the ticket's price as a wallet does before paying: its lines add up to what the
    /// invoice charges, and its escrow may pay out no more than the ticket above the stake. The
    /// network fee is the coordinator's to set; it is fixed on the ticket when issued.
    pub fn check_price(&self) -> Result<()> {
        let entry_fee =
            match (
                self.entry_fee_sats,
                self.coordinator_fee_sats,
                self.network_fee_sats,
                self.ticket_price_sats,
            ) {
                (None, None, None, None) => None,
                (Some(entry), Some(coordinator), Some(network), Some(price)) => {
                    anyhow::ensure!(
                    entry.checked_add(coordinator).and_then(|v| v.checked_add(network))
                        == Some(price)
                        && price == self.amount_sats,
                    "Ticket price lines do not add up to its invoice: {entry} + {coordinator} + \
                     {network} for {price}, invoiced at {}",
                    self.amount_sats
                );
                    Some(entry)
                }
                _ => anyhow::bail!("Ticket price has only some of its lines"),
            };
        let escrow = self
            .keymeld_registration
            .as_ref()
            .and_then(|assignment| assignment.payout_policy.as_deref())
            .map(serde_json::from_str::<coordinator_core::keymeld::PayoutPolicy>)
            .transpose()?
            .and_then(|policy| policy.ark_escrow);
        if let (Some(escrow), Some(entry_fee)) = (escrow, entry_fee) {
            anyhow::ensure!(
                escrow.max_fee_sats.checked_add(entry_fee) <= Some(self.amount_sats),
                "The escrow may pay out {} sats beyond the {entry_fee} sat stake of a {} sat ticket",
                escrow.max_fee_sats,
                self.amount_sats
            );
        }
        Ok(())
    }
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

/// The Keymeld registration a player sends for their ticket before paying for it, so the ticket
/// can be refunded even if it is never used for an entry.
#[derive(Debug, Clone, Serialize)]
pub struct TicketRegistration {
    pub ephemeral_pubkey: String,
    pub encrypted_keymeld_private_key: String,
    pub keymeld_auth_pubkey: String,
    pub keymeld_registration_context: RegistrationContext,
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
            return Err(ApiRejection::new(status, body).into());
        }

        resp.json().await.context("Failed to parse ticket response")
    }

    /// Send the ticket's Keymeld registration, before paying for it (requires Nostr auth).
    pub async fn register_ticket(
        &self,
        keys: &Keys,
        competition_id: &Uuid,
        ticket_id: &Uuid,
        registration: &TicketRegistration,
    ) -> Result<()> {
        let url = format!(
            "{}/api/v1/competitions/{}/tickets/{}/registration",
            self.base_url(),
            competition_id,
            ticket_id
        );
        let body = serde_json::to_vec(registration)?;
        let auth = create_auth_header(keys, "POST", &url, Some(&body)).await?;
        let resp = self
            .http()
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .context("Failed to register the ticket")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Register ticket failed ({}): {}", status, body);
        }
        Ok(())
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
        match self.attempt_submit_entry(keys, entry).await? {
            EntrySubmission::Accepted(entry) => Ok(entry),
            EntrySubmission::Rejected(error) => Err(error.into()),
        }
    }

    /// Negative scenarios must inspect a real rejection, never count a transport error
    /// or an unreadable successful response as proof that an entry was refused.
    pub async fn attempt_submit_entry(
        &self,
        keys: &Keys,
        entry: &AddEntry,
    ) -> Result<EntrySubmission> {
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
            return Ok(EntrySubmission::Rejected(ApiRejection::new(status, body)));
        }

        Ok(EntrySubmission::Accepted(
            resp.json()
                .await
                .context("Failed to parse entry response")?,
        ))
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

#[cfg(test)]
mod ticket_price_tests {
    use super::*;

    fn ticket(value: serde_json::Value) -> TicketResponse {
        let mut base = serde_json::json!({
            "ticket_id": Uuid::nil(),
            "payment_request": "lnbc1",
            "payment_hash": "00",
            "amount_sats": 5_300,
            "keymeld_user_id": Uuid::nil(),
            "keymeld_gateway_url": null,
            "keymeld_session_id": null,
            "keymeld_enclave_public_key": null,
            "keymeld_registration": null,
        });
        base.as_object_mut()
            .unwrap()
            .extend(value.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn priced(network: u64, amount: u64) -> TicketResponse {
        ticket(serde_json::json!({
            "amount_sats": amount,
            "entry_fee_sats": 5_000,
            "coordinator_fee_sats": 250,
            "network_fee_sats": network,
            "ticket_price_sats": 5_250 + network,
        }))
    }

    #[test]
    fn a_ticket_with_a_network_fee_is_paid_at_its_full_price() {
        priced(50, 5_300).check_price().unwrap();
        priced(0, 5_250).check_price().unwrap();
        // An older coordinator sends no lines.
        ticket(serde_json::json!({})).check_price().unwrap();
    }

    #[test]
    fn price_lines_that_do_not_add_up_are_refused() {
        assert!(priced(50, 5_250).check_price().is_err());
        assert!(priced(50, 5_301).check_price().is_err());
        assert!(ticket(serde_json::json!({ "network_fee_sats": 50 }))
            .check_price()
            .is_err());
    }
}
