use super::CoordinatorClient;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Request body for creating a competition
#[derive(Debug, Clone, Serialize)]
pub struct CreateCompetition {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end_observation_date: OffsetDateTime,
    pub locations: Vec<String>,
    pub number_of_values_per_entry: usize,
    pub number_of_places_win: usize,
    pub total_allowed_entries: usize,
    pub entry_fee: usize,
    pub coordinator_fee_percentage: usize,
    pub total_competition_pool: usize,
}

/// Competition response from the API
#[derive(Debug, Clone, Deserialize)]
pub struct CompetitionResponse {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub event_submission: serde_json::Value,
    pub total_entries: u64,
    pub total_paid_entries: u64,
    pub total_paid_out_entries: u64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub failed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    // Lifecycle timestamps
    #[serde(with = "time::serde::rfc3339::option")]
    pub escrow_funds_confirmed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub event_created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub entries_submitted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub contracted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub signed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_broadcasted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_confirmed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub invoices_settled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub funding_settled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub awaiting_attestation_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub outcome_broadcasted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub delta_broadcasted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub keymeld_keygen_completed_at: Option<OffsetDateTime>,
}

impl CompetitionResponse {
    /// Infer the current status from lifecycle timestamps
    pub fn inferred_status(&self) -> &'static str {
        if self.completed_at.is_some() {
            "completed"
        } else if self.failed_at.is_some() {
            "failed"
        } else if self.cancelled_at.is_some() {
            "cancelled"
        } else if self.delta_broadcasted_at.is_some() {
            "delta_broadcasted"
        } else if self.outcome_broadcasted_at.is_some() {
            "outcome_broadcasted"
        } else if self.awaiting_attestation_at.is_some() {
            "awaiting_attestation"
        } else if self.funding_settled_at.is_some() {
            "funding_settled"
        } else if self.funding_confirmed_at.is_some() {
            "funding_confirmed"
        } else if self.funding_broadcasted_at.is_some() {
            "funding_broadcasted"
        } else if self.signed_at.is_some() {
            "signing_complete"
        } else if self.contracted_at.is_some() {
            "contract_created"
        } else if self.entries_submitted_at.is_some() {
            "entries_submitted"
        } else if self.event_created_at.is_some() {
            "event_created"
        } else if self.escrow_funds_confirmed_at.is_some() {
            "escrow_confirmed"
        } else if self.total_entries > 0 {
            "collecting_entries"
        } else {
            "created"
        }
    }
}

impl CoordinatorClient {
    /// Create a new competition via the admin API
    pub async fn create_competition(
        &self,
        competition: &CreateCompetition,
    ) -> Result<CompetitionResponse> {
        let url = format!("{}/api/v1/competitions", self.admin_url());
        let resp = self
            .http()
            .post(&url)
            .json(competition)
            .send()
            .await
            .context("Failed to create competition")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Create competition failed ({}): {}", status, body);
        }

        resp.json().await.context("Failed to parse competition response")
    }

    /// List all competitions
    pub async fn list_competitions(&self) -> Result<Vec<CompetitionResponse>> {
        let url = format!("{}/api/v1/competitions", self.base_url());
        let resp = self
            .http()
            .get(&url)
            .send()
            .await
            .context("Failed to list competitions")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("List competitions failed ({}): {}", status, body);
        }

        resp.json().await.context("Failed to parse competitions response")
    }

    /// Get a specific competition by ID
    pub async fn get_competition(&self, id: &Uuid) -> Result<CompetitionResponse> {
        let url = format!("{}/api/v1/competitions/{}", self.base_url(), id);
        let resp = self
            .http()
            .get(&url)
            .send()
            .await
            .context("Failed to get competition")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Get competition failed ({}): {}", status, body);
        }

        resp.json().await.context("Failed to parse competition response")
    }
}
