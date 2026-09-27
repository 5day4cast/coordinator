use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Inclusive, per-player wait bounds. Zero preserves the original immediate entry flow.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DelayRange {
    pub min_secs: u64,
    pub max_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct EntryTiming {
    pub arrival: DelayRange,
    pub before_payment: DelayRange,
    pub before_submit: DelayRange,
    pub deadline_margin_secs: u64,
}

impl EntryTiming {
    pub fn validate(&self, entry_window_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(entry_window_secs > 60 && entry_window_secs <= 86_400,
            "entry_window_secs must be between 61 and 86400 seconds (invoices close 60 seconds before observations)");
        for range in [self.arrival, self.before_payment, self.before_submit] {
            anyhow::ensure!(
                range.min_secs <= range.max_secs,
                "entry timing minimum exceeds maximum"
            );
        }
        let payment = self
            .arrival
            .max_secs
            .checked_add(self.before_payment.max_secs)
            .ok_or_else(|| anyhow::anyhow!("entry timing overflow"))?;
        let submit = payment
            .checked_add(self.before_submit.max_secs)
            .ok_or_else(|| anyhow::anyhow!("entry timing overflow"))?;
        anyhow::ensure!(payment.checked_add(self.deadline_margin_secs.max(60)).is_some_and(|end| end < entry_window_secs),
            "arrival and payment waits must finish before the invoice deadline, including the safety margin");
        anyhow::ensure!(
            submit
                .checked_add(self.deadline_margin_secs)
                .is_some_and(|end| end < entry_window_secs),
            "entry waits and deadline margin must fit inside the entry window"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryBehavior {
    #[default]
    Complete,
    AbandonUnpaid,
    AbandonPaid,
    DuplicateSubmission,
    LateSubmission,
}

/// Resolved before any competition is created. Arrival is measured from its creation;
/// stage waits must still respect the actual invoice and competition deadlines.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryPlan {
    pub user_index: usize,
    pub arrival_secs: u64,
    pub before_payment_secs: u64,
    pub before_submit_secs: u64,
    pub behavior: EntryBehavior,
}

/// Longer weather windows plus a deliberate short case for refund-path coverage.
pub fn default_observation_windows() -> Vec<u64> {
    vec![7200, 10800, 14400, 600]
}

/// Choose once per manual run; its saved ScenarioConfig keeps the resolved duration.
pub fn choose_observation_window(windows: &[u64]) -> u64 {
    use rand::Rng;
    windows[rand::rng().random_range(0..windows.len())]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioConfig {
    #[serde(default)]
    pub planned_scenario: Option<String>,
    /// Reproduce a run's selected timings and observation window. Chosen before recording if absent.
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub entry_timing: EntryTiming,
    #[serde(default)]
    pub entry_plan: Vec<EntryPlan>,
    /// Optional choices for a manual run; the resolved duration is saved below.
    #[serde(default)]
    pub observation_window_choices: Vec<u64>,
    /// Number of synthetic users
    pub users: usize,
    /// NOAA stations to use
    pub stations: Vec<String>,
    /// Entry fee in sats
    pub entry_fee: usize,
    /// Time before observation starts (entry window) in seconds
    pub entry_window_secs: u64,
    /// Observation window duration in seconds
    pub observation_window_secs: u64,
    /// Delay after observation ends before signing deadline, in seconds
    pub signing_delay_secs: u64,
    /// Max time to wait for each state transition (seconds)
    pub state_timeout_secs: u64,
    /// Poll interval for state transitions (seconds)
    pub poll_interval_secs: u64,
    /// The players' Lightning Address, where payouts and refunds go. The refund scenario needs
    /// one, since the enclave resolves a refund's address before signing anything; without one a
    /// cancelled entry's money stays in its escrow.
    #[serde(default)]
    pub lightning_address: Option<String>,
    /// The node a scenario pays entries from. Escrow scenarios need one, because ark-swapd
    /// funds an escrow from a real payment.
    #[serde(default)]
    pub lnd: Option<crate::lnd::LndConfig>,
    /// Max time to wait for a refund to settle (seconds), which includes its escrow's locktime.
    #[serde(default = "default_refund_timeout_secs")]
    pub refund_timeout_secs: u64,
}

fn default_refund_timeout_secs() -> u64 {
    30 * 60
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            planned_scenario: None,
            seed: None,
            entry_timing: EntryTiming::default(),
            entry_plan: vec![],
            observation_window_choices: vec![],
            users: 3,
            stations: vec!["KDEN".to_string(), "KJFK".to_string(), "KORD".to_string()],
            entry_fee: 1000,
            entry_window_secs: 120,
            observation_window_secs: default_observation_windows()[0],
            signing_delay_secs: 60,
            state_timeout_secs: 600,
            poll_interval_secs: 5,
            lightning_address: None,
            lnd: None,
            refund_timeout_secs: default_refund_timeout_secs(),
        }
    }
}

impl ScenarioConfig {
    pub fn resolve_plan(&self, scenario: &str) -> anyhow::Result<Self> {
        use rand::{Rng, SeedableRng};
        anyhow::ensure!(
            (1..=100).contains(&self.users),
            "users must be between 1 and 100"
        );
        let mut config = self.clone();
        if matches!(scenario, "abandoned_unpaid" | "paid_abandonment") {
            // The replacement cannot reserve this seat until the unpaid ticket's 10-minute
            // reservation expires; paid abandonment probes that the paid seat still cannot
            // be replaced at that point. Save this deadline before creating the competition.
            let required = [
                600,
                self.entry_timing.arrival.max_secs,
                self.entry_timing.before_payment.max_secs,
                self.entry_timing.before_submit.max_secs,
                self.entry_timing.deadline_margin_secs.max(60),
                120,
            ]
            .into_iter()
            .try_fold(0u64, u64::checked_add)
            .ok_or_else(|| anyhow::anyhow!("entry timing overflow"))?;
            config.entry_window_secs = config.entry_window_secs.max(required);
        }
        self.entry_timing.validate(config.entry_window_secs)?;
        let seed = self.seed.unwrap_or_else(rand::random);
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(seed);
        anyhow::ensure!(
            self.observation_window_choices
                .iter()
                .all(|seconds| (1..=604_800).contains(seconds)),
            "observation window choices must be between 1 second and 7 days"
        );
        if !self.observation_window_choices.is_empty() {
            // A fixed-duration replay must keep the same player plan. Select the weather
            // window from a separate stream so changing its choices consumes no player draws.
            let mut window_rng = rand_chacha::ChaCha20Rng::seed_from_u64(seed ^ 0x5749_4e44_4f57);
            config.observation_window_secs = self.observation_window_choices
                [window_rng.random_range(0..self.observation_window_choices.len())];
        }
        anyhow::ensure!(
            (1..=604_800).contains(&config.observation_window_secs),
            "observation window must be between 1 second and 7 days"
        );
        let behavior = match scenario {
            "full_lifecycle" | "escrow_refund" => EntryBehavior::Complete,
            "abandoned_unpaid" => EntryBehavior::AbandonUnpaid,
            "paid_abandonment" => EntryBehavior::AbandonPaid,
            "duplicate_submission" => EntryBehavior::DuplicateSubmission,
            "late_submission" => EntryBehavior::LateSubmission,
            _ => anyhow::bail!("Unknown scenario: {scenario}"),
        };
        let exceptional_user = rng.random_range(0..self.users);
        config.seed = Some(seed);
        config.planned_scenario = Some(scenario.to_string());
        config.entry_plan = (0..self.users)
            .map(|user_index| EntryPlan {
                user_index,
                arrival_secs: rng.random_range(
                    self.entry_timing.arrival.min_secs..=self.entry_timing.arrival.max_secs,
                ),
                before_payment_secs: rng.random_range(
                    self.entry_timing.before_payment.min_secs
                        ..=self.entry_timing.before_payment.max_secs,
                ),
                before_submit_secs: rng.random_range(
                    self.entry_timing.before_submit.min_secs
                        ..=self.entry_timing.before_submit.max_secs,
                ),
                behavior: if user_index == exceptional_user {
                    behavior
                } else {
                    EntryBehavior::Complete
                },
            })
            .collect();
        Ok(config)
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn timed_config() -> ScenarioConfig {
        ScenarioConfig {
            seed: Some(42),
            entry_window_secs: 600,
            observation_window_choices: default_observation_windows(),
            entry_timing: EntryTiming {
                arrival: DelayRange {
                    min_secs: 0,
                    max_secs: 90,
                },
                before_payment: DelayRange {
                    min_secs: 5,
                    max_secs: 60,
                },
                before_submit: DelayRange {
                    min_secs: 10,
                    max_secs: 120,
                },
                deadline_margin_secs: 60,
            },
            ..Default::default()
        }
    }

    #[test]
    fn seed_replays_every_player_wait_and_manual_observation_window() {
        let config = timed_config();
        let first = config.resolve_plan("duplicate_submission").unwrap();
        let second = config.resolve_plan("duplicate_submission").unwrap();
        assert_eq!(first.entry_plan, second.entry_plan);
        assert_eq!(
            first.observation_window_secs,
            second.observation_window_secs
        );
        assert_eq!(first.seed, Some(42));
        assert_eq!(
            first.planned_scenario.as_deref(),
            Some("duplicate_submission")
        );
        assert_eq!(
            first
                .entry_plan
                .iter()
                .filter(|plan| plan.behavior == EntryBehavior::DuplicateSubmission)
                .count(),
            1
        );
        for (index, plan) in first.entry_plan.iter().enumerate() {
            assert_eq!(plan.user_index, index);
            assert!(plan.arrival_secs <= 90);
            assert!((5..=60).contains(&plan.before_payment_secs));
            assert!((10..=120).contains(&plan.before_submit_secs));
        }
        let restored: ScenarioConfig =
            serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(restored.entry_plan, first.entry_plan);
        let mut fixed_window = config.clone();
        fixed_window.observation_window_choices.clear();
        fixed_window.observation_window_secs = first.observation_window_secs;
        assert_eq!(fixed_window.resolve_plan("duplicate_submission").unwrap().entry_plan, first.entry_plan,
            "replaying the recorded duration must not consume a different number of player RNG draws");
        let mut different = config;
        different.seed = Some(43);
        assert_ne!(
            different
                .resolve_plan("duplicate_submission")
                .unwrap()
                .entry_plan,
            first.entry_plan
        );
    }

    #[test]
    fn legacy_defaults_have_no_delays_and_abandonment_records_a_usable_window() {
        let config = ScenarioConfig::default();
        let normal = config.resolve_plan("full_lifecycle").unwrap();
        assert_eq!(normal.entry_window_secs, 120);
        assert!(normal.entry_plan.iter().all(|plan| plan.arrival_secs == 0
            && plan.before_payment_secs == 0
            && plan.before_submit_secs == 0
            && plan.behavior == EntryBehavior::Complete));
        let abandoned = timed_config().resolve_plan("abandoned_unpaid").unwrap();
        assert_eq!(abandoned.entry_window_secs, 1050);
        assert_eq!(
            timed_config()
                .resolve_plan("paid_abandonment")
                .unwrap()
                .entry_window_secs,
            1050
        );
        assert_eq!(
            abandoned
                .entry_plan
                .iter()
                .filter(|plan| plan.behavior == EntryBehavior::AbandonUnpaid)
                .count(),
            1
        );
    }

    #[test]
    fn invalid_ranges_and_invoice_deadline_overruns_fail_before_planning() {
        let mut config = ScenarioConfig::default();
        config.entry_timing.arrival = DelayRange {
            min_secs: 10,
            max_secs: 9,
        };
        assert!(config.resolve_plan("full_lifecycle").is_err());
        config.entry_timing.arrival = DelayRange {
            min_secs: 0,
            max_secs: 60,
        };
        assert!(
            config.resolve_plan("full_lifecycle").is_err(),
            "invoice cutoff is observation start minus 60 seconds"
        );
        config.entry_timing.arrival.max_secs = 10;
        config.entry_timing.before_submit.max_secs = 110;
        assert!(config.resolve_plan("full_lifecycle").is_err());
        config.entry_timing.before_submit.max_secs = u64::MAX;
        assert!(config.resolve_plan("abandoned_unpaid").is_err());
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    pub scenario: String,
    pub status: ScenarioStatus,
    pub steps: Vec<StepResult>,
    pub total_duration_ms: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioStatus {
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub name: String,
    pub status: StepStatus,
    pub duration_ms: i64,
    pub details: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Passed,
    Failed,
    Skipped,
}
