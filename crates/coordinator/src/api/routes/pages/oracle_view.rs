//! What the public pages read from the oracle, cached briefly.
//!
//! The oracle event holds each station's forecast (the "baseline" picks are
//! judged against) and, once recorded, the observed reading. It answers in
//! milliseconds, unlike the oracle's forecast queries, which scan a day of
//! files. The station list changes rarely and is large, so it is kept an hour.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use log::warn;
use serde::Deserialize;
use uuid::Uuid;

use crate::templates::admin::dashboard::Station;

const EVENT_TTL: Duration = Duration::from_secs(15);
const STATIONS_TTL: Duration = Duration::from_secs(60 * 60);

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default()
});

/// One station's forecast and observation for one metric.
#[derive(Debug, Clone, Deserialize)]
pub struct Reading {
    pub target: String,
    pub metric: String,
    pub baseline: Option<f64>,
    pub observed: Option<f64>,
}

/// The parts of an oracle event the pages show.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OracleEventView {
    #[serde(default)]
    pub readings: Vec<Reading>,
}

impl OracleEventView {
    pub fn reading(&self, station: &str, metric: &str) -> Option<&Reading> {
        self.readings
            .iter()
            .find(|reading| reading.target == station && reading.metric == metric)
    }
}

type Cache<K, V> = LazyLock<Mutex<HashMap<K, (Instant, Arc<V>)>>>;

static EVENTS: Cache<Uuid, OracleEventView> = LazyLock::new(Default::default);
static STATIONS: Cache<String, HashMap<String, Station>> = LazyLock::new(Default::default);

fn cached<K: std::hash::Hash + Eq, V>(cache: &Cache<K, V>, key: &K, ttl: Duration) -> Option<Arc<V>> {
    let cache = cache.lock().ok()?;
    let (fetched, value) = cache.get(key)?;
    (fetched.elapsed() < ttl).then(|| value.clone())
}

fn store<K: std::hash::Hash + Eq, V>(cache: &Cache<K, V>, key: K, value: Arc<V>) {
    if let Ok(mut cache) = cache.lock() {
        // Old competitions fall out instead of accumulating.
        cache.retain(|_, (fetched, _)| fetched.elapsed() < STATIONS_TTL);
        cache.insert(key, (Instant::now(), value));
    }
}

/// The oracle's event for a competition; empty when the oracle is unreachable.
pub async fn event(oracle_url: &str, competition_id: Uuid) -> Arc<OracleEventView> {
    if let Some(event) = cached(&EVENTS, &competition_id, EVENT_TTL) {
        return event;
    }
    let url = format!(
        "{}/oracle/events/{competition_id}",
        oracle_url.trim_end_matches('/')
    );
    let fetched = async {
        CLIENT
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json::<OracleEventView>()
            .await
    }
    .await;
    match fetched {
        Ok(event) => {
            let event = Arc::new(event);
            store(&EVENTS, competition_id, event.clone());
            event
        }
        Err(error) => {
            warn!("oracle event {competition_id} unavailable: {error}");
            Arc::new(OracleEventView::default())
        }
    }
}

/// The oracle's stations by ID; empty when the oracle is unreachable.
pub async fn stations(oracle_url: &str) -> Arc<HashMap<String, Station>> {
    let key = oracle_url.to_owned();
    if let Some(stations) = cached(&STATIONS, &key, STATIONS_TTL) {
        return stations;
    }
    let url = format!("{}/stations", oracle_url.trim_end_matches('/'));
    let fetched = async {
        CLIENT
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Station>>()
            .await
    }
    .await;
    match fetched {
        Ok(list) => {
            let stations: Arc<HashMap<_, _>> = Arc::new(
                list.into_iter()
                    .map(|station| (station.station_id.clone(), station))
                    .collect(),
            );
            store(&STATIONS, key, stations.clone());
            stations
        }
        Err(error) => {
            warn!("oracle stations unavailable: {error}");
            Arc::new(HashMap::new())
        }
    }
}

/// A station's name as players know it: `Portland International, ME`.
pub fn station_name(stations: &HashMap<String, Station>, station_id: &str) -> Option<String> {
    let station = stations.get(station_id)?;
    let name = station
        .station_name
        .split_whitespace()
        .map(|word| match word {
            "Intl" => "International",
            "Rgnl" => "Regional",
            "Muni" => "Municipal",
            "Fld" => "Field",
            "Arpt" => "Airport",
            word => word,
        })
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        return None;
    }
    Some(match station.state.trim() {
        "" => name,
        state => format!("{name}, {state}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_names_are_spelled_out() {
        let station = Station {
            station_id: "KPWM".into(),
            station_name: "Portland Intl".into(),
            state: "ME".into(),
            iata_id: "PWM".into(),
            elevation_m: None,
            latitude: 43.6,
            longitude: -70.3,
        };
        let stations = HashMap::from([("KPWM".to_owned(), station)]);
        assert_eq!(
            station_name(&stations, "KPWM").as_deref(),
            Some("Portland International, ME")
        );
        assert_eq!(station_name(&stations, "KXXX"), None);
    }
}
