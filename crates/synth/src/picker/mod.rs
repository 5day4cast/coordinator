//! Choose weather stations only from the oracle's eligible list.
//!
//! For each run of a lane with `picker.mode = "weather"`, the picker drops the stations the lane
//! used in its last few competitions, scores the rest on their forecast for the window, and takes
//! the best as the leader. The others are the best near the leader, in the same weather, or the
//! best far enough from each other. The pick and why are saved with the competition and shown on
//! the run's page. If eligibility or enough forecasts are unavailable, creation stops.

pub mod oracle;
pub mod score;
pub mod store;
pub mod view;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use log::{info, warn};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db::SynthDb;
use crate::runner::lanes::LaneConfig;
use crate::scenarios::ScenarioConfig;
use oracle::{OracleClient, StationInfo};
use score::Picked;

/// How long the oracle's station lists are reused before being asked for again.
const LIST_TTL: Duration = Duration::from_secs(600);

/// How a lane chooses its stations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Rank eligible stations from the lane's configured list.
    Fixed,
    /// The stations with the most weather in the competition's window.
    #[default]
    Weather,
}

/// Where a lane's stations are chosen from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Candidates {
    /// Every station the oracle can attest.
    #[default]
    Eligible,
    /// The lane's own stations.
    Configured,
}

/// What each part of a station's forecast counts for. Shares of their total, so they need not
/// add up to one.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Weights {
    /// The highest high less the lowest low.
    #[serde(default = "default_swing")]
    pub swing: f64,
    /// The strongest wind.
    #[serde(default = "default_wind")]
    pub wind: f64,
    /// The chance of precipitation and how much falls.
    #[serde(default = "default_precip")]
    pub precip: f64,
    /// Very hot, very cold, snow and ice.
    #[serde(default = "default_extremes")]
    pub extremes: f64,
}

fn default_swing() -> f64 {
    0.35
}
fn default_wind() -> f64 {
    0.25
}
fn default_precip() -> f64 {
    0.25
}
fn default_extremes() -> f64 {
    0.15
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            swing: default_swing(),
            wind: default_wind(),
            precip: default_precip(),
            extremes: default_extremes(),
        }
    }
}

impl Weights {
    pub fn total(&self) -> f64 {
        self.swing + self.wind + self.precip + self.extremes
    }
}

/// A lane's `picker` table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickerConfig {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub candidates: Candidates,
    /// Rank a station with an IATA code, a city people recognise, above an unknown one that
    /// scores less than 10% more.
    #[serde(default = "default_prefer_known_airports")]
    pub prefer_known_airports: bool,
    /// Take the others from within this many kilometres of the leader, in the same weather; at 0,
    /// pick each on its own score, at least 300 km from every other.
    #[serde(default)]
    pub cluster_km: f64,
    /// Leave out the stations picked for the lane's last this many competitions.
    #[serde(default)]
    pub recent_runs_to_avoid: usize,
    /// The days of clean observations the oracle looks back over for its eligible stations.
    #[serde(default = "default_eligible_days")]
    pub eligible_days: u32,
    #[serde(default)]
    pub weights: Weights,
}

fn default_prefer_known_airports() -> bool {
    true
}

fn default_eligible_days() -> u32 {
    3
}

impl Default for PickerConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Weather,
            candidates: Candidates::Eligible,
            prefer_known_airports: true,
            cluster_km: 600.0,
            recent_runs_to_avoid: 0,
            eligible_days: default_eligible_days(),
            weights: Weights::default(),
        }
    }
}

impl PickerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let weights = [
            self.weights.swing,
            self.weights.wind,
            self.weights.precip,
            self.weights.extremes,
        ];
        anyhow::ensure!(
            weights
                .iter()
                .all(|weight| weight.is_finite() && *weight >= 0.0)
                && self.weights.total() > 0.0,
            "picker weights must be zero or more, and not all zero"
        );
        anyhow::ensure!(
            self.cluster_km.is_finite() && self.cluster_km >= 0.0,
            "picker cluster_km must be zero or more"
        );
        anyhow::ensure!(
            (1..=31).contains(&self.eligible_days),
            "picker eligible_days must be between 1 and 31"
        );
        Ok(())
    }
}

/// A competition's stations as the picker chose them, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    pub lane: String,
    pub competition_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub window_start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub window_end: OffsetDateTime,
    /// Where the candidates came from, in words.
    pub source: String,
    /// Candidates left after the recent ones were dropped.
    pub candidates: usize,
    /// Of those, the ones with a forecast for the window.
    pub scored: usize,
    /// Stations left out for being in the lane's recent competitions.
    pub avoided: Vec<String>,
    pub picked: Vec<Picked>,
    /// One line on each station picked.
    pub explanation: String,
}

