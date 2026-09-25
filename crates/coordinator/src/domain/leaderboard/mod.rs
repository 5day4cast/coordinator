//! A competition's leaderboard: every paid entry, ranked, with each pick's progress.
//!
//! [`Leaderboards::get`] is the one call a page needs. It returns [`Leaderboard`], whose rows are
//! in payout order and carry everything to render:
//!
//! - the standing to show (rank, shared on ties), the full entry id, and the player;
//! - per pick: the forecast, the value observed so far, how many hours of the window that covers,
//!   the points it would score now, and a [`PickState`];
//! - `provisional` until the oracle has attested the result. Provisional scores and ranks say
//!   "if the window ended now"; the final ones are the oracle's attested scores, unchanged.
//!
//! Picks are withheld until the observation window opens, when entries have closed.
//!
//! The oracle's weather is read through a cache that refreshes in the background, a few minutes
//! apart while a window is open, so a page never waits on the oracle for long: a read that finds
//! nothing cached waits for the first fetch only as long as its caller allows, then shows what
//! it has, with [`Leaderboard::refreshing`] saying whether the weather is still on its way.

mod phase;
pub mod progress;

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures::FutureExt;
use log::{debug, error, warn};
use nostr::ToBech32;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

pub use phase::Phase;
pub use progress::{Metric, PickState, Standing};

use crate::{
    domain::{Competition, Coordinator, EntryStatus, Error, UserEntry, UserInfo},
    infra::{
        oracle::ValueOptions,
        oracle_weather::{EventReadings, OracleWeather, Reading, Station, StationObservations},
        refresh_cache::{Cached, RefreshCache, RETRY_AFTER},
    },
};

/// How long a page's own render waits for a competition's first weather fetch.
pub const FIRST_READ_WAIT: Duration = Duration::from_millis(250);
/// How long a fragment that loads after its page may wait for a first weather fetch. Cold
/// fetches can queue behind others for the oracle, so this is a few seconds.
pub const LOAD_WAIT: Duration = Duration::from_secs(3);
/// Refresh interval while a window is open or awaiting its score. Stations report hourly.
const LIVE_TTL: Duration = Duration::from_secs(3 * 60);
/// Before a window opens only the forecast changes, about hourly.
const UPCOMING_TTL: Duration = Duration::from_secs(10 * 60);
/// An attested competition's weather is final.
const SCORED_TTL: Duration = Duration::from_secs(60 * 60);
const STATIONS_TTL: Duration = Duration::from_secs(60 * 60);
/// How long forecasts computed ahead of the oracle are kept. New forecasts are issued through
/// the day, and the oracle computes its own readings for an event within the hour.
const EARLY_FORECASTS_TTL: time::Duration = time::Duration::HOUR;
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
    /// Forecasts computed the oracle's way while it has no readings of its own for the event:
    /// until the coordinator creates the oracle's event, and up to an hour after. `None` when
    /// the oracle has readings, or when computing them has failed so far.
    early_forecasts: Option<EarlyForecasts>,
}

#[derive(Debug, Clone)]
struct EarlyForecasts {
    readings: Vec<Reading>,
    computed_at: OffsetDateTime,
}

