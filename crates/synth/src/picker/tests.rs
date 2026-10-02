//! The picker against a fake oracle and a real database.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use time::macros::datetime;

use super::fixtures::{forecast, station};
use super::oracle::{Forecast, StationInfo, FORECAST_BATCH};
use super::*;

const DENVER: (f64, f64) = (39.86, -104.67);
const COLORADO_SPRINGS: (f64, f64) = (38.81, -104.70);
const CHEYENNE: (f64, f64) = (41.16, -104.81);
const CHICAGO: (f64, f64) = (41.98, -87.90);
const NEW_YORK: (f64, f64) = (40.64, -73.78);
const SEATTLE: (f64, f64) = (47.45, -122.31);

/// What the fake oracle answers, and what it was asked.
#[derive(Default)]
struct Oracle {
    /// None answers 404, as an oracle without the list does.
    eligible: Option<Vec<StationInfo>>,
    stations: Vec<StationInfo>,
    forecasts: Vec<Forecast>,
    eligible_calls: usize,
    /// The station ids of each forecast request.
    forecast_batches: Vec<Vec<String>>,
}

type Shared = Arc<StdMutex<Oracle>>;

fn json(station: &StationInfo) -> serde_json::Value {
    serde_json::json!({
        "station_id": station.station_id,
        "station_name": station.station_name,
        "state": station.state,
        "iata_id": station.iata_id.clone().unwrap_or_default(),
        "latitude": station.latitude,
        "longitude": station.longitude,
        "clean_days": 30,
        "days_checked": 30,
        "last_report": "2026-10-01T00:00:00Z",
        "forecast_through": "2026-10-08T00:00:00Z",
    })
}

