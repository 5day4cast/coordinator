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
    /// The coordinator fee in basis points (1000 = 10%).
    pub coordinator_fee_basis_points: u32,
    /// The same fee as a whole percent, for coordinators before basis points;
    /// newer ones check that it agrees.
    pub coordinator_fee_percentage: u32,
    pub total_competition_pool: usize,
    /// Keep the competition's oracle event off the oracle's public list. Set for the test
    /// competitions synth makes.
    pub unlisted: bool,
}

/// Request body for a queued competition: entries without a seat count, split into pools of
/// `min_players` to `max_pool_size` when registration closes at the observation start. Each pool
/// pays one winner and is its own competition.
#[derive(Debug, Clone, Serialize)]
pub struct CreateQueuedCompetition {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end_observation_date: OffsetDateTime,
    pub locations: Vec<String>,
    pub number_of_values_per_entry: usize,
    /// Each player's stake: a pool of `n` players funds `n` stakes.
    pub entry_fee: usize,
    pub coordinator_fee_basis_points: u32,
    pub coordinator_fee_percentage: u32,
    pub min_players: usize,
    pub max_pool_size: usize,
    /// The most entries the queue takes; the coordinator's default if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u32>,
}

/// What a `competitions` row is: a single competition, a queue, or one of a queue's pools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompetitionKind {
    /// A fixed seat count; coordinators before queued competitions only have these.
    #[default]
    Single,
    Queued,
    Pool,
}

/// One pool a queued competition formed when its registration closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSummary {
    /// The pool's own competition, which is also its oracle event.
    pub competition_id: Uuid,
    pub pool_index: u32,
    pub players: usize,
}

/// Competition response from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompetitionResponse {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub event_submission: serde_json::Value,
    #[serde(default)]
    pub total_entries: u64,
    #[serde(default)]
    pub total_paid_entries: u64,
    #[serde(default)]
    pub total_paid_out_entries: u64,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub failed_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub escrow_funds_confirmed_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub event_created_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub entries_submitted_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub contracted_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub signed_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub funding_broadcasted_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub funding_confirmed_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub invoices_settled_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub funding_settled_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub awaiting_attestation_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub outcome_broadcasted_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub delta_broadcasted_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub keymeld_keygen_completed_at: Option<OffsetDateTime>,
    /// The coordinator's own name for where the competition is.
    #[serde(default)]
    pub state: Option<String>,
    /// The output funding the contract: an Arkade transaction's, for an Arkade competition.
    #[serde(default)]
    pub funding_outpoint: Option<String>,
    #[serde(default)]
    pub errors: Vec<serde_json::Value>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expiry_broadcasted_at: Option<OffsetDateTime>,
    /// The oracle's attestation, hex: the discrete log of the deciding outcome's locking point.
    #[serde(default)]
    pub attestation: Option<String>,
    /// The oracle's locking points, one per outcome it can attest to.
    #[serde(default)]
    pub event_announcement: Option<serde_json::Value>,
    /// The contract: its players, funding value, and each outcome's payout weights.
    #[serde(default)]
    pub contract_parameters: Option<serde_json::Value>,
    /// The transaction settling the contract on the attested outcome, once broadcast.
    #[serde(default)]
    pub outcome_transaction: Option<serde_json::Value>,
    #[serde(default)]
    pub kind: CompetitionKind,
    /// A pool's queued competition.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// A pool's index among its queue's pools.
    #[serde(default)]
    pub pool_index: Option<u32>,
    /// When a queued competition split its entries into pools.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub pools_formed_at: Option<OffsetDateTime>,
    /// A queued competition's pool sizes.
    #[serde(default)]
    pub pool_rules: Option<coordinator_core::keymeld::pools::PoolRules>,
    /// A queued competition's paid entries, in the queue and in its pools.
    #[serde(default)]
    pub entries: Option<u64>,
    /// The most entries a queued competition takes.
    #[serde(default)]
    pub max_entries: Option<u32>,
    /// A queued competition's stake per player.
    #[serde(default)]
    pub stake_sats: Option<u64>,
    /// A queued competition's pools, by index; empty until they form.
    #[serde(default)]
    pub pools: Vec<PoolSummary>,
    /// The kickoff check an Arkade competition or pool passes before its contract is built.
    #[serde(default)]
    pub kickoff_check: Option<KickoffCheck>,
}

/// What a competition's kickoff check found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KickoffCheck {
    pub players: u64,
    pub min_players: u64,
    pub sat_per_vb: u64,
    pub passed: bool,
    /// A failed check waits for fees to fall until then before the competition is cancelled.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub retry_until: Option<OffsetDateTime>,
}

