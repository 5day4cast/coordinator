//! Shared types between coordinator server and WASM client

use serde::{Deserialize, Serialize};

/// Comparison type for predictions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Over,
    Equal,
    Under,
}

/// Generic observation choice for predictions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationChoice {
    /// Generic identifier (station, team, ticker)
    pub source_id: String,
    /// Metric name (temp_high, score, price)
    pub metric: String,
    /// What user predicts
    pub prediction: Comparison,
}

/// Data source configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSourceConfig {
    /// Unique identifier for this data source
    pub id: String,
    /// Type of data source
    pub source_type: DataSourceType,
}

/// Type of data source - currently Weather only
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataSourceType {
    Weather { station_code: String },
    // Future variants (not implemented yet):
    // PriceFeed { ticker: String, exchange: String },
    // Sports { event_id: String, provider: String },
    // Custom { provider: String, params: serde_json::Value },
}

/// Observation window for events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationWindow {
    #[serde(with = "time::serde::rfc3339")]
    pub start: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end: time::OffsetDateTime,
}

/// Metric configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricConfig {
    /// Name of the metric
    pub name: String,
    /// Tolerance for "Equal" comparison
    pub comparison_tolerance: Option<f64>,
}

/// Scoring configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringConfig {
    /// Points for exact match (Equal/Par)
    pub exact_match_points: u32,
    /// Points for direction match (Over/Under correct)
    pub direction_match_points: u32,
    /// Tiebreaker method
    pub tiebreaker: Tiebreaker,
}

/// Tiebreaker methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tiebreaker {
    /// Older entry wins
    EarliestEntry,
    /// Highest total points wins
    HighestScore,
    /// Deterministic random
    Random { seed: [u8; 32] },
}

/// Competition status for API responses
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompetitionStatusKind {
    Created,
    CollectingEntries,
    AwaitingEscrow,
    EscrowConfirmed,
    AwaitingKeygen,
    AwaitingSigning,
    FundingReady,
    FundingBroadcasted,
    FundingConfirmed,
    AwaitingAttestation,
    OutcomeBroadcasted,
    DeltaBroadcasted,
    Completed,
    Failed,
    Cancelled,
}