async fn eligible(State(oracle): State<Shared>) -> Response {
    let mut oracle = oracle.lock().unwrap();
    oracle.eligible_calls += 1;
    match &oracle.eligible {
        Some(stations) => Json(stations.iter().map(json).collect::<Vec<_>>()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn stations(State(oracle): State<Shared>) -> Response {
    let oracle = oracle.lock().unwrap();
    Json(oracle.stations.iter().map(json).collect::<Vec<_>>()).into_response()
}

async fn forecasts(
    State(oracle): State<Shared>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut oracle = oracle.lock().unwrap();
    assert!(query.contains_key("start") && query.contains_key("end"));
    let ids: Vec<String> = query["station_ids"]
        .split(',')
        .map(str::to_string)
        .collect();
    let rows: Vec<serde_json::Value> = oracle
        .forecasts
        .iter()
        .filter(|row| ids.contains(&row.station_id))
        .map(|row| {
            serde_json::json!({
                "station_id": row.station_id,
                "date": "2026-10-02",
                "start_time": "2026-10-02T00:00:00Z",
                "end_time": "2026-10-02T06:00:00Z",
                "temp_low": row.temp_low,
                "temp_high": row.temp_high,
                "wind_speed": row.wind_speed,
                "wind_direction": 270,
                "humidity_min": 30,
                "humidity_max": 80,
                "temp_unit_code": "F",
                "precip_chance": row.precip_chance,
                "rain_amt": row.rain_amt,
                "snow_amt": row.snow_amt,
                "ice_amt": row.ice_amt,
            })
        })
        .collect();
    oracle.forecast_batches.push(ids);
    Json(rows).into_response()
}

struct Fixture {
    oracle: Shared,
    picker: Picker,
    db: SynthDb,
    server: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(oracle: Oracle) -> Fixture {
    let oracle = Arc::new(StdMutex::new(oracle));
    let app = Router::new()
        .route("/stations", get(stations))
        .route("/stations/eligible", get(eligible))
        .route("/stations/forecasts", get(forecasts))
        .with_state(oracle.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
        .await
        .unwrap();
    Fixture {
        oracle,
        picker: Picker::new(&url, db.clone()),
        db,
        server,
        _directory: directory,
    }
}

fn lane(toml: &str) -> LaneConfig {
    let lane: LaneConfig = toml::from_str(&format!(
        r#"
name = "open"
scenarios = ["full_lifecycle"]
observation_windows_secs = [86400]
stations = ["KDEN", "KJFK", "KORD", "KSEA"]
stations_per_run = 3
{toml}
"#
    ))
    .unwrap();
    lane.validate(&ScenarioConfig::default()).unwrap();
    lane
}

fn run(lane: &LaneConfig) -> ScenarioConfig {
    let base = ScenarioConfig::default();
    lane.run_config(&base, 0, Some(datetime!(2026-10-02 00:00 UTC)))
        .1
}

/// Denver is the stormiest, with Colorado Springs and Cheyenne near it; the cities elsewhere
/// are calm.
fn weather() -> (Vec<StationInfo>, Vec<Forecast>) {
    let stations = vec![
        station("KDEN", "DEN", DENVER),
        station("KCOS", "COS", COLORADO_SPRINGS),
        station("KCYS", "CYS", CHEYENNE),
        station("KORD", "ORD", CHICAGO),
        station("KJFK", "JFK", NEW_YORK),
        station("KSEA", "SEA", SEATTLE),
    ];
    let forecasts = vec![
        forecast("KDEN", 25, 60, 30, 80),
        forecast("KCOS", 30, 55, 25, 60),
        forecast("KCYS", 28, 50, 28, 50),
        forecast("KORD", 50, 70, 15, 40),
        forecast("KJFK", 60, 65, 10, 10),
        forecast("KSEA", 52, 58, 5, 30),
    ];
    (stations, forecasts)
}

#[tokio::test]
async fn a_lane_picks_the_storm_and_its_neighbours_from_the_eligible_stations() {
    let (stations, forecasts) = weather();
    let picking = fixture(Oracle {
        eligible: Some(stations),
        forecasts,
        ..Default::default()
    })
    .await;
    let lane = lane("[picker]\nmode = \"weather\"\ncluster_km = 600\n");
    let mut config = run(&lane);
    let pick = picking
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .unwrap();
    // Neither is on the lane's own list: the eligible stations are the candidates.
    assert_eq!(config.stations, ["KDEN", "KCOS", "KCYS"]);
    assert_eq!(pick.picked[0].role, Role::Leader);
    assert_eq!(pick.window_start, datetime!(2026-10-02 00:00 UTC));
    assert_eq!(pick.window_end, datetime!(2026-10-03 00:00 UTC));
    assert_eq!((pick.candidates, pick.scored), (6, 6));
    assert!(
        pick.explanation.starts_with("KDEN swing 35 °F"),
        "{}",
        pick.explanation
    );
    assert!(
        pick.explanation.contains("KCOS 117 km away"),
        "{}",
        pick.explanation
    );
    // The pick is saved with the competition, whose id it chose, for the run's page.
    let id = config.competition_id.expect("an id for the competition");
    assert_eq!(store::pick_for(&picking.db, id).await.unwrap(), pick);
    let page = view::section(&pick).into_string();
    assert!(page.contains("leader: the most weather"), "{page}");
}

#[tokio::test]
async fn the_last_runs_stations_are_not_picked_again() {
    let (stations, forecasts) = weather();
    let picking = fixture(Oracle {
        eligible: Some(stations),
        forecasts,
        ..Default::default()
    })
    .await;
    let lane = lane("[picker]\nmode = \"weather\"\nrecent_runs_to_avoid = 1\n");
    let base = ScenarioConfig::default();
    let mut first = run(&lane);
    picking
        .picker
        .choose(&lane, &base, &mut first)
        .await
        .unwrap();
    // Independent picks: Denver, then the best 300 km or more away, Chicago and Seattle.
    assert_eq!(first.stations, ["KDEN", "KORD", "KSEA"]);
    let mut second = run(&lane);
    let pick = picking
        .picker
        .choose(&lane, &base, &mut second)
        .await
        .unwrap();
    assert_eq!(pick.avoided.len(), 3);
    assert!(second
        .stations
        .iter()
        .all(|id| !first.stations.contains(id)));
    assert_eq!(second.stations[0], "KCOS");
    // Only the last run is avoided: the third may use the first's again.
    let mut third = run(&lane);
    picking
        .picker
        .choose(&lane, &base, &mut third)
        .await
        .unwrap();
    assert!(third
        .stations
        .iter()
        .all(|id| !second.stations.contains(id)));
    assert_eq!(third.stations[0], "KDEN");
    // One eligible call is enough for runs ten minutes apart.
    assert_eq!(picking.oracle.lock().unwrap().eligible_calls, 1);
}

#[tokio::test]
async fn without_the_eligible_list_the_lanes_own_stations_are_ranked() {
    let (stations, forecasts) = weather();
    let picking = fixture(Oracle {
        eligible: None,
        stations,
        forecasts,
        ..Default::default()
    })
    .await;
    let lane = lane("[picker]\nmode = \"weather\"\n");
    let base = ScenarioConfig::default();
    let mut config = run(&lane);
    let pick = picking
        .picker
        .choose(&lane, &base, &mut config)
        .await
        .unwrap();
    assert_eq!(
        pick.source,
        "the lane's stations, for want of the oracle's eligible list"
    );
    assert_eq!(pick.candidates, 4);
    // Placed from the oracle's station list, so the spread rule holds.
    assert_eq!(config.stations, ["KDEN", "KORD", "KSEA"]);
    assert!(pick.picked[1].km_from_leader.is_some());
    // The missing list is asked for again only once the cache runs out.
    let mut again = run(&lane);
    picking
        .picker
        .choose(&lane, &base, &mut again)
        .await
        .unwrap();
    assert_eq!(picking.oracle.lock().unwrap().eligible_calls, 1);
}

#[tokio::test]
async fn stations_without_a_forecast_are_dropped_and_the_lanes_list_fills_in() {
    let picking = fixture(Oracle {
        eligible: Some(vec![
            station("KDEN", "DEN", DENVER),
            station("KXXX", "", CHICAGO),
            station("KSEA", "SEA", SEATTLE),
        ]),
        forecasts: vec![forecast("KDEN", 25, 60, 30, 80)],
        ..Default::default()
    })
    .await;
    let lane = lane("[picker]\nmode = \"weather\"\n");
    let mut config = run(&lane);
    let drawn = config.stations.clone();
    let pick = picking
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .unwrap();
    assert_eq!(pick.scored, 1);
    assert_eq!(config.stations.len(), 3);
    assert_eq!(config.stations[0], "KDEN");
    assert_eq!(
        pick.picked.iter().map(|p| p.role).collect::<Vec<_>>(),
        [Role::Leader, Role::Fallback, Role::Fallback]
    );
    // From the run's draw from the lane's list, first.
    let fill: Vec<_> = drawn.iter().filter(|id| *id != "KDEN").take(2).collect();
    assert_eq!(config.stations[1..].iter().collect::<Vec<_>>(), fill);
    assert!(pick.explanation.ends_with("from the lane's list"));
}

#[tokio::test]
async fn forecasts_are_asked_for_in_batches() {
    let stations: Vec<StationInfo> = (0..120)
        .map(|n| station(&format!("K{n:03}"), "", (30.0 + n as f64 / 10.0, -100.0)))
        .collect();
    let forecasts = stations
        .iter()
        .enumerate()
        .map(|(n, s)| forecast(&s.station_id, 40, 50 + n as i64 % 30, 10, 0))
        .collect();
    let picking = fixture(Oracle {
        eligible: Some(stations),
        forecasts,
        ..Default::default()
    })
    .await;
    let lane = lane("[picker]\nmode = \"weather\"\nprefer_known_airports = false\n");
    let mut config = run(&lane);
    let pick = picking
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .unwrap();
    assert_eq!(pick.scored, 120);
    let oracle = picking.oracle.lock().unwrap();
    assert_eq!(oracle.forecast_batches.len(), 3);
    assert!(oracle
        .forecast_batches
        .iter()
        .all(|batch| batch.len() <= FORECAST_BATCH));
}

#[tokio::test]
async fn a_fixed_lane_keeps_its_draw() {
    let picking = fixture(Oracle::default()).await;
    let lane = lane("[picker]\nmode = \"fixed\"\n");
    let mut config = run(&lane);
    let drawn = config.stations.clone();
    assert!(picking
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .is_none());
    assert_eq!(config.stations, drawn);
    assert_eq!(picking.oracle.lock().unwrap().eligible_calls, 0);
}

#[test]
fn the_picker_table_loads_with_defaults_and_bad_settings_are_refused() {
    let lane = lane(
        r#"
[picker]
mode = "weather"
candidates = "configured"
prefer_known_airports = true
cluster_km = 600
recent_runs_to_avoid = 6
"#,
    );
    let picker = lane.picker.unwrap();
    assert_eq!(picker.candidates, Candidates::Configured);
    assert_eq!(picker.recent_runs_to_avoid, 6);
    assert_eq!(picker.weights, Weights::default());
    assert_eq!(picker.eligible_days, 3);

    let base = ScenarioConfig::default();
    for bad in [
        "[picker]\nmode = \"weather\"\ncluster_km = -1\n",
        "[picker]\nweights = { swing = 0, wind = 0, precip = 0, extremes = 0 }\n",
        "[picker]\nweights = { swing = -0.5 }\n",
    ] {
        let lane: LaneConfig = toml::from_str(&format!(
            "name = \"open\"\nscenarios = [\"full_lifecycle\"]\nobservation_windows_secs = [86400]\nstations = [\"KDEN\"]\n{bad}"
        ))
        .unwrap();
        assert!(lane.validate(&base).is_err(), "{bad}");
    }
    let unknown: Result<LaneConfig, _> = toml::from_str(
        "name = \"open\"\nscenarios = [\"full_lifecycle\"]\n[picker]\nmode = \"weather\"\ncolour = 1\n",
    );
    assert!(unknown.is_err());
}

/// A short-history lane must not fill the cache for one requiring a longer record.
#[tokio::test]
async fn eligible_cache_keeps_each_history_window_separate() {
    let f = fixture(Oracle {
        eligible: Some(vec![station("KDEN", "DEN", DENVER)]),
        ..Default::default()
    })
    .await;
    assert_eq!(f.picker.eligible(3, 24).await.unwrap().len(), 1);
    f.oracle.lock().unwrap().eligible = Some(vec![]);
    assert!(f.picker.eligible(30, 24).await.is_none());
    assert_eq!(f.picker.eligible(3, 24).await.unwrap().len(), 1);
    assert_eq!(f.oracle.lock().unwrap().eligible_calls, 2);
}
