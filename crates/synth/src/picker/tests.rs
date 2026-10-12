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
use super::score::Role;
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
    /// Whether it answers eligible stations and their forecasts in one request.
    discovery: bool,
    discovery_calls: usize,
    /// The station ids of each forecast request.
    forecast_batches: Vec<Vec<String>>,
}

type Shared = Arc<StdMutex<Oracle>>;

fn json(station: &StationInfo) -> serde_json::Value {
    serde_json::to_value(station).unwrap()
}

async fn eligible(
    State(oracle): State<Shared>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut oracle = oracle.lock().unwrap();
    oracle.eligible_calls += 1;
    match &oracle.eligible {
        Some(stations) => Json(
            stations
                .iter()
                .map(|station| {
                    let mut value = json(station);
                    if station.coverage.is_some() {
                        value["recent_window_hours"] =
                            query["window_hours"].parse::<u32>().unwrap().into();
                    }
                    value
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
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
        .map(forecast_json)
        .collect();
    oracle.forecast_batches.push(ids);
    Json(rows).into_response()
}

fn forecast_json(row: &Forecast) -> serde_json::Value {
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
}

/// The eligible stations and their forecasts in one answer, as the oracle has since 2.7.0.
async fn eligible_forecasts(
    State(oracle): State<Shared>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut oracle = oracle.lock().unwrap();
    if !oracle.discovery {
        return StatusCode::NOT_FOUND.into_response();
    }
    assert!(["days", "start", "end"]
        .iter()
        .all(|key| query.contains_key(*key)));
    oracle.discovery_calls += 1;
    let start = OffsetDateTime::parse(
        &query["start"],
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let end = OffsetDateTime::parse(
        &query["end"],
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let stations: Vec<_> = oracle
        .eligible
        .iter()
        .flatten()
        .map(|station| {
            let mut value = json(station);
            if station.coverage.is_some() {
                value["recent_window_hours"] = (end - start).whole_hours().min(24).into();
            }
            value
        })
        .collect();
    let forecasts: Vec<_> = oracle.forecasts.iter().map(forecast_json).collect();
    Json(serde_json::json!({ "stations": stations, "forecasts": forecasts })).into_response()
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
        .route("/stations/eligible/forecasts", get(eligible_forecasts))
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

const PENSACOLA: (f64, f64) = (30.48, -87.19);
const SELMA: (f64, f64) = (32.34, -86.98);
const EGLIN: (f64, f64) = (30.65, -86.53);

/// [`weather`] with a landfalling storm on the Gulf coast that has more weather than Denver,
/// a station inland under its rain, and a calm one on the coast beside it.
fn storm_weather() -> (Vec<StationInfo>, Vec<Forecast>) {
    let (mut stations, mut forecasts) = weather();
    stations.extend([
        station("KPNS", "PNS", PENSACOLA),
        station("KSEM", "SEM", SELMA),
        station("KEGI", "EGI", EGLIN),
    ]);
    let mut storm = forecast("KPNS", 66, 82, 77, 96);
    storm.rain_amt = Some(5.41);
    let mut inland = forecast("KSEM", 60, 80, 24, 90);
    inland.rain_amt = Some(2.07);
    forecasts.extend([storm, inland, forecast("KEGI", 70, 75, 5, 0)]);
    (stations, forecasts)
}

#[tokio::test]
async fn a_lane_leaves_out_a_storm_and_the_stations_near_it() {
    let (stations, forecasts) = storm_weather();
    let picking = fixture(Oracle {
        eligible: Some(stations),
        forecasts,
        ..Default::default()
    })
    .await;
    let guarded = lane("[picker]\nmode = \"weather\"\ncluster_km = 600\n");
    let mut config = run(&guarded);
    let pick = picking
        .picker
        .choose(&guarded, &ScenarioConfig::default(), &mut config)
        .await
        .unwrap();
    // Scored without the storm, Denver's weather leads as it does without one.
    assert_eq!(config.stations, ["KDEN", "KCOS", "KCYS"]);
    assert_eq!((pick.candidates, pick.scored), (9, 9));
    assert_eq!(pick.storms.left_out, ["KPNS", "KSEM", "KEGI"]);
    let page = view::section(&pick).into_string();
    assert!(
        page.contains("Left out 3 under or within 300 km of a storm-strength forecast: KPNS 89 mph, 5.41 in of rain."),
        "{page}"
    );
    let id = config.competition_id.unwrap();
    assert_eq!(store::pick_for(&picking.db, id).await.unwrap(), pick);

    // A pick saved before the guard reads back with nothing left out, and its page says none.
    let mut saved = serde_json::to_value(&pick).unwrap();
    saved.as_object_mut().unwrap().remove("storms");
    let old: Pick = serde_json::from_value(saved).unwrap();
    assert_eq!(old.storms, score::Storms::default());
    assert!(!view::section(&old).into_string().contains("Left out"));

    // With the guard off, the storm leads, and the station inland that went silent with it.
    let unguarded =
        lane("[picker]\nmode = \"weather\"\ncluster_km = 600\nstorm = { enabled = false }\n");
    let mut config = run(&unguarded);
    let pick = picking
        .picker
        .choose(&unguarded, &ScenarioConfig::default(), &mut config)
        .await
        .unwrap();
    assert_eq!(config.stations[..2], ["KPNS", "KSEM"]);
    assert!(pick.storms.left_out.is_empty());
}

/// A storm the lane picked last time is not picked again, and neither are the stations near it,
/// whether the oracle sends every forecast at once or the lane asks for them in batches.
#[tokio::test]
async fn a_storm_used_recently_still_keeps_its_neighbours_out() {
    for discovery in [false, true] {
        let (stations, forecasts) = storm_weather();
        let picking = fixture(Oracle {
            eligible: Some(stations),
            forecasts,
            discovery,
            ..Default::default()
        })
        .await;
        // One-request discovery takes only windows in the future.
        let close = if discovery {
            crate::runner::lanes::next_half(OffsetDateTime::now_utc() + time::Duration::DAY)
        } else {
            datetime!(2026-10-02 00:00 UTC)
        };
        let run = |lane: &LaneConfig| {
            lane.run_config(&ScenarioConfig::default(), 0, Some(close))
                .1
        };
        let base = ScenarioConfig::default();
        let without = lane(
            "[picker]\nmode = \"weather\"\nrecent_runs_to_avoid = 1\nstorm = { enabled = false }\n",
        );
        let mut first = run(&without);
        picking
            .picker
            .choose(&without, &base, &mut first)
            .await
            .unwrap();
        // Independent picks: the storm, then the best 300 km or more from it and each other.
        assert_eq!(first.stations, ["KPNS", "KDEN", "KORD"]);

        let guarded = lane("[picker]\nmode = \"weather\"\nrecent_runs_to_avoid = 1\n");
        let mut second = run(&guarded);
        let pick = picking
            .picker
            .choose(&guarded, &base, &mut second)
            .await
            .unwrap();
        assert_eq!(pick.avoided, ["KDEN", "KORD", "KPNS"]);
        assert_eq!(
            pick.storms.left_out,
            ["KSEM", "KEGI"],
            "discovery {discovery}"
        );
        assert_eq!(pick.storms.forecasts[0].station_id, "KPNS");
        // Colorado Springs leads; Cheyenne is too close to it.
        assert_eq!(second.stations, ["KCOS", "KSEA", "KJFK"]);
        assert_eq!(
            picking.oracle.lock().unwrap().discovery_calls,
            if discovery { 2 } else { 0 }
        );
    }
}

/// A station that stopped reporting in a storm cannot be picked, but its forecast still keeps
/// the stations near it out.
#[tokio::test]
async fn a_storm_whose_station_went_silent_still_keeps_its_neighbours_out() {
    for discovery in [false, true] {
        let (mut stations, forecasts) = storm_weather();
        stations[6].coverage.as_mut().unwrap().last_report -= time::Duration::hours(2);
        let picking = fixture(Oracle {
            eligible: Some(stations),
            forecasts,
            discovery,
            ..Default::default()
        })
        .await;
        let lane = lane("[picker]\nmode = \"weather\"\ncluster_km = 600\n");
        let mut config = run(&lane);
        if discovery {
            config.observation_start = Some(crate::runner::lanes::next_half(
                OffsetDateTime::now_utc() + time::Duration::DAY,
            ));
        }
        let pick = picking
            .picker
            .choose(&lane, &ScenarioConfig::default(), &mut config)
            .await
            .unwrap();
        assert_eq!(config.stations, ["KDEN", "KCOS", "KCYS"]);
        assert_eq!(pick.candidates, 8, "the silent station is not a candidate");
        assert_eq!(
            pick.storms.left_out,
            ["KSEM", "KEGI"],
            "discovery {discovery}"
        );
        assert_eq!(pick.storms.forecasts[0].station_id, "KPNS");
    }
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
async fn missing_eligibility_stops_creation_and_does_not_cache_failure() {
    let (stations, forecasts) = weather();
    let f = fixture(Oracle {
        eligible: None,
        stations: stations.clone(),
        forecasts,
        ..Default::default()
    })
    .await;
    let lane = lane("[picker]\nmode = \"weather\"\n");
    let mut config = run(&lane);
    assert!(f
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .is_err());
    assert!(config.competition_id.is_none());
    f.oracle.lock().unwrap().eligible = Some(stations);
    assert!(f
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .is_ok());
    assert_eq!(f.oracle.lock().unwrap().eligible_calls, 2);
}

#[tokio::test]
async fn insufficient_forecasts_never_fill_from_configured_stations() {
    let f = fixture(Oracle {
        eligible: Some(vec![station("KDEN", "DEN", DENVER)]),
        forecasts: vec![forecast("KDEN", 25, 60, 30, 80)],
        ..Default::default()
    })
    .await;
    let lane = lane("[picker]\nmode = \"weather\"\n");
    let mut config = run(&lane);
    assert!(f
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .is_err());
    assert!(config.competition_id.is_none());
}

#[tokio::test]
async fn lanes_without_picker_settings_always_use_eligibility() {
    let (stations, forecasts) = weather();
    let f = fixture(Oracle {
        eligible: Some(stations),
        forecasts,
        ..Default::default()
    })
    .await;
    let lane = lane("");
    let mut config = run(&lane);
    f.picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .unwrap();
    assert_eq!(config.stations, ["KDEN", "KCOS", "KCYS"]);
    assert_eq!(f.oracle.lock().unwrap().eligible_calls, 1);
}

#[tokio::test]
async fn explicit_manual_stations_must_all_be_eligible() {
    let f = fixture(Oracle {
        eligible: Some(vec![station("KDEN", "DEN", DENVER)]),
        ..Default::default()
    })
    .await;
    let mut config = ScenarioConfig::default();
    assert!(f.picker.validate_stations(&config).await.is_err());
    config.stations = vec!["KDEN".into()];
    assert!(
        f.picker.validate_stations(&config).await.is_err(),
        "eligibility alone does not provide the requested forecast"
    );
    f.oracle.lock().unwrap().forecasts = vec![forecast("KDEN", 30, 70, 20, 10)];
    f.picker.validate_stations(&config).await.unwrap();
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

/// Where the oracle offers it, the eligible stations and their forecasts come in one request
/// instead of a list and a request per batch of stations.
#[tokio::test]
async fn eligible_forecasts_come_in_one_request_where_the_oracle_offers_them() {
    let (stations, forecasts) = weather();
    let picking = fixture(Oracle {
        eligible: Some(stations),
        forecasts,
        discovery: true,
        ..Default::default()
    })
    .await;
    let lane = lane("[picker]\nmode = \"weather\"\ncluster_km = 600\n");
    let close = crate::runner::lanes::next_half(OffsetDateTime::now_utc() + time::Duration::DAY);
    let mut config = lane
        .run_config(&ScenarioConfig::default(), 0, Some(close))
        .1;
    let pick = picking
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .unwrap();
    assert_eq!(config.stations, ["KDEN", "KCOS", "KCYS"]);
    assert_eq!((pick.candidates, pick.scored), (6, 6));
    let oracle = picking.oracle.lock().unwrap();
    assert_eq!(oracle.discovery_calls, 1);
    assert_eq!(oracle.eligible_calls, 0);
    assert!(oracle.forecast_batches.is_empty());
}

#[tokio::test]
async fn a_fixed_lane_cannot_bypass_eligibility() {
    let f = fixture(Oracle::default()).await;
    let lane = lane("[picker]\nmode = \"fixed\"\n");
    let mut config = run(&lane);
    assert!(f
        .picker
        .choose(&lane, &ScenarioConfig::default(), &mut config)
        .await
        .is_err());
    assert_eq!(f.oracle.lock().unwrap().eligible_calls, 1);
}

#[test]
fn the_picker_table_loads_with_defaults_and_bad_settings_are_refused() {
    let configured = lane(
        r#"
[picker]
mode = "weather"
candidates = "configured"
prefer_known_airports = true
cluster_km = 600
recent_runs_to_avoid = 6
"#,
    );
    let picker = configured.picker.unwrap();
    assert_eq!(picker.candidates, Candidates::Configured);
    assert_eq!(picker.recent_runs_to_avoid, 6);
    assert_eq!(picker.weights, Weights::default());
    assert_eq!(picker.eligible_days, 3);
    // The storm guard is on with the measured thresholds unless a lane says otherwise.
    assert_eq!(
        picker.storm,
        StormGuard {
            enabled: true,
            wind_mph: 45.0,
            rain_in: 2.0,
            radius_km: 300.0
        }
    );
    let storm = lane("[picker]\n[picker.storm]\nwind_mph = 50\nradius_km = 250\n")
        .picker
        .unwrap()
        .storm;
    assert_eq!(
        (storm.wind_mph, storm.rain_in, storm.radius_km),
        (50.0, 2.0, 250.0)
    );

    let base = ScenarioConfig::default();
    for bad in [
        "[picker]\nmode = \"weather\"\ncluster_km = -1\n",
        "[picker]\nweights = { swing = 0, wind = 0, precip = 0, extremes = 0 }\n",
        "[picker]\nweights = { swing = -0.5 }\n",
        "[picker]\nstorm = { wind_mph = 0 }\n",
        "[picker]\nstorm = { rain_in = -1 }\n",
        "[picker]\nstorm = { radius_km = -300 }\n",
        "[picker]\nstorm = { radius_km = nan }\n",
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
    let unknown: Result<LaneConfig, _> = toml::from_str(
        "name = \"open\"\nscenarios = [\"full_lifecycle\"]\n[picker]\nstorm = { gust_mph = 60 }\n",
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
    assert!(f.picker.eligible(30, 24).await.is_err());
    assert_eq!(f.picker.eligible(3, 24).await.unwrap().len(), 1);
    assert_eq!(f.oracle.lock().unwrap().eligible_calls, 2);
}

#[tokio::test]
async fn both_discovery_paths_reject_missing_stale_or_incomplete_coverage() {
    for discovery in [false, true] {
        let (mut stations, forecasts) = weather();
        stations[0].coverage.as_mut().unwrap().clean_days = 2;
        stations[1].coverage.as_mut().unwrap().last_report -= time::Duration::hours(2);
        stations[2].coverage.as_mut().unwrap().coverage_checked_at -= time::Duration::minutes(21);
        stations[3].coverage = None;
        stations[4].coverage.as_mut().unwrap().coverage_checked_at += time::Duration::HOUR;
        let f = fixture(Oracle {
            eligible: Some(stations),
            forecasts,
            discovery,
            ..Default::default()
        })
        .await;
        let mut lane = lane("");
        lane.stations_per_run = Some(1);
        let mut config = run(&lane);
        config.observation_start = Some(OffsetDateTime::now_utc() + time::Duration::HOUR);
        let picked = f
            .picker
            .choose(&lane, &ScenarioConfig::default(), &mut config)
            .await
            .unwrap();
        assert_eq!(picked.stations(), ["KSEA"]);
        assert!(picked.explanation.contains("coverage 3/3 clean days"));
        assert_eq!(
            f.oracle.lock().unwrap().discovery_calls,
            usize::from(discovery)
        );
    }
}

#[tokio::test]
async fn local_cache_cannot_extend_expired_coverage_evidence() {
    let f = fixture(Oracle::default()).await;
    let mut stale = station("KDEN", "DEN", DENVER);
    stale.coverage.as_mut().unwrap().coverage_checked_at -= time::Duration::minutes(21);
    f.picker
        .eligible
        .lock()
        .await
        .insert((3, 24), (Instant::now(), vec![stale]));
    assert!(f.picker.eligible(3, 24).await.is_err());
}
