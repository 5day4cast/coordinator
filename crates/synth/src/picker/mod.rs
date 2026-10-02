//! Choosing a lane's stations for the weather: from every station the oracle can attest, or the
//! lane's own list, the ones with the most going on in the competition's window.
//!
//! For each run of a lane with `picker.mode = "weather"`, the picker drops the stations the lane
//! used in its last few competitions, scores the rest on their forecast for the window, and takes
//! the best as the leader. The others are the best near the leader, in the same weather, or the
//! best far enough from each other. The pick and why are saved with the competition and shown on
//! the run's page. Whatever the oracle cannot answer, the lane's own stations fill in for.

pub mod oracle;
pub mod score;
pub mod store;
pub mod view;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
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
use score::{Picked, Role};

/// How long the oracle's station lists are reused before being asked for again.
const LIST_TTL: Duration = Duration::from_secs(600);

/// How a lane chooses its stations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// `stations_per_run` drawn at random from the lane's stations.
    #[default]
    Fixed,
    /// The stations with the most weather in the competition's window.
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
    30
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
            self.eligible_days > 0,
            "picker eligible_days must be positive"
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

/// A list from the oracle and when it was read.
type Cached<T> = Option<(Instant, T)>;

/// Picks the stations of lanes that choose them for the weather.
pub struct Picker {
    oracle: OracleClient,
    db: SynthDb,
    /// The eligible stations by window length, None where the oracle does not offer the list.
    eligible: Mutex<HashMap<u64, (Instant, Option<Vec<StationInfo>>)>>,
    /// Every station the oracle knows, to place a lane's own stations.
    directory: Mutex<Cached<Vec<StationInfo>>>,
    /// Whether the missing eligible list has been warned about since it was last read.
    warned: AtomicBool,
}

impl Picker {
    pub fn new(oracle_url: &str, db: SynthDb) -> Self {
        Self {
            oracle: OracleClient::new(oracle_url),
            db,
            eligible: Mutex::new(HashMap::new()),
            directory: Mutex::new(None),
            warned: AtomicBool::new(false),
        }
    }

    /// Choose `config`'s stations for the weather, if `lane` picks that way, and save the pick
    /// with the competition, whose id is chosen here if it is not yet. Returns the pick. Any
    /// station the oracle cannot help with comes from the lane's own list.
    pub async fn choose(
        &self,
        lane: &LaneConfig,
        base: &ScenarioConfig,
        config: &mut ScenarioConfig,
    ) -> Option<Pick> {
        let settings = lane.picker.as_ref()?;
        if settings.mode != Mode::Weather {
            return None;
        }
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
        let (source, all) = self.candidates(settings, configured, window_hours).await;
        let (avoided, candidates): (Vec<StationInfo>, Vec<StationInfo>) = all
            .into_iter()
            .partition(|station| recent.contains(&station.station_id));
        let ids: Vec<String> = candidates
            .iter()
            .map(|station| station.station_id.clone())
            .collect();
        let forecasts = self
            .oracle
            .forecasts(&ids, start, end)
            .await
            .unwrap_or_else(|error| {
                warn!("Lane {}: no forecasts: {error:#}", lane.name);
                Vec::new()
            });
        let scored = score::score(&candidates, &forecasts, &settings.weights);
        let scored_count = scored.len();
        let ranked = score::rank(scored, settings.prefer_known_airports);
        let mut picked = score::choose(&ranked, count, settings.cluster_km);
        if picked.len() < count {
            warn!(
                "Lane {}: {} of {} candidates have a forecast for the window; the rest of its \
                 {count} stations come from its own list",
                lane.name,
                scored_count,
                candidates.len()
            );
            fill(&mut picked, count, &config.stations, configured, &recent);
        }

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
        Some(pick)
    }

    /// The candidates, and where they came from in words: the eligible stations, or the lane's
    /// own if it ranks only those or the oracle does not offer the list.
    async fn candidates(
        &self,
        settings: &PickerConfig,
        configured: &[String],
        window_hours: u64,
    ) -> (String, Vec<StationInfo>) {
        let eligible = self.eligible(settings.eligible_days, window_hours).await;
        if settings.candidates == Candidates::Eligible {
            if let Some(eligible) = eligible {
                return ("the oracle's eligible stations".into(), eligible);
            }
        }
        let known = match eligible {
            Some(eligible) => eligible,
            None => self.directory().await,
        };
        let located = configured
            .iter()
            .map(|id| {
                known
                    .iter()
                    .find(|station| &station.station_id == id)
                    .cloned()
                    .unwrap_or_else(|| StationInfo::unknown(id))
            })
            .collect();
        let source = match settings.candidates {
            Candidates::Configured => "the lane's stations",
            Candidates::Eligible => "the lane's stations, for want of the oracle's eligible list",
        };
        (source.into(), located)
    }

    /// The oracle's eligible stations, read at most every [`LIST_TTL`]; None if it does not
    /// offer the list or cannot be asked, which is warned about once until it can again.
    async fn eligible(&self, days: u32, window_hours: u64) -> Option<Vec<StationInfo>> {
        let mut cache = self.eligible.lock().await;
        if let Some((read, stations)) = cache.get(&window_hours) {
            if read.elapsed() < LIST_TTL {
                return stations.clone();
            }
        }
        let stations = match self.oracle.eligible(days, window_hours).await {
            Ok(Some(stations)) if !stations.is_empty() => {
                self.warned.store(false, Ordering::Relaxed);
                Some(stations)
            }
            other => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    let why = match other {
                        Ok(Some(_)) => "lists no stations".to_string(),
                        Ok(None) => "does not offer it yet".to_string(),
                        Err(error) => format!("{error:#}"),
                    };
                    warn!("Picking from the lanes' own stations: the oracle's eligible list {why}");
                }
                None
            }
        };
        cache.insert(window_hours, (Instant::now(), stations.clone()));
        stations
    }

    /// Every station the oracle knows, read at most every [`LIST_TTL`]; none if it cannot say.
    async fn directory(&self) -> Vec<StationInfo> {
        let mut cache = self.directory.lock().await;
        if let Some((read, stations)) = cache.as_ref() {
            if read.elapsed() < LIST_TTL {
                return stations.clone();
            }
        }
        let stations = self.oracle.stations().await.unwrap_or_else(|error| {
            warn!("Cannot place the lanes' stations: {error:#}");
            Vec::new()
        });
        *cache = Some((Instant::now(), stations.clone()));
        stations
    }
}

/// Make `picked` up to `count` from the lane's own stations: those `drawn` for the run first,
/// then the rest of its list, those not used recently before those that were.
fn fill(
    picked: &mut Vec<Picked>,
    count: usize,
    drawn: &[String],
    configured: &[String],
    recent: &HashSet<String>,
) {
    let fresh = drawn
        .iter()
        .chain(configured)
        .filter(|id| !recent.contains(*id));
    let stale = drawn
        .iter()
        .chain(configured)
        .filter(|id| recent.contains(*id));
    for id in fresh.chain(stale) {
        if picked.len() >= count {
            break;
        }
        if picked.iter().all(|pick| &pick.station_id != id) {
            picked.push(Picked {
                station_id: id.clone(),
                role: Role::Fallback,
                scored: None,
                km_from_leader: None,
            });
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::oracle::{Forecast, StationInfo};

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