impl CompetitionResponse {
    /// Seats a single competition has: its players at most.
    pub fn seats(&self) -> Option<u64> {
        self.event_submission
            .get("total_allowed_entries")
            .and_then(serde_json::Value::as_u64)
    }

    /// Other players paid for every seat of a single competition, so none is left to take.
    pub fn seats_all_paid(&self) -> bool {
        self.seats()
            .is_some_and(|seats| self.total_paid_entries >= seats)
    }

    /// Its kickoff check failed and it was cancelled for it, refunding every entry. Not while the
    /// check is still waiting for fees to fall.
    pub fn failed_kickoff(&self) -> bool {
        (self.failed_at.is_some() || self.cancelled_at.is_some())
            && self
                .kickoff_check
                .as_ref()
                .is_some_and(|check| !check.passed)
    }

    /// When its kickoff check stops waiting for fees to fall, if it is waiting.
    pub fn kickoff_retry_until(&self) -> Option<OffsetDateTime> {
        self.kickoff_check
            .as_ref()
            .filter(|check| !check.passed)
            .and_then(|check| check.retry_until)
    }
}

impl CompetitionResponse {
    /// The outcome transaction's id, once it is broadcast.
    pub fn outcome_txid(&self) -> Option<String> {
        let transaction: dlctix::bitcoin::Transaction =
            serde_json::from_value(self.outcome_transaction.clone()?).ok()?;
        Some(transaction.compute_txid().to_string())
    }

    /// The funding transaction's id and the contract's output, from `txid:vout`.
    pub fn funding(&self) -> Option<(String, Option<u32>)> {
        let outpoint = self.funding_outpoint.as_deref()?;
        let (txid, vout) = outpoint.split_once(':').unwrap_or((outpoint, ""));
        Some((txid.to_string(), vout.parse().ok()))
    }

    /// Itself without the contract, signatures, and transactions: what a page needs to say where
    /// the competition got to, small enough to keep with each run.
    pub fn slim(mut self) -> Self {
        self.contract_parameters = None;
        self.event_announcement = None;
        self.outcome_transaction = None;
        self
    }
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
        } else if self.pools_formed_at.is_some() {
            // A queue's pools run the lifecycle from here; the queue itself has none.
            "pools_formed"
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
            .admin_post(&url)
            .json(competition)
            .send()
            .await
            .context("Failed to create competition")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Create competition failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse competition response")
    }

    /// Create a queued competition via the admin API
    pub async fn create_queued_competition(
        &self,
        competition: &CreateQueuedCompetition,
    ) -> Result<CompetitionResponse> {
        let url = format!("{}/api/v1/competitions/queued", self.admin_url());
        let resp = self
            .admin_post(&url)
            .json(competition)
            .send()
            .await
            .context("Failed to create queued competition")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Create queued competition failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse competition response")
    }

    /// The network fee rate the coordinator prices entries at, in sat/vB.
    pub async fn network_fee_rate(&self) -> Result<f64> {
        #[derive(Deserialize)]
        struct NetworkFee {
            sat_per_vb: f64,
        }
        let url = format!("{}/api/v1/network-fee", self.base_url());
        let resp = super::retry_transport(3, || async {
            anyhow::Ok(self.http().get(&url).send().await?)
        })
        .await
        .context("Failed to get the network fee")?;
        if !resp.status().is_success() {
            anyhow::bail!("Get network fee failed ({})", resp.status());
        }
        Ok(resp
            .json::<NetworkFee>()
            .await
            .context("Failed to parse the network fee")?
            .sat_per_vb)
    }

    /// List all competitions
    pub async fn list_competitions(&self) -> Result<Vec<CompetitionResponse>> {
        let url = format!("{}/api/v1/competitions", self.base_url());
        let resp = super::retry_transport(3, || async {
            anyhow::Ok(self.http().get(&url).send().await?)
        })
        .await
        .context("Failed to list competitions")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("List competitions failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse competitions response")
    }

    /// Get a specific competition by ID
    pub async fn get_competition(&self, id: &Uuid) -> Result<CompetitionResponse> {
        let url = format!("{}/api/v1/competitions/{}", self.base_url(), id);
        let resp = super::retry_transport(3, || async {
            anyhow::Ok(self.http().get(&url).send().await?)
        })
        .await
        .context("Failed to get competition")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Get competition failed ({}): {}", status, body);
        }

        resp.json()
            .await
            .context("Failed to parse competition response")
    }
}
