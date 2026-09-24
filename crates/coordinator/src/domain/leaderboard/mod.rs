//! A competition's leaderboard: every paid entry, ranked, with each pick's progress.
//!
//! [`Leaderboards::get`] is the one call a page needs. It returns [`Leaderboard`], whose rows are
//! in payout order and carry everything to render:
//!
//! - the standing to show (rank, shared on ties), the full entry id, the player, and the prize;
//! - per pick: the forecast, the value observed so far, how many hours of the window that covers
//!   and when it was reported, the points it would score now, and a [`PickState`];
//! - `provisional` until the oracle has attested the result. Provisional scores and ranks say
//!   "if the window ended now"; the final ones are the oracle's attested scores, unchanged.
//!
//! Picks are withheld until the observation window opens, when entries have closed.
//!
//! The oracle's weather is read through a cache that refreshes in the background, a few minutes
//! apart while a window is open, so a page never waits on the oracle: the first view of a
//! competition waits at most [`FIRST_READ_WAIT`], then shows what it has.

pub mod progress;

use std::{collections::HashMap, sync::Arc, time::Duration};

use log::{debug, warn};
use nostr::ToBech32;
use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

pub use progress::{Metric, PickState, Standing};

use crate::{
    domain::{Competition, Coordinator, EntryStatus, Error, UserEntry, UserInfo},
    infra::{
        oracle::ValueOptions,
        oracle_weather::{EventReadings, OracleWeather, Reading, StationObservations},
        refresh_cache::{Fetched, RefreshCache},
    },
    templates::admin::dashboard::Station,
};

/// Longest a page waits for a competition's first weather fetch.
pub const FIRST_READ_WAIT: Duration = Duration::from_millis(250);
/// Refresh interval while a window is open or awaiting its score. Stations report hourly.
const LIVE_TTL: Duration = Duration::from_secs(3 * 60);
/// Before a window opens only the forecast changes, about hourly.
const UPCOMING_TTL: Duration = Duration::from_secs(10 * 60);
/// An attested competition's weather is final.
const SCORED_TTL: Duration = Duration::from_secs(60 * 60);
const STATIONS_TTL: Duration = Duration::from_secs(60 * 60);
/// How often the background task refreshes open competitions ahead of any page.
const WARM_EVERY: Duration = Duration::from_secs(2 * 60);
/// Competitions whose weather is fetched from the oracle at once.
const ORACLE_REQUESTS: usize = 2;
/// How long after its window a competition's weather is still refreshed ahead of pages.
const WARM_AFTER_END: time::Duration = time::Duration::days(1);

/// The oracle's weather for one competition.
#[derive(Debug, Clone)]
pub struct CompetitionWeather {
    pub event: EventReadings,
    /// Observations over the window so far, or `None` when that request failed.
    pub observations: Option<Vec<StationObservations>>,
    /// Forecasts computed the oracle's way, only while the oracle has no readings of its own
    /// for the event yet (up to an hour after it is created).
    pub early_forecasts: Vec<Reading>,
}

impl CompetitionWeather {
    /// The forecast the oracle compares `metric` at `station` with.
    pub fn forecast(&self, station: &str, metric: Metric) -> Option<f64> {
        // Once present, the oracle's reading is authoritative, including a missing baseline.
        // Replacing a missing final baseline with an early forecast would invent scored hits.
        if let Some(reading) = self.event.reading(station, metric.id()) {
            return reading.baseline;
        }
        self.early_forecasts
            .iter()
            .find(|reading| reading.target == station && reading.metric == metric.id())
            .and_then(|reading| reading.baseline)
    }

    /// The value observed so far: the oracle's own reading once it has attested, otherwise the
    /// latest observations, or the oracle's last reading when those are unavailable.
    pub fn observed(&self, station: &str, metric: Metric) -> Option<f64> {
        let oracle = self
            .event
            .reading(station, metric.id())
            .and_then(|reading| reading.observed);
        if self.event.is_final() {
            return oracle;
        }
        match &self.observations {
            Some(observations) => observations
                .iter()
                .find(|observation| observation.station_id == station)
                .and_then(|observation| observation.value(metric.id()))
                .or(oracle),
            None => oracle,
        }
    }

