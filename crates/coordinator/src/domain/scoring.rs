//! Score calculation for competition entries
//!
//! Ported from frontend/public/leader_board.js

use serde::{Deserialize, Serialize};

use crate::infra::oracle::{ValueOptions, WeatherChoices};

/// Observation data from the oracle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub station_id: String,
    pub wind_speed: Option<f64>,
    pub temp_high: Option<f64>,
    pub temp_low: Option<f64>,
}

/// Forecast data from the oracle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forecast {
    pub station_id: String,
    pub wind_speed: Option<f64>,
    pub temp_high: Option<f64>,
    pub temp_low: Option<f64>,
}

/// Entry with score details
#[derive(Debug, Clone)]
pub struct ScoredEntry {
    pub entry_id: String,
    pub raw_score: i32,
    /// Final score with timestamp tiebreaker factored in
    pub final_score: i64,
    pub details: Vec<ScoredStation>,
}

/// Score details for a single station
#[derive(Debug, Clone)]
pub struct ScoredStation {
    pub station_id: String,
    pub wind_speed: Option<ScoreDetail>,
    pub temp_high: Option<ScoreDetail>,
    pub temp_low: Option<ScoreDetail>,
}

/// Score detail for a single metric
#[derive(Debug, Clone)]
pub struct ScoreDetail {
    pub pick: ValueOptions,
    pub forecast: Option<f64>,
    pub observation: Option<f64>,
    pub score: i32,
}

/// Calculate option score based on forecast, observation, and pick
///
/// - If observation < forecast and pick is "under": 10 points
/// - If observation == forecast and pick is "par": 20 points
/// - If observation > forecast and pick is "over": 10 points
/// - Otherwise: 0 points
pub fn calculate_option_score(
    forecast_val: Option<f64>,
    observation_val: Option<f64>,
    pick: &ValueOptions,
) -> i32 {
    match (forecast_val, observation_val) {
        (Some(forecast), Some(observation)) => {
            if observation < forecast {
                // Observation was UNDER the forecast
                if matches!(pick, ValueOptions::Under) {
                    10
                } else {
                    0
                }
            } else if (observation - forecast).abs() < f64::EPSILON {
                // Exact match (par)
                if matches!(pick, ValueOptions::Par) {
                    20
                } else {
                    0
                }
            } else {
                // Observation was OVER the forecast
                if matches!(pick, ValueOptions::Over) {
                    10
                } else {
                    0
                }
            }
        }
        _ => 0, // No data available
    }
}

