use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioConfig {
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
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            users: 3,
            stations: vec!["KDEN".to_string(), "KJFK".to_string(), "KORD".to_string()],
            entry_fee: 1000,
            entry_window_secs: 120,
            observation_window_secs: 300,
            signing_delay_secs: 60,
            state_timeout_secs: 600,
            poll_interval_secs: 5,
        }
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