impl Pick {
    pub fn stations(&self) -> Vec<String> {
        self.picked
            .iter()
            .map(|pick| pick.station_id.clone())
            .collect()
    }
}

/// Successful eligible lists are reused for at most ten minutes. Failures are never cached.
type EligibleCache = HashMap<(u32, u64), (Instant, Vec<StationInfo>)>;

pub struct Picker {
    oracle: OracleClient,
    db: SynthDb,
    eligible: Mutex<EligibleCache>,
}

impl Picker {
    pub fn new(oracle_url: &str, db: SynthDb) -> Self {
        Self {
            oracle: OracleClient::new(oracle_url),
            db,
            eligible: Mutex::new(HashMap::new()),
        }
    }

    /// Choose only eligible stations with forecasts. Missing evidence stops this run.
    pub async fn choose(
        &self,
        lane: &LaneConfig,
        base: &ScenarioConfig,
        config: &mut ScenarioConfig,
    ) -> anyhow::Result<Pick> {
        let defaults = PickerConfig::default();
        let settings = lane.picker.as_ref().unwrap_or(&defaults);
        let configured = lane.configured_stations(base);
        let count = lane.stations_per_run.unwrap_or(configured.len());
        let start = config.observation_start.unwrap_or_else(|| {
            OffsetDateTime::now_utc() + time::Duration::seconds(config.entry_window_secs as i64)
        });
        let end = start + time::Duration::seconds(config.observation_window_secs as i64);

        let recent =
            match store::recent_stations(&self.db, &lane.name, settings.recent_runs_to_avoid).await
            {
                Ok(recent) => recent,
                Err(error) => {
                    warn!(
                        "Lane {}: cannot read its recent stations: {error:#}",
                        lane.name
                    );
                    HashSet::new()
                }
            };
        let window_hours = (config.observation_window_secs / 3600).clamp(1, 24);
        // One request for the stations and their forecasts where the oracle offers it, rather
        // than one per batch of stations: lanes starting together sent the oracle more than it
        // takes at once.
        let (source, all, forecasts) = match self
            .oracle
            .eligible_forecasts(settings.eligible_days, start, end)
            .await?
        {
            Some((eligible, forecasts)) => {
                anyhow::ensure!(
                    !eligible.is_empty(),
                    "Oracle lists no eligible stations; no competition was created"
                );
                let (source, all) = narrow(settings, configured, eligible);
                (source, all, Some(forecasts))
            }
            None => {
                let (source, all) = self.candidates(settings, configured, window_hours).await?;
                (source, all, None)
            }
        };
        let (avoided, candidates): (Vec<StationInfo>, Vec<StationInfo>) = all
            .into_iter()
            .partition(|station| recent.contains(&station.station_id));
        let ids: Vec<String> = candidates
            .iter()
            .map(|station| station.station_id.clone())
            .collect();
        let forecasts = match forecasts {
            Some(forecasts) => forecasts,
            None => self
                .oracle
                .forecasts(&ids, start, end)
                .await
                .unwrap_or_else(|error| {
                    warn!("Lane {}: no forecasts: {error:#}", lane.name);
                    Vec::new()
                }),
        };
        let scored = score::score(&candidates, &forecasts, &settings.weights);
        let scored_count = scored.len();
        let ranked = score::rank(scored, settings.prefer_known_airports);
        let picked = score::choose(&ranked, count, settings.cluster_km);
        anyhow::ensure!(picked.len() == count,
            "Only {} eligible stations have forecasts for this window; need {count}. No competition was created",
            picked.len());

        let pick = Pick {
            lane: lane.name.clone(),
            competition_id: *config.competition_id.get_or_insert_with(Uuid::now_v7),
            window_start: start,
            window_end: end,
            source,
            candidates: candidates.len(),
            scored: scored_count,
            avoided: avoided
                .into_iter()
                .map(|station| station.station_id)
                .collect(),
            explanation: score::explain(&picked),
            picked,
        };
        config.stations = pick.stations();
        info!("Lane {} picked {}", lane.name, pick.explanation);
        if let Err(error) = store::record(&self.db, &pick).await {
            warn!("Lane {}: cannot save its pick: {error:#}", lane.name);
        }
        Ok(pick)
    }

    async fn candidates(
        &self,
        settings: &PickerConfig,
        configured: &[String],
        window_hours: u64,
    ) -> anyhow::Result<(String, Vec<StationInfo>)> {
        let eligible = self.eligible(settings.eligible_days, window_hours).await?;
        Ok(narrow(settings, configured, eligible))
    }