impl EarlyForecasts {
    /// Whether to compute them again. Before the window opens they change as forecasts are
    /// issued; once it opens the oracle's baseline stops changing, so they are computed once
    /// more to catch the forecasts issued since.
    fn due(&self, start: OffsetDateTime, now: OffsetDateTime) -> bool {
        (self.computed_at < start && now >= start) || now - self.computed_at >= EARLY_FORECASTS_TTL
    }
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
            .as_ref()?
            .readings
            .iter()
            .find(|reading| reading.target == station && reading.metric == metric.id())
            .and_then(|reading| reading.baseline)
    }

    /// Whether the forecasts are missing because computing them failed, rather than because
    /// none have been issued. They are fetched again within [`RETRY_AFTER`].
    pub fn forecasts_unavailable(&self) -> bool {
        self.event.readings.is_empty() && self.early_forecasts.is_none()
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

#[derive(Debug, Clone)]
pub struct Leaderboard {
    /// The competition's [`Phase::of`], except that it is `Scored` exactly when the rows hold
    /// the oracle's attested scores: a closed window counts as `Scored` as soon as the cached
    /// result is final, and as `AwaitingResult` until the cache has caught up with an
    /// attestation the coordinator already has.
    pub phase: Phase,
    /// Scores and ranks say "if the window ended now" until the oracle's attested result is in.
    pub provisional: bool,
    /// In payout order: highest score first, ties by entry id as the oracle breaks them.
    pub rows: Vec<LeaderboardRow>,
    /// When the oracle's weather was fetched; `None` until a first fetch succeeds.
    pub weather_fetched_at: Option<OffsetDateTime>,
    /// A weather fetch is running. Without weather yet, it is still loading; without weather
    /// and not refreshing, the last fetch failed and the next one is at most [`RETRY_AFTER`]
    /// away.
    pub refreshing: bool,
    /// The latest observation report at any of the competition's stations.
    pub observed_until: Option<OffsetDateTime>,
}

#[derive(Debug, Clone)]
pub struct LeaderboardRow {
    /// The coordinator's entry id.
    pub entry_id: Uuid,
    pub player: Player,
    /// The oracle's final standing once attested, otherwise "if the window ended now".
    pub standing: Standing,
    /// Empty until the window opens.
    pub picks: Vec<PickProgress>,
}

#[derive(Debug, Clone)]
pub struct Player {
    /// Username, or the start of the public key for a player without one.
    pub name: String,
    /// Nostr public key, hex.
    pub pubkey: String,
}

#[derive(Debug, Clone)]
pub struct PickProgress {
    pub station_id: String,
    pub metric: Metric,
    pub pick: ValueOptions,
    /// What the oracle compares with; fixed once the window opens.
    pub forecast: Option<f64>,
    /// Observed over the window so far: the running high, low or peak wind.
    pub observed: Option<f64>,
    pub state: PickState,
    /// Points if the window ended now; the oracle's once the result is final.
    pub points: u64,
    pub hit: bool,
    /// Hours of the window the observations cover so far, and the window's length.
    pub hours_covered: f64,
    pub hours_total: f64,
}

/// Builds leaderboards, and serves the oracle's weather to pages without making them wait.
pub struct Leaderboards {
    coordinator: Arc<Coordinator>,
    users: Arc<UserInfo>,
    weather: WeatherCache,
}

impl Leaderboards {
    pub fn new(
        coordinator: Arc<Coordinator>,
        users: Arc<UserInfo>,
        oracle_url: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            coordinator,
            users,
            weather: WeatherCache::new(oracle_url)?,
        })
    }

    /// The leaderboard of `competition`, as of the latest weather fetched. With no weather
    /// cached yet, this waits up to `wait` for the first fetch.
    pub async fn get(
        &self,
        competition: &Competition,
        wait: Duration,
    ) -> Result<Leaderboard, Error> {
        let (entries, weather) = tokio::join!(
            self.coordinator
                .competition_store
                .get_competition_entries(competition.id, vec![EntryStatus::Paid]),
            self.weather.weather(competition, wait),
        );
        let entries = entries?;
        let names = self.player_names(&entries).await;
        Ok(build(
            competition,
            &entries,
            &weather,
            &names,
            OffsetDateTime::now_utc(),
        ))
    }

    /// The oracle's weather for `competition`, possibly a few minutes old. With nothing cached
    /// yet, or only weather whose forecasts failed and are being fetched again, this waits up
    /// to `wait` for that fetch.
    pub async fn weather(
        &self,
        competition: &Competition,
        wait: Duration,
    ) -> Cached<CompetitionWeather> {
        self.weather.weather(competition, wait).await
    }

    /// Start fetching `competition`'s weather now, if it is due, without waiting for it.
    pub fn warm(&self, competition: &Competition) {
        self.weather.warm(competition);
    }

    /// Every station the oracle knows, refreshed hourly. With nothing cached yet, this waits up
    /// to `wait` for the first fetch.
    pub async fn stations(&self, wait: Duration) -> Cached<Vec<Station>> {
        self.weather.stations(wait).await
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
                    _ = interval.tick() => {
                        // Pages still refresh what they read; don't let one bad tick stop this.
                        let tick = std::panic::AssertUnwindSafe(
                            leaderboards.refresh_open_competitions(),
                        );
                        if tick.catch_unwind().await.is_err() {
                            error!("Refreshing competitions' weather panicked");
                        }
                    }
                }
            }
        });
    }

    async fn refresh_open_competitions(&self) {
        self.weather.warm_stations();
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
            let current = match Phase::of(&competition, now) {
                Phase::Upcoming | Phase::Live => true,
                // Scored too, so the attested result reaches the cache without a page asking.
                // Once it has, it is refreshed hourly at most.
                Phase::AwaitingResult | Phase::Scored => {
                    now < competition.event_submission.end_observation_date + WARM_AFTER_END
                }
                _ => false,
            };
            if current {
                self.warm(&competition);
            }
        }
    }
}

