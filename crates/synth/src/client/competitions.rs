use super::CoordinatorClient;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// The network fee a ticket issued now would carry, as the coordinator quotes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct NetworkFeeQuote {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub network_fee_sats: u64,
    /// No ticket is issued while the fee is more than this share of the entry fee, in basis
    /// points; 0 never pauses.
    #[serde(default)]
    pub pause_above_entry_bps: u64,
    /// No ticket for an Arkade competition is issued while the Arkade server is failing.
    #[serde(default)]
    pub arkade_unavailable: bool,
}

impl NetworkFeeQuote {
    /// What a player is told when no ticket for an `entry_fee_sats` entry is issued now, as the
    /// coordinator says it; None while tickets are issued.
    pub fn paused(&self, entry_fee_sats: u64) -> Option<&'static str> {
        if self.arkade_unavailable {
            return Some(
                "Entries are paused while the Arkade network recovers; try again in a little while",
            );
        }
        (self.enabled
            && self.pause_above_entry_bps > 0
            && u128::from(self.network_fee_sats) * 10_000
                > u128::from(entry_fee_sats) * u128::from(self.pause_above_entry_bps))
        .then_some("Entries are paused while Bitcoin network fees are high")
    }
}

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

/// A competition's funded Arkade escrows, and how many of their players were refunded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundProgress {
    pub escrowed: u64,
    pub refunded: u64,
    /// When the first escrow not refunded yet can be; not every coordinator says.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub opens_at: Option<OffsetDateTime>,
}

/// A step a competition reached, as the operator listener names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// A competition as the operator listener reports it: what synth needs to find escrows a
/// competition that never ran still holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorCompetition {
    pub id: Uuid,
    pub state: String,
    pub event_submission: serde_json::Value,
    #[serde(default)]
    pub total_paid_entries: u64,
    #[serde(default)]
    pub milestones: Vec<Milestone>,
    /// None when it has no funded escrows.
    #[serde(default)]
    pub refunds: Option<RefundProgress>,
    #[serde(default)]
    pub kind: CompetitionKind,
}

impl OperatorCompetition {
    fn reached(&self, name: &str) -> Option<OffsetDateTime> {
        self.milestones
            .iter()
            .find(|milestone| milestone.name == name)
            .map(|milestone| milestone.at)
    }

    /// When it stopped without running: cancelled or failed before its funding confirmed, or
    /// still waiting for entries after its observation window opened. None if it ran or may.
    pub fn did_not_run(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        if self.reached("funding_confirmed").is_some() {
            return None;
        }
        match self.state.as_str() {
            "cancelled" => self.reached("cancelled"),
            "failed" => self.reached("failed"),
            // A queued competition forms its pools at the start; it is not left unfilled.
            "created" if self.kind != CompetitionKind::Queued => {
                let start = self
                    .event_submission
                    .get("start_observation_date")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|at| {
                        OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
                            .ok()
                    })?;
                (start <= now).then_some(start)
            }
            _ => None,
        }
    }

    /// Paid escrows not refunded yet.
    pub fn unrefunded(&self) -> u64 {
        self.refunds.map_or(0, |refunds| {
            refunds.escrowed.saturating_sub(refunds.refunded)
        })
    }

    /// The entry fee, in sats, each escrow returns.
    pub fn entry_fee(&self) -> Option<u64> {
        self.event_submission
            .get("entry_fee")
            .and_then(serde_json::Value::as_u64)
    }
}

/// What the coordinator tells a player while the Arkade server is failing batch steps.
pub const ENTRIES_PAUSED_FOR_ARKADE: &str =
    "Entries are paused while the Arkade network recovers; try again in a little while";