    fn station(&self, station: &str) -> Option<&StationObservations> {
        self.observations
            .as_ref()?
            .iter()
            .find(|observation| observation.station_id == station)
    }
}

/// Where a competition's observation window is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Entries are open; nothing is observed yet.
    Upcoming,
    /// The window is open.
    Live,
    /// The window has closed; the oracle has not attested the result yet.
    Closed,
    /// The oracle has attested the result. Nothing changes any more.
    Scored,
}

#[derive(Debug, Clone, Serialize)]
pub struct Leaderboard {
    pub competition_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub window_start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub window_end: OffsetDateTime,
    pub phase: Phase,
    /// Scores and ranks say "if the window ended now" until the phase is `Scored`.
    pub provisional: bool,
    pub pot: Pot,
    /// In payout order: highest score first, ties by entry id as the oracle breaks them.
    pub rows: Vec<LeaderboardRow>,
    /// When the oracle's weather was fetched; `None` until the first fetch finishes.
    #[serde(with = "time::serde::rfc3339::option")]
    pub weather_fetched_at: Option<OffsetDateTime>,
    /// The latest observation report at any of the competition's stations.
    #[serde(with = "time::serde::rfc3339::option")]
    pub observed_until: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pot {
    pub pool_sats: u64,
    pub entry_fee_sats: u64,
    /// Paid places, and each one's percent of the pool.
    pub places: usize,
    pub place_weights: Vec<u64>,
    /// Nobody scores (or too few entered): every entry is refunded an equal share.
    pub refund_all: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardRow {
    /// The coordinator's entry id.
    pub entry_id: Uuid,
    /// The id the oracle knows the entry by; it breaks ties.
    pub oracle_entry_id: Uuid,
    pub player: Player,
    /// What to show: the oracle's final standing once attested, otherwise the provisional one.
    pub standing: Standing,
    /// "If the window ended now", from the observations so far.
    pub provisional_standing: Standing,
    /// The oracle's, once it has attested the result.
    pub final_standing: Option<Standing>,
    /// 1-based position in payout order.
    pub place: usize,
    /// What this entry is paid at its standing; `None` before the window opens.
    pub prize_sats: Option<u64>,
    /// Empty until the window opens.
    pub picks: Vec<PickProgress>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Player {
    /// Username, or the start of the public key for a player without one.
    pub name: String,
    /// Nostr public key, hex.
    pub pubkey: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PickProgress {
    pub station_id: String,
    pub metric: Metric,
    /// "°F" or "kt".
    pub unit: &'static str,
    pub pick: ValueOptions,
    /// What the oracle compares with; fixed once the window opens.
    pub forecast: Option<f64>,
    /// Observed over the window so far: the running high, low or peak wind.
    pub observed: Option<f64>,
    pub state: PickState,
    /// Points if the window ended now; the oracle's once the phase is `Scored`.
    pub points: u64,
    pub hit: bool,
    /// Hours of the window the observations cover so far, and the window's length.
    pub hours_covered: f64,
    pub hours_total: f64,
    /// When the station last reported inside the window.
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
}

/// Builds leaderboards, and serves the oracle's weather to pages without making them wait.
pub struct Leaderboards {
    coordinator: Arc<Coordinator>,
    users: Arc<UserInfo>,
    oracle: OracleWeather,
    weather: Arc<RefreshCache<Uuid, CompetitionWeather>>,
    stations: Arc<RefreshCache<(), Vec<Station>>>,
    /// Oracle weather requests in flight at once.
    oracle_requests: Arc<Semaphore>,
}

type WeatherFetch =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<CompetitionWeather>> + Send>>;

impl Leaderboards {
    pub fn new(
        coordinator: Arc<Coordinator>,
        users: Arc<UserInfo>,
        oracle_url: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            coordinator,
            users,
            oracle: OracleWeather::new(oracle_url)?,
            weather: Arc::new(RefreshCache::new()),
            stations: Arc::new(RefreshCache::new()),
            oracle_requests: Arc::new(Semaphore::new(ORACLE_REQUESTS)),
        })
    }

    /// The leaderboard of `competition_id`, as of the latest weather fetched.
    pub async fn get(&self, competition_id: Uuid) -> Result<Leaderboard, Error> {
        let competition = self.coordinator.get_competition(competition_id).await?;
        let (entries, weather) = tokio::join!(
            self.coordinator
                .competition_store
                .get_competition_entries(competition_id, vec![EntryStatus::Paid]),
            self.weather(&competition),
        );
        let entries = entries?;
        let names = self.player_names(&entries).await;
        Ok(build(
            &competition,
            &entries,
            weather.as_deref(),
            &names,
            OffsetDateTime::now_utc(),
        ))
    }

    /// The oracle's weather for `competition`, possibly a few minutes old. `None` only while the
    /// first fetch is still running, or the oracle has not answered at all.
    pub async fn weather(
        &self,
        competition: &Competition,
    ) -> Option<Arc<Fetched<CompetitionWeather>>> {
        let (ttl, fetch) = self.weather_fetch(competition);
        self.weather
            .get(competition.id, ttl, FIRST_READ_WAIT, fetch)
            .await
    }

    /// Start fetching `competition`'s weather now, if it is due, without waiting for it.
    pub fn warm(&self, competition: &Competition) {
        let (ttl, fetch) = self.weather_fetch(competition);
        self.weather.refresh(competition.id, ttl, fetch);
    }

    /// Every station the oracle knows, refreshed hourly.
    pub async fn stations(&self) -> Option<Arc<Fetched<Vec<Station>>>> {
        let oracle = self.oracle.clone();
        self.stations
            .get((), STATIONS_TTL, FIRST_READ_WAIT, move || async move {
                oracle.stations().await
            })
            .await
    }

    /// How long `competition`'s cached weather stays fresh, and how to fetch it again.
    fn weather_fetch(
        &self,
        competition: &Competition,
    ) -> (Duration, impl FnOnce() -> WeatherFetch + Send + 'static) {
        let now = OffsetDateTime::now_utc();
        let start = competition.event_submission.start_observation_date;
        let end = competition.event_submission.end_observation_date;
        let ttl = match self.weather.peek(&competition.id) {
            Some(cached) if cached.value.event.is_final() => SCORED_TTL,
            _ if now < start => UPCOMING_TTL,
            _ => LIVE_TTL,
        };
        let oracle = self.oracle.clone();
        let requests = Arc::clone(&self.oracle_requests);
        let cache = Arc::clone(&self.weather);
        let id = competition.id;
        let stations = competition.event_submission.locations.clone();
        let fetch = move || -> WeatherFetch {
            Box::pin(async move {
                // The oracle answers these from DuckDB; leave it room for its own pages.
                let _permit = requests.acquire_owned().await?;
                let now = OffsetDateTime::now_utc();
                let observed = async {
                    if now < start {
                        return Some(vec![]);
                    }
                    // The oracle scores observations from the start to just before the end.
                    match oracle
                        .observations(&stations, start, end - time::Duration::NANOSECOND)
                        .await
                    {
                        Ok(observations) => Some(observations),
                        Err(e) => {
                            warn!("Observations for competition {id}: {e:#}");
                            None
                        }
                    }
                };
                let (event, observations) = tokio::join!(oracle.event_readings(id), observed);
                let event = event?;
                // A successful event read must not erase more recent weather when only the
                // observations endpoint fails. Its report time continues to expose its age.
                let observations = observations.or_else(|| {
                    cache
                        .peek(&id)
                        .and_then(|cached| cached.value.observations.clone())
                });
                // A new event has no readings until the oracle's next hourly run. Compute its
                // forecasts once meanwhile; the query takes seconds, so keep the result.
                let early_forecasts = if !event.readings.is_empty() {
                    vec![]
                } else if let Some(cached) = cache
                    .peek(&id)
                    .filter(|cached| !cached.value.early_forecasts.is_empty())
                {
                    cached.value.early_forecasts.clone()
                } else {
                    oracle
                        .forecast_baselines(&stations, start, end)
                        .await
                        .unwrap_or_else(|e| {
                            warn!("Forecasts for competition {id}: {e:#}");
                            vec![]
                        })
                };
                Ok::<_, anyhow::Error>(CompetitionWeather {
                    event,
                    observations,
                    early_forecasts,
                })
            })
        };
        (ttl, fetch)
    }

    async fn player_names(&self, entries: &[UserEntry]) -> HashMap<String, String> {
        let mut names = HashMap::new();
        for entry in entries {
            if names.contains_key(&entry.pubkey) {
                continue;
            }
            let username = match nostr::PublicKey::from_hex(&entry.pubkey) {
                Ok(pubkey) => {
                    let npub = pubkey.to_bech32().unwrap_or_else(|never| match never {});
                    self.users
                        .get_username_by_pubkey(&npub)
                        .await
                        .ok()
                        .flatten()
                }
                Err(_) => None,
            };
            let name = username.unwrap_or_else(|| entry.pubkey.chars().take(8).collect());
            names.insert(entry.pubkey.clone(), name);
        }
        names
    }

    /// Keep open competitions' weather fresh ahead of any page, until `cancel`.
    pub fn spawn_refresher(self: &Arc<Self>, tracker: &TaskTracker, cancel: CancellationToken) {
        let leaderboards = Arc::clone(self);
        tracker.spawn(async move {
            let mut interval = tokio::time::interval(WARM_EVERY);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => leaderboards.refresh_open_competitions().await,
                }
            }
        });
    }