    /// Check explicit manual choices and already selected plans before recording a run.
    pub async fn validate_stations(&self, config: &ScenarioConfig) -> anyhow::Result<()> {
        let eligible = self
            .eligible(
                default_eligible_days(),
                (config.observation_window_secs / 3600).clamp(1, 24),
            )
            .await?;
        anyhow::ensure!(
            !config.stations.is_empty(),
            "Choose at least one eligible station"
        );
        let rejected: Vec<_> = config
            .stations
            .iter()
            .filter(|id| !eligible.iter().any(|s| &s.station_id == *id))
            .collect();
        anyhow::ensure!(
            rejected.is_empty(),
            "Stations are not currently eligible: {}. No competition was created",
            rejected.into_iter().cloned().collect::<Vec<_>>().join(", ")
        );
        let start = config.observation_start.unwrap_or_else(|| {
            OffsetDateTime::now_utc() + time::Duration::seconds(config.entry_window_secs as i64)
        });
        let end = start + time::Duration::seconds(config.observation_window_secs as i64);
        let forecasts = self.oracle.forecasts(&config.stations, start, end).await?;
        anyhow::ensure!(
            config
                .stations
                .iter()
                .all(|id| forecasts.iter().any(|row| &row.station_id == id)),
            "Every selected station needs a forecast for this window. No competition was created"
        );
        Ok(())
    }

    pub async fn choose_default(&self, config: &mut ScenarioConfig) -> anyhow::Result<Pick> {
        let lane: LaneConfig = serde_json::from_value(serde_json::json!({
            "name": "automatic", "scenarios": ["full_lifecycle"],
            "stations_per_run": config.stations.len(),
            "observation_windows_secs": [config.observation_window_secs]
        }))?;
        self.choose(&lane, &config.clone(), config).await
    }

    async fn eligible(&self, days: u32, window_hours: u64) -> anyhow::Result<Vec<StationInfo>> {
        let mut cache = self.eligible.lock().await;
        if let Some((read, stations)) = cache.get(&(days, window_hours)) {
            if read.elapsed() < LIST_TTL {
                return Ok(stations.clone());
            }
        }
        // Never extend an expired success after an error, and never substitute a directory list.
        let stations = self
            .oracle
            .eligible(days, window_hours)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("Oracle eligibility is unavailable; no competition was created")
            })?;
        anyhow::ensure!(
            !stations.is_empty(),
            "Oracle lists no eligible stations; no competition was created"
        );
        cache.insert((days, window_hours), (Instant::now(), stations.clone()));
        Ok(stations)
    }
}

/// The eligible stations a lane draws from: all of them, or those of its own it lists.
fn narrow(
    settings: &PickerConfig,
    configured: &[String],
    eligible: Vec<StationInfo>,
) -> (String, Vec<StationInfo>) {
    let candidates =
        if settings.candidates == Candidates::Configured || settings.mode == Mode::Fixed {
            eligible
                .into_iter()
                .filter(|station| configured.contains(&station.station_id))
                .collect()
        } else {
            eligible
        };
    ("the oracle's eligible stations".into(), candidates)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::oracle::{Forecast, StationInfo};

    /// A local Oracle fixture for runner tests; exercises the real HTTP eligibility boundary.
    pub fn picker(db: crate::db::SynthDb) -> super::Picker {
        use axum::{routing::get, Json, Router};
        let app = Router::new()
            .route("/stations/eligible", get(|| async {
                Json(["KDEN", "KJFK", "KORD", "KSEA", "KBOS", "KATL", "KLAX"].into_iter()
                    .map(|id| serde_json::json!({"station_id":id,"latitude":40.0,"longitude":-100.0})).collect::<Vec<_>>())
            }))
            .route("/stations/forecasts", get(|| async {
                Json(["KDEN", "KJFK", "KORD", "KSEA", "KBOS", "KATL", "KLAX"].into_iter()
                    .map(|id| serde_json::json!({"station_id":id,"temp_low":30,"temp_high":70,"wind_speed":20})).collect::<Vec<_>>())
            }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        super::Picker::new(&url, db)
    }

    pub fn station(id: &str, iata: &str, at: (f64, f64)) -> StationInfo {
        StationInfo {
            station_id: id.into(),
            station_name: format!("{id} airport"),
            state: "CO".into(),
            iata_id: (!iata.is_empty()).then(|| iata.into()),
            latitude: Some(at.0),
            longitude: Some(at.1),
        }
    }

    pub fn forecast(id: &str, low: i64, high: i64, wind_kt: i64, chance: i64) -> Forecast {
        Forecast {
            station_id: id.into(),
            temp_low: low,
            temp_high: high,
            wind_speed: Some(wind_kt),
            precip_chance: Some(chance),
            rain_amt: None,
            snow_amt: None,
            ice_amt: None,
        }
    }
}

#[cfg(test)]
mod tests;