/// What the coordinator tells a player while network fees are too high for the entry fee.
pub const ENTRIES_PAUSED_FOR_FEES: &str = "Entries are paused while Bitcoin network fees are high";

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
    /// The fewest players it would start with at the current network fees, while it takes
    /// entries; not every coordinator says.
    #[serde(default)]
    pub min_players_now: Option<u64>,
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

    /// The fewest players it would start with now: as the coordinator says, or else its terms'
    /// minimum, a queue's smallest pool or two players.
    pub fn min_players(&self) -> u64 {
        self.min_players_now.unwrap_or_else(|| {
            self.pool_rules
                .map_or(2, |rules| rules.min_players() as u64)
        })
    }

    /// On the public lists: a queue or pool always, a single competition unless unlisted.
    pub fn listed(&self) -> bool {
        self.kind != CompetitionKind::Single
            || !self
                .event_submission
                .get("unlisted")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
    }

    /// The stations its entries pick for.
    pub fn stations(&self) -> Vec<String> {
        self.event_submission
            .get("locations")
            .and_then(serde_json::Value::as_array)
            .map(|stations| {
                stations
                    .iter()
                    .filter_map(|station| station.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// When its entries close: when its observations start.
    pub fn entries_close(&self) -> Option<OffsetDateTime> {
        let at = self
            .event_submission
            .get("start_observation_date")?
            .as_str()?;
        OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()
    }

    /// Anyone can still enter: it is taking entries and has room. A pool's players come from its
    /// queue, so a pool never is.
    pub fn open_to_enter(&self, now: OffsetDateTime) -> bool {
        let room = match self.kind {
            CompetitionKind::Single => self.seats().is_none_or(|seats| self.total_entries < seats),
            CompetitionKind::Queued => self
                .max_entries
                .is_none_or(|max| self.paid_entries() < u64::from(max)),
            CompetitionKind::Pool => false,
        };
        room && matches!(self.inferred_status(), "created" | "collecting_entries")
            && self.entries_close().is_some_and(|close| now < close)
    }

    /// Everyone's paid entries: a queue's, in it and in its pools, or a single competition's.
    pub fn paid_entries(&self) -> u64 {
        self.entries.unwrap_or(self.total_paid_entries)
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

    /// What a ticket issued now would carry in network fees, and whether entries are paused.
    pub async fn network_fee_quote(&self) -> Result<NetworkFeeQuote> {
        let url = format!("{}/api/v1/network-fee", self.base_url());
        let resp = super::retry_transport(3, || async {
            anyhow::Ok(self.http().get(&url).send().await?)
        })
        .await
        .context("Failed to get the network fee")?;
        if !resp.status().is_success() {
            anyhow::bail!("Get network fee failed ({})", resp.status());
        }
        resp.json().await.context("Failed to parse the network fee")
    }

    /// Why the coordinator would refuse a ticket for an `entry_fee` entry now, or None if it
    /// would issue one: entries are paused while the Arkade network recovers or while network
    /// fees are high, or it answered 503 for want of a fee estimate.
    pub async fn entries_paused(&self, entry_fee: u64) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Quote {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            network_fee_sats: u64,
            #[serde(default)]
            pause_above_entry_bps: u64,
            #[serde(default)]
            arkade_unavailable: bool,
        }
        let url = format!("{}/api/v1/network-fee", self.base_url());
        let resp = super::retry_transport(3, || async {
            anyhow::Ok(self.http().get(&url).send().await?)
        })
        .await
        .context("Failed to get the network fee")?;
        if resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Ok(Some(resp.text().await.unwrap_or_default()));
        }
        if !resp.status().is_success() {
            anyhow::bail!("Get network fee failed ({})", resp.status());
        }
        let quote: Quote = resp
            .json()
            .await
            .context("Failed to parse the network fee")?;
        Ok(if quote.arkade_unavailable {
            Some(ENTRIES_PAUSED_FOR_ARKADE.into())
        } else if quote.enabled
            && quote.pause_above_entry_bps > 0
            && u128::from(quote.network_fee_sats) * 10_000
                > u128::from(entry_fee) * u128::from(quote.pause_above_entry_bps)
        {
            Some(ENTRIES_PAUSED_FOR_FEES.into())
        } else {
            None
        })
    }

    /// Current and recently completed competitions, following every response page.
    pub async fn list_competitions(&self) -> Result<Vec<CompetitionResponse>> {
        self.list_page("competitions", &[], None).await
    }

    /// Ask only for competitions that could still take entries.
    pub async fn list_open_competitions(&self) -> Result<Vec<CompetitionResponse>> {
        self.list_page("competitions", &[("status", "open".into())], None)
            .await
    }

    /// Every competition as the operator listener reports it, with its escrow refunds.
    pub async fn list_operator_competitions(&self) -> Result<Vec<OperatorCompetition>> {
        let url = format!("{}/api/v1/admin/competitions", self.admin_url());
        let resp = super::retry_transport(3, || async {
            anyhow::Ok(self.admin_get(&url).send().await?)
        })
        .await
        .context("Failed to list the operator's competitions")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("List the operator's competitions failed ({status}): {body}");
        }

        resp.json()
            .await
            .context("Failed to parse the operator's competitions")
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