/// The oracle's weather for each competition, and its stations, refreshed in the background.
struct WeatherCache {
    oracle: OracleWeather,
    weather: Arc<RefreshCache<Uuid, CompetitionWeather>>,
    stations: Arc<RefreshCache<(), Vec<Station>>>,
    /// Oracle weather requests in flight at once.
    oracle_requests: Arc<Semaphore>,
}

type WeatherFetch =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<CompetitionWeather>> + Send>>;

impl WeatherCache {
    fn new(oracle_url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            oracle: OracleWeather::new(oracle_url)?,
            weather: Arc::new(RefreshCache::new()),
            stations: Arc::new(RefreshCache::new()),
            oracle_requests: Arc::new(Semaphore::new(ORACLE_REQUESTS)),
        })
    }

    /// Weather whose forecasts failed is refreshed within [`RETRY_AFTER`], and a reader that
    /// finds only that waits for the retry like a reader that finds nothing, so a page's retry
    /// can show the forecasts it fetched.
    async fn weather(
        &self,
        competition: &Competition,
        wait: Duration,
    ) -> Cached<CompetitionWeather> {
        let (ttl, fetch) = self.weather_fetch(competition);
        self.weather
            .get_usable(
                competition.id,
                ttl,
                wait,
                |weather| !weather.forecasts_unavailable(),
                fetch,
            )
            .await
    }

    fn warm(&self, competition: &Competition) {
        let (ttl, fetch) = self.weather_fetch(competition);
        self.weather.refresh(competition.id, ttl, fetch);
    }

    async fn stations(&self, wait: Duration) -> Cached<Vec<Station>> {
        let oracle = self.oracle.clone();
        self.stations
            .get((), STATIONS_TTL, wait, move || async move {
                oracle.stations().await
            })
            .await
    }

    fn warm_stations(&self) {
        let oracle = self.oracle.clone();
        self.stations.refresh(
            (),
            STATIONS_TTL,
            move || async move { oracle.stations().await },
        );
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
            // A failure is not data: try again soon rather than show "unavailable" for long.
            Some(cached) if cached.value.forecasts_unavailable() => RETRY_AFTER,
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
                let cached = cache.peek(&id);
                // A successful event read must not erase more recent weather when only the
                // observations endpoint fails. Its report time continues to expose its age.
                let observations = observations.or_else(|| {
                    cached
                        .as_ref()
                        .and_then(|cached| cached.value.observations.clone())
                });
                // Until the oracle stores readings for the event, compute its forecasts the way
                // it will. The query takes seconds, so keep the result a while, empty or not.
                let early_forecasts = if !event.readings.is_empty() {
                    None
                } else {
                    let kept = cached.and_then(|cached| cached.value.early_forecasts.clone());
                    match kept {
                        Some(kept) if !kept.due(start, now) => Some(kept),
                        kept => match oracle.forecast_baselines(&stations, start, end).await {
                            Ok(readings) => Some(EarlyForecasts {
                                readings,
                                computed_at: now,
                            }),
                            Err(e) => {
                                warn!("Forecasts for competition {id}: {e:#}");
                                kept
                            }
                        },
                    }
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
}

/// The leaderboard from what is stored and what the oracle said. Pure, for tests.
pub fn build(
    competition: &Competition,
    entries: &[UserEntry],
    weather: &Cached<CompetitionWeather>,
    names: &HashMap<String, String>,
    now: OffsetDateTime,
) -> Leaderboard {
    let start = competition.event_submission.start_observation_date;
    let end = competition.event_submission.end_observation_date;
    let hours_total = (end - start).as_seconds_f64() / 3600.0;
    let weather_value = weather.value();
    let scored = weather_value.is_some_and(|weather| weather.event.is_final());
    // The phase says what the rows show: Scored only with the oracle's attested scores.
    let phase = match Phase::of(competition, now) {
        Phase::AwaitingResult if scored => Phase::Scored,
        // The coordinator has the attestation, but the cached weather predates it.
        Phase::Scored if !scored => Phase::AwaitingResult,
        phase => phase,
    };
    let window = if scored {
        Window::Scored
    } else if now >= end {
        Window::Closed
    } else {
        Window::Open
    };

    let mut entries: Vec<&UserEntry> = entries.iter().collect();
    entries.sort_by_key(|entry| entry.entry_submission.id);

    let mut picks: Vec<Vec<PickProgress>> = entries
        .iter()
        .map(|entry| {
            if now < start {
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
                            window,
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

    let scores: Vec<u64> = match weather_value.filter(|_| scored) {
        Some(weather) => entries
            .iter()
            .map(|entry| {
                weather
                    .event
                    .scores
                    .get(&entry.entry_submission.id)
                    .copied()
                    .unwrap_or(0)
            })
            .collect(),
        None => picks
            .iter()
            .map(|picks| picks.iter().map(|pick| pick.points).sum())
            .collect(),
    };
    let standings = progress::standings(&scores);
    let keyed: Vec<(u64, Uuid)> = entries
        .iter()
        .zip(&scores)
        .map(|(entry, &score)| (score, entry.entry_submission.id))
        .collect();

    let rows: Vec<LeaderboardRow> = progress::payout_order(&keyed)
        .into_iter()
        .map(|index| {
            let entry = entries[index];
            LeaderboardRow {
                entry_id: entry.id,
                player: Player {
                    name: names
                        .get(&entry.pubkey)
                        .cloned()
                        .unwrap_or_else(|| entry.pubkey.chars().take(8).collect()),
                    pubkey: entry.pubkey.clone(),
                },
                standing: standings[index],
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
        phase,
        provisional: !scored,
        rows,
        weather_fetched_at: weather.latest.as_ref().map(|weather| weather.fetched_at),
        refreshing: weather.refreshing,
        observed_until,
    }
}

/// Where the observation window is, for a pick's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    Open,
    /// Closed, and the oracle has not attested the result yet.
    Closed,
    /// The oracle has attested the result.
    Scored,
}

#[allow(clippy::too_many_arguments)]
fn pick_progress(
    station: &str,
    metric: Metric,
    pick: &ValueOptions,
    weather: Option<&CompetitionWeather>,
    window: Window,
    start: OffsetDateTime,
    end: OffsetDateTime,
    hours_total: f64,
) -> PickProgress {
    let forecast = weather.and_then(|weather| weather.forecast(station, metric));
    let observed = weather.and_then(|weather| weather.observed(station, metric));
    let points = progress::points(pick, metric, forecast, observed);
    let state = match window {
        Window::Scored => PickState::Final,
        Window::Closed => PickState::AwaitingResult,
        Window::Open => progress::pick_state(pick, metric, forecast, observed),
    };
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
        pick: pick.clone(),
        forecast,
        observed,
        state,
        points,
        hit: points > 0,
        hours_covered,
        hours_total,
    }
}

#[cfg(test)]
mod tests;