    async fn refresh_open_competitions(&self) {
        let oracle = self.oracle.clone();
        self.stations.refresh(
            (),
            STATIONS_TTL,
            move || async move { oracle.stations().await },
        );
        let competitions = match self.coordinator.get_competitions().await {
            Ok(competitions) => competitions,
            Err(e) => {
                debug!("Cannot list competitions to refresh their weather: {e}");
                return;
            }
        };
        let now = OffsetDateTime::now_utc();
        for competition in competitions {
            // Upcoming ones too: the entry form shows their forecasts.
            let current = now < competition.event_submission.end_observation_date + WARM_AFTER_END;
            let finished = competition.cancelled_at.is_some()
                || competition.failed_at.is_some()
                || competition.attestation.is_some();
            if !current || finished {
                continue;
            }
            self.warm(&competition);
        }
    }
}

/// The leaderboard from what is stored and what the oracle said. Pure, for tests.
pub fn build(
    competition: &Competition,
    entries: &[UserEntry],
    weather: Option<&Fetched<CompetitionWeather>>,
    names: &HashMap<String, String>,
    now: OffsetDateTime,
) -> Leaderboard {
    let start = competition.event_submission.start_observation_date;
    let end = competition.event_submission.end_observation_date;
    let hours_total = (end - start).as_seconds_f64() / 3600.0;
    let scored = weather.is_some_and(|weather| weather.value.event.is_final());
    let phase = if scored {
        Phase::Scored
    } else if now < start {
        Phase::Upcoming
    } else if now < end {
        Phase::Live
    } else {
        Phase::Closed
    };
    let weather_value = weather.map(|weather| &weather.value);

    let mut entries: Vec<&UserEntry> = entries.iter().collect();
    // Contract players are in ticket order, which decides who gets a refund's remainder.
    entries.sort_by_key(|entry| entry.ticket_id);
    let ticket_order: Vec<usize> = (0..entries.len()).collect();

    let picks: Vec<Vec<PickProgress>> = entries
        .iter()
        .map(|entry| {
            if phase == Phase::Upcoming {
                return vec![];
            }
            entry
                .entry_submission
                .expected_observations
                .iter()
                .flat_map(|choice| {
                    [
                        (Metric::TempHigh, &choice.temp_high),
                        (Metric::TempLow, &choice.temp_low),
                        (Metric::WindSpeed, &choice.wind_speed),
                    ]
                    .into_iter()
                    .filter_map(|(metric, pick)| Some((metric, pick.as_ref()?)))
                    .map(|(metric, pick)| {
                        pick_progress(
                            &choice.stations,
                            metric,
                            pick,
                            weather_value,
                            phase,
                            start,
                            end,
                            hours_total,
                        )
                    })
                    .collect::<Vec<_>>()
                })
                .collect()
        })
        .collect();

    let provisional_scores: Vec<u64> = picks
        .iter()
        .map(|picks| picks.iter().map(|pick| pick.points).sum())
        .collect();
    let final_scores: Option<Vec<u64>> = weather_value.filter(|_| scored).map(|weather| {
        entries
            .iter()
            .map(|entry| {
                weather
                    .event
                    .scores
                    .get(&entry.entry_submission.id)
                    .copied()
                    .unwrap_or(0)
            })
            .collect()
    });
    let shown_scores = final_scores.as_ref().unwrap_or(&provisional_scores);
    let provisional_standings = progress::standings(&provisional_scores);
    let final_standings = final_scores.as_deref().map(progress::standings);
    let shown_standings = final_standings
        .clone()
        .unwrap_or_else(|| provisional_standings.clone());

    let keyed: Vec<(u64, Uuid)> = entries
        .iter()
        .zip(shown_scores)
        .map(|(entry, &score)| (score, entry.entry_submission.id))
        .collect();
    let places = competition.event_submission.number_of_places_win;
    let pool_sats = competition
        .contract_parameters
        .as_ref()
        .map(|params| params.funding_value.to_sat())
        .unwrap_or(competition.event_submission.total_competition_pool as u64);
    let prizes = progress::prizes(&keyed, &ticket_order, places, pool_sats);
    let refund_all = !entries.is_empty()
        && (entries.len() < places || keyed.iter().all(|(score, _)| *score == 0));

    let mut picks = picks;
    let rows: Vec<LeaderboardRow> = progress::payout_order(&keyed)
        .into_iter()
        .enumerate()
        .map(|(place, index)| {
            let entry = entries[index];
            LeaderboardRow {
                entry_id: entry.id,
                oracle_entry_id: entry.entry_submission.id,
                player: Player {
                    name: names
                        .get(&entry.pubkey)
                        .cloned()
                        .unwrap_or_else(|| entry.pubkey.chars().take(8).collect()),
                    pubkey: entry.pubkey.clone(),
                },
                standing: shown_standings[index],
                provisional_standing: provisional_standings[index],
                final_standing: final_standings.as_ref().map(|standings| standings[index]),
                place: place + 1,
                prize_sats: (phase != Phase::Upcoming).then_some(prizes[index]),
                picks: std::mem::take(&mut picks[index]),
            }
        })
        .collect();

    let observed_until = weather_value
        .and_then(|weather| weather.observations.as_ref())
        .and_then(|observations| {
            observations
                .iter()
                .filter_map(|observation| observation.latest_report)
                .max()
        });
    Leaderboard {
        competition_id: competition.id,
        window_start: start,
        window_end: end,
        phase,
        provisional: phase != Phase::Scored,
        pot: Pot {
            pool_sats,
            entry_fee_sats: competition.event_submission.entry_fee as u64,
            places,
            place_weights: progress::place_weights(places),
            refund_all,
        },
        rows,
        weather_fetched_at: weather.map(|weather| weather.fetched_at),
        observed_until,
    }
}