/// Calculate scores for a list of entries
///
/// Returns entries sorted by final_score (highest first)
pub fn calculate_scores(
    entries: &[(String, Vec<WeatherChoices>)],
    forecasts: &std::collections::HashMap<String, Forecast>,
    observations: &std::collections::HashMap<String, Observation>,
) -> Vec<ScoredEntry> {
    let mut scored_entries: Vec<ScoredEntry> = entries
        .iter()
        .map(|(entry_id, expected_observations)| {
            let mut raw_score = 0i32;
            let mut details = Vec::new();

            for choice in expected_observations {
                let station_id = &choice.stations;
                let forecast = forecasts.get(station_id);
                let observation = observations.get(station_id);

                let wind_speed_detail = choice.wind_speed.as_ref().map(|pick| {
                    let score = calculate_option_score(
                        forecast.and_then(|f| f.wind_speed),
                        observation.and_then(|o| o.wind_speed),
                        pick,
                    );
                    raw_score += score;
                    ScoreDetail {
                        pick: pick.clone(),
                        forecast: forecast.and_then(|f| f.wind_speed),
                        observation: observation.and_then(|o| o.wind_speed),
                        score,
                    }
                });

                let temp_high_detail = choice.temp_high.as_ref().map(|pick| {
                    let score = calculate_option_score(
                        forecast.and_then(|f| f.temp_high),
                        observation.and_then(|o| o.temp_high),
                        pick,
                    );
                    raw_score += score;
                    ScoreDetail {
                        pick: pick.clone(),
                        forecast: forecast.and_then(|f| f.temp_high),
                        observation: observation.and_then(|o| o.temp_high),
                        score,
                    }
                });

                let temp_low_detail = choice.temp_low.as_ref().map(|pick| {
                    let score = calculate_option_score(
                        forecast.and_then(|f| f.temp_low),
                        observation.and_then(|o| o.temp_low),
                        pick,
                    );
                    raw_score += score;
                    ScoreDetail {
                        pick: pick.clone(),
                        forecast: forecast.and_then(|f| f.temp_low),
                        observation: observation.and_then(|o| o.temp_low),
                        score,
                    }
                });

                details.push(ScoredStation {
                    station_id: station_id.clone(),
                    wind_speed: wind_speed_detail,
                    temp_high: temp_high_detail,
                    temp_low: temp_low_detail,
                });
            }

            // Calculate final score with timestamp tiebreaker
            // Extract timestamp from UUID v7 (first 48 bits)
            let final_score = calculate_final_score(entry_id, raw_score);

            ScoredEntry {
                entry_id: entry_id.clone(),
                raw_score,
                final_score,
                details,
            }
        })
        .collect();

    // Sort by final score (highest first)
    scored_entries.sort_by(|a, b| b.final_score.cmp(&a.final_score));

    scored_entries
}

/// Calculate final score with timestamp tiebreaker
///
/// The final score incorporates the entry's UUID v7 timestamp to break ties.
/// Earlier entries get slightly higher scores as a tiebreaker.
fn calculate_final_score(entry_id: &str, raw_score: i32) -> i64 {
    // Extract timestamp from UUID v7
    let clean_uuid = entry_id.replace('-', "");
    if clean_uuid.len() >= 12 {
        if let Ok(timestamp_bits) = u64::from_str_radix(&clean_uuid[..12], 16) {
            let time_millis = timestamp_bits & 0xFFFF_FFFF_FFFF;
            let timestamp_part = (time_millis % 10000) as i64;
            // Ensure unique scores even for 0 base scores
            let base = std::cmp::max(10000, (raw_score as i64) * 10000);
            return base - timestamp_part;
        }
    }
    // Fallback if UUID parsing fails
    (raw_score as i64) * 10000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_option_score_under() {
        // Observation (50) < Forecast (60), pick Under = 10 points
        assert_eq!(
            calculate_option_score(Some(60.0), Some(50.0), &ValueOptions::Under),
            10
        );
        // Wrong pick
        assert_eq!(
            calculate_option_score(Some(60.0), Some(50.0), &ValueOptions::Over),
            0
        );
    }

    #[test]
    fn test_option_score_over() {
        // Observation (70) > Forecast (60), pick Over = 10 points
        assert_eq!(
            calculate_option_score(Some(60.0), Some(70.0), &ValueOptions::Over),
            10
        );
        // Wrong pick
        assert_eq!(
            calculate_option_score(Some(60.0), Some(70.0), &ValueOptions::Under),
            0
        );
    }

    #[test]
    fn test_option_score_par() {
        // Observation == Forecast, pick Par = 20 points
        assert_eq!(
            calculate_option_score(Some(60.0), Some(60.0), &ValueOptions::Par),
            20
        );
        // Wrong pick
        assert_eq!(
            calculate_option_score(Some(60.0), Some(60.0), &ValueOptions::Over),
            0
        );
    }

    #[test]
    fn test_option_score_no_data() {
        assert_eq!(
            calculate_option_score(None, Some(60.0), &ValueOptions::Over),
            0
        );
        assert_eq!(
            calculate_option_score(Some(60.0), None, &ValueOptions::Over),
            0
        );
        assert_eq!(calculate_option_score(None, None, &ValueOptions::Over), 0);
    }
}