#[allow(clippy::too_many_arguments)]
fn pick_progress(
    station: &str,
    metric: Metric,
    pick: &ValueOptions,
    weather: Option<&CompetitionWeather>,
    phase: Phase,
    start: OffsetDateTime,
    end: OffsetDateTime,
    hours_total: f64,
) -> PickProgress {
    let forecast = weather.and_then(|weather| weather.forecast(station, metric));
    let observed = weather.and_then(|weather| weather.observed(station, metric));
    let closed = matches!(phase, Phase::Closed | Phase::Scored);
    let points = progress::points(pick, metric, forecast, observed);
    let latest_report = weather
        .and_then(|weather| weather.station(station))
        .and_then(|observations| observations.latest_report)
        .filter(|report| *report >= start);
    let hours_covered = latest_report
        .map(|report| (report.min(end) - start).as_seconds_f64() / 3600.0)
        .unwrap_or(0.0)
        .clamp(0.0, hours_total.max(0.0));
    PickProgress {
        station_id: station.to_owned(),
        metric,
        unit: metric.unit(),
        pick: pick.clone(),
        forecast,
        observed,
        state: progress::pick_state(pick, metric, forecast, observed, closed),
        points,
        hit: points > 0,
        hours_covered,
        hours_total,
        updated_at: latest_report,
    }
}

#[cfg(test)]
mod tests;
