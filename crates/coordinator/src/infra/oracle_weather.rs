//! Weather the leaderboard and entry form show, read from the oracle's public API.
//!
//! - `GET /oracle/events/{id}`: the event's readings, a forecast ("baseline") and an observed
//!   value per station and metric, which the oracle refreshes about hourly and scores entries
//!   with, each entry's score once the window has closed, and a `lines` event's Par bands.
//! - `GET /stations/observations`: the same aggregate over the window the oracle scores (the
//!   high and wind as maxima, the low as a minimum), as of the latest report, and that report's
//!   time.
//! - `GET /stations`: every station's name and location.
//! - `GET /stations/forecasts`: daily forecasts, to compute the baselines the oracle will use for
//!   an event it has not stored readings for yet.
//!
//! Each call can take a while under load, so pages read them through a
//! [`RefreshCache`](crate::infra::refresh_cache::RefreshCache) and never
//! wait on the oracle for long; see `domain::leaderboard`.

use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use crate::api::request_context::ParentRequestId;
use anyhow::Context;
use log::warn;
use serde::Deserialize;
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Date, OffsetDateTime,
    UtcOffset,
};
use uuid::Uuid;

/// Longest a single oracle request may take. Only background refreshes make these requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct OracleWeather {
    http: reqwest::Client,
    base_url: String,
}

/// The forecast and observed value of one metric at one station, as the oracle scores them.
#[derive(Debug, Clone, Deserialize)]
pub struct Reading {
    pub target: String,
    pub metric: String,
    pub baseline: Option<f64>,
    pub observed: Option<f64>,
}

/// A `lines` event's Par band for one station and metric, fixed when the oracle created the
/// event: Par when the observed value minus the forecast is from `lower` to `upper`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EventLine {
    pub target: String,
    pub metric: String,
    pub lower: f64,
    pub upper: f64,
}

/// Why the oracle could not settle an event at its last check of the data, in its own words.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SettlementBlock {
    /// The oracle's name for the reason: `incomplete_readings`.
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    /// When the oracle last checked, as it reports it.
    #[serde(default)]
    pub checked_at: Option<String>,
}

impl SettlementBlock {
    /// The oracle's name for the reason, or `blocked` when it gave none.
    pub fn code(&self) -> &str {
        if self.code.is_empty() {
            "blocked"
        } else {
            &self.code
        }
    }
}

/// What the oracle holds for a competition's event.
#[derive(Debug, Clone, Default)]
pub struct EventReadings {
    pub readings: Vec<Reading>,
    /// Par bands, for events scored with lines.
    pub lines: Vec<EventLine>,
    /// Points per oracle entry id. The oracle rescores entries on each run once the window has
    /// opened, so these are only final once the event is attested.
    pub scores: HashMap<Uuid, u64>,
    pub entry_count: usize,
    pub attested: bool,
    /// The oracle could not settle the event unsigned: its last check of the data failed.
    /// Players are told only that it happened; why is for the operator pages.
    pub settlement_block: Option<SettlementBlock>,
    /// When the event's contract expires unsigned, from the oracle's announcement.
    pub expiry: Option<OffsetDateTime>,
}

impl EventReadings {
    pub fn reading(&self, station: &str, metric: &str) -> Option<&Reading> {
        self.readings
            .iter()
            .find(|reading| reading.target == station && reading.metric == metric)
    }

    pub fn line(&self, station: &str, metric: &str) -> Option<&EventLine> {
        self.lines
            .iter()
            .find(|line| line.target == station && line.metric == metric)
    }

    /// Whether the oracle has attested the result, so its readings and scores are final.
    pub fn is_final(&self) -> bool {
        self.attested && self.entry_count > 0 && self.scores.len() == self.entry_count
    }
}

/// A weather station the oracle reads, from `GET /stations`.
#[derive(Debug, Clone, Deserialize)]
pub struct Station {
    pub station_id: String,
    pub station_name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub iata_id: String,
    #[serde(default)]
    pub elevation_m: Option<f64>,
    pub latitude: f64,
    pub longitude: f64,
}

/// The observations at one station over a window so far.
#[derive(Debug, Clone)]
pub struct StationObservations {
    pub station_id: String,
    /// The latest report inside the window.
    pub latest_report: Option<OffsetDateTime>,
    pub temp_high: Option<f64>,
    pub temp_low: Option<f64>,
    pub wind_speed: Option<f64>,
}

impl StationObservations {
    pub fn value(&self, metric: &str) -> Option<f64> {
        match metric {
            "temp_high" => self.temp_high,
            "temp_low" => self.temp_low,
            "wind_speed" => self.wind_speed,
            _ => None,
        }
    }
}

/// A time for a query string: UTC, so no `+` offset is read back as a space.
fn query_time(time: OffsetDateTime) -> anyhow::Result<String> {
    Ok(time.to_offset(UtcOffset::UTC).format(&Rfc3339)?)
}

impl OracleWeather {
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path_and_query: &str) -> anyhow::Result<T> {
        let url = format!("{}{path_and_query}", self.base_url);
        let response = self.http.get(&url).parent_request_id().send().await?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("{url} answered {status}");
        }
        Ok(response.json().await?)
    }

    /// The event's readings and entry scores. An event the oracle does not have yet reads as empty.
    pub async fn event_readings(&self, event_id: Uuid) -> anyhow::Result<EventReadings> {
        #[derive(Deserialize)]
        struct Entry {
            id: Uuid,
            base_score: Option<i64>,
        }
        #[derive(Deserialize)]
        struct Event {
            #[serde(default)]
            readings: Vec<Reading>,
            #[serde(default)]
            entries: Vec<Entry>,
            attestation: Option<serde_json::Value>,
            #[serde(default)]
            lines: Vec<EventLine>,
            #[serde(default)]
            settlement_block: Option<serde_json::Value>,
            #[serde(default)]
            event_announcement: Option<serde_json::Value>,
        }
        let url = format!("{}/oracle/events/{event_id}", self.base_url);
        let response = self.http.get(&url).parent_request_id().send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(EventReadings::default());
        }
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("{url} answered {status}");
        }
        let event: Event = response.json().await?;
        Ok(EventReadings {
            entry_count: event.entries.len(),
            scores: event
                .entries
                .iter()
                .filter_map(|entry| Some((entry.id, u64::try_from(entry.base_score?).ok()?)))
                .collect(),
            readings: event.readings,
            lines: event.lines,
            attested: event.attestation.is_some_and(|value| !value.is_null()),
            // A block the oracle words another way is still a block.
            settlement_block: event
                .settlement_block
                .filter(|value| !value.is_null())
                .map(|value| serde_json::from_value(value).unwrap_or_default()),
            expiry: event
                .event_announcement
                .and_then(|announcement| announcement.get("expiry")?.as_i64())
                .and_then(|expiry| OffsetDateTime::from_unix_timestamp(expiry).ok()),
        })
    }

    /// Observations at `stations` from `start` to `end`, aggregated as the oracle scores them.
    pub async fn observations(
        &self,
        stations: &[String],
        start: OffsetDateTime,
        end: OffsetDateTime,
    ) -> anyhow::Result<Vec<StationObservations>> {
        #[derive(Deserialize)]
        struct Row {
            station_id: String,
            end_time: Option<String>,
            temp_high: Option<f64>,
            temp_low: Option<f64>,
            wind_speed: Option<f64>,
        }
        if stations.is_empty() || start >= end {
            return Ok(vec![]);
        }
        let rows: Vec<Row> = self
            .get(&format!(
                "/stations/observations?station_ids={}&start={}&end={}&temperature_unit=fahrenheit",
                stations.join(","),
                query_time(start)?,
                query_time(end)?,
            ))
            .await
            .context("observations")?;
        let parse = |value: Option<String>| {
            value.and_then(|value| OffsetDateTime::parse(&value, &Rfc3339).ok())
        };
        Ok(rows
            .into_iter()
            .map(|row| StationObservations {
                station_id: row.station_id,
                latest_report: parse(row.end_time),
                temp_high: row.temp_high,
                temp_low: row.temp_low,
                wind_speed: row.wind_speed,
            })
            .collect())
    }

    pub async fn stations(&self) -> anyhow::Result<Vec<Station>> {
        self.get("/stations").await.context("stations")
    }

    /// The forecasts the oracle will compare a window's observations with, computed as it does
    /// before it has stored readings for a new event. The query is slow (seconds), so only a
    /// background refresh should make it, and only until the oracle's own readings exist.
    pub async fn forecast_baselines(
        &self,
        stations: &[String],
        start: OffsetDateTime,
        end: OffsetDateTime,
    ) -> anyhow::Result<Vec<Reading>> {
        if stations.is_empty() || start >= end {
            return Ok(vec![]);
        }
        let rows: Vec<DailyForecast> = self
            .get(&format!(
                "/stations/forecasts?station_ids={}&start={}&end={}&generated_start={}&generated_end={}&temperature_unit=fahrenheit",
                stations.join(","),
                query_time(start)?,
                query_time(end)?,
                query_time(start - BASELINE_LOOKBACK)?,
                query_time(start - time::Duration::NANOSECOND)?,
            ))
            .await
            .context("forecasts")?;
        // Said here, where an operator reads it; the form shows the forecasts it has.
        let days = window_days(start, end);
        let expected = (days.1 - days.0).whole_days() + 1;
        let short: Vec<&str> = stations
            .iter()
            .filter(|station| {
                let given = station_days(station, days, &rows).0.len() as i64;
                given > 0 && given < expected
            })
            .map(String::as_str)
            .collect();
        if !short.is_empty() {
            warn!(
                "The oracle's forecasts from {start} to {end} leave out some of the window's \
                 days at {}; picks there use the days it gave",
                short.join(", ")
            );
        }
        Ok(stations
            .iter()
            .flat_map(|station| baselines(station, start, end, &rows))
            .collect())
    }
}

/// NOAA publishes up to a week of forecast periods per issue.
const BASELINE_LOOKBACK: time::Duration = time::Duration::days(7);

/// One station's forecast for one UTC day.
#[derive(Debug, Clone, Deserialize)]
struct DailyForecast {
    station_id: String,
    /// `YYYY-MM-DD...`
    date: String,
    temp_high: Option<f64>,
    temp_low: Option<f64>,
    wind_speed: Option<f64>,
}

/// The first and last UTC day a window touches.
fn window_days(start: OffsetDateTime, end: OffsetDateTime) -> (Date, Date) {
    let first_day = start.to_offset(UtcOffset::UTC).date();
    let last_day = (end - time::Duration::NANOSECOND)
        .to_offset(UtcOffset::UTC)
        .date();
    (first_day, last_day)
}

/// A station's forecast for each of the window's days that has one, and whether no day has two.
fn station_days<'a>(
    station: &str,
    (first_day, last_day): (Date, Date),
    rows: &'a [DailyForecast],
) -> (BTreeMap<Date, &'a DailyForecast>, bool) {
    let mut daily = BTreeMap::new();
    let mut unique = true;
    for row in rows.iter().filter(|row| row.station_id == station) {
        let Some(day) = row
            .date
            .get(..10)
            .and_then(|day| Date::parse(day, format_description!("[year]-[month]-[day]")).ok())
        else {
            continue;
        };
        if day >= first_day && day <= last_day {
            unique &= daily.insert(day, row).is_none();
        }
    }
    (daily, unique)
}

/// A station's baselines over the window's UTC days, as noaa-oracle's `station_readings`
/// aggregates them: the high as the days' maximum, the low as their minimum, the wind as its
/// maximum. A day with two forecasts leaves no baseline, and so does a forecast without a value
/// for the metric. A day with no forecast at all is left out: the days that have one still
/// give a forecast to pick against, where requiring every day left the whole form without any.
fn baselines(
    station: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
    rows: &[DailyForecast],
) -> Vec<Reading> {
    let (daily, unique) = station_days(station, window_days(start, end), rows);
    if daily.is_empty() {
        return vec![];
    }
    let aggregate = |field: fn(&DailyForecast) -> Option<f64>, reduce: fn(f64, f64) -> f64| {
        if !unique {
            return None;
        }
        daily
            .values()
            .map(|row| field(row).filter(|value| value.is_finite()))
            .collect::<Option<Vec<f64>>>()?
            .into_iter()
            .reduce(reduce)
    };
    let reading = |metric: &str, baseline: Option<f64>| Reading {
        target: station.to_owned(),
        metric: metric.to_owned(),
        baseline,
        observed: None,
    };
    vec![
        reading("temp_high", aggregate(|row| row.temp_high, f64::max)),
        reading("temp_low", aggregate(|row| row.temp_low, f64::min)),
        reading("wind_speed", aggregate(|row| row.wind_speed, f64::max)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(station: &str, date: &str, high: f64, low: f64, wind: Option<f64>) -> DailyForecast {
        DailyForecast {
            station_id: station.into(),
            date: format!("{date} 00:00:00"),
            temp_high: Some(high),
            temp_low: Some(low),
            wind_speed: wind,
        }
    }

    fn baseline(readings: &[Reading], metric: &str) -> Option<f64> {
        readings
            .iter()
            .find(|reading| reading.metric == metric)
            .and_then(|reading| reading.baseline)
    }

    /// The oracle's answer for competition 01a0d161 (window 09:06 Sep 24 to 03:06 Sep 25 UTC),
    /// whose stored readings were KPWM 66/42/9, KBTV 71/41/7 and KBED 64/42/10.
    #[test]
    fn baselines_aggregate_the_window_days_like_the_oracle() {
        let rows = vec![
            row("KBED", "2026-09-25", 64.0, 48.0, Some(5.0)),
            row("KBED", "2026-09-23", 62.0, 48.0, Some(10.0)),
            row("KBTV", "2026-09-24", 69.0, 41.0, Some(7.0)),
            row("KBTV", "2026-09-25", 71.0, 44.0, Some(4.0)),
            row("KPWM", "2026-09-24", 64.0, 44.0, Some(9.0)),
            row("KBED", "2026-09-24", 64.0, 42.0, Some(10.0)),
            row("KPWM", "2026-09-25", 66.0, 42.0, Some(3.0)),
            row("KPWM", "2026-09-23", 62.0, 44.0, Some(4.0)),
        ];
        let start = OffsetDateTime::parse("2026-09-24T09:06:40Z", &Rfc3339).unwrap();
        let end = OffsetDateTime::parse("2026-09-25T03:06:40Z", &Rfc3339).unwrap();
        for (station, high, low, wind) in [
            ("KPWM", 66.0, 42.0, 9.0),
            ("KBTV", 71.0, 41.0, 7.0),
            ("KBED", 64.0, 42.0, 10.0),
        ] {
            let readings = baselines(station, start, end, &rows);
            assert_eq!(baseline(&readings, "temp_high"), Some(high), "{station}");
            assert_eq!(baseline(&readings, "temp_low"), Some(low), "{station}");
            assert_eq!(baseline(&readings, "wind_speed"), Some(wind), "{station}");
        }
    }

    /// The oracle once answered a two-day window with the second day's row only. The days it
    /// does give still make a forecast; a value it leaves out, or a day it gives twice, does not.
    #[test]
    fn a_missing_day_uses_the_days_given_and_a_missing_value_leaves_no_baseline() {
        let start = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).unwrap();
        let end = OffsetDateTime::parse("2030-01-02T12:00:00Z", &Rfc3339).unwrap();
        let one_day = vec![row("KORD", "2030-01-02", 50.0, 30.0, Some(8.0))];
        let readings = baselines("KORD", start, end, &one_day);
        assert_eq!(baseline(&readings, "temp_high"), Some(50.0));
        assert_eq!(baseline(&readings, "temp_low"), Some(30.0));
        assert_eq!(baseline(&readings, "wind_speed"), Some(8.0));
        let twice = vec![
            row("KORD", "2030-01-02", 50.0, 30.0, Some(8.0)),
            row("KORD", "2030-01-02", 51.0, 31.0, Some(9.0)),
        ];
        let readings = baselines("KORD", start, end, &twice);
        assert_eq!(baseline(&readings, "temp_high"), None);
        let no_wind = vec![
            row("KORD", "2030-01-01", 50.0, 30.0, None),
            row("KORD", "2030-01-02", 52.0, 31.0, Some(8.0)),
        ];
        let readings = baselines("KORD", start, end, &no_wind);
        assert_eq!(baseline(&readings, "temp_high"), Some(52.0));
        assert_eq!(
            baseline(&readings, "wind_speed"),
            None,
            "a nil forecast is not calm"
        );
        assert!(baselines("KMSP", start, end, &no_wind).is_empty());
    }

    #[tokio::test]
    async fn public_api_readings_keep_units_times_missing_values_and_final_scores() {
        use axum::{extract::Query, routing::get, Json, Router};
        use serde_json::json;

        let id = Uuid::now_v7();
        let entry_id = Uuid::now_v7();
        let event = json!({
            "readings": [
                { "target": "KPWM", "metric": "temp_high", "baseline": 66, "observed": 60.4 },
                { "target": "KPWM", "metric": "wind_speed", "baseline": 9, "observed": null }
            ],
            "entries": [{ "id": entry_id, "base_score": 10 }],
            "attestation": "signed-result"
        });
        let blocked_id = Uuid::now_v7();
        let blocked = json!({
            "entries": [{ "id": entry_id, "base_score": null }],
            "attestation": null,
            "settlement_block": {
                "code": "source_check_failed",
                "message": "KPWM: observations disagree",
                "checked_at": "2026-09-24T13:00:00Z"
            },
            "event_announcement": { "locking_points": [], "expiry": 1790341200 }
        });
        let app = Router::new()
            .route(
                &format!("/oracle/events/{id}"),
                get(move || async move { Json(event) }),
            )
            .route(
                &format!("/oracle/events/{blocked_id}"),
                get(move || async move { Json(blocked) }),
            )
            .route(
                "/stations/observations",
                get(|Query(query): Query<HashMap<String, String>>| async move {
                    assert_eq!(query["station_ids"], "KPWM,KBTV");
                    assert_eq!(query["temperature_unit"], "fahrenheit");
                    assert_eq!(query["start"], "2026-09-24T09:06:40Z");
                    assert_eq!(query["end"], "2026-09-24T12:00:00Z");
                    Json(json!([{
                        "station_id": "KPWM",
                        "start_time": "2026-09-24T09:51:00.000000Z",
                        "end_time": "2026-09-24T11:51:00.000000Z",
                        "temp_high": 46.94,
                        "temp_low": 41,
                        "wind_speed": null
                    }]))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let oracle =
            OracleWeather::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let readings = oracle.event_readings(id).await.unwrap();
        assert!(readings.is_final());
        assert!(readings.settlement_block.is_none());
        let blocked = oracle.event_readings(blocked_id).await.unwrap();
        assert!(!blocked.is_final());
        let block = blocked.settlement_block.expect("blocked");
        assert_eq!(block.code, "source_check_failed");
        assert_eq!(block.message, "KPWM: observations disagree");
        assert_eq!(block.checked_at.as_deref(), Some("2026-09-24T13:00:00Z"));
        assert_eq!(
            blocked.expiry,
            Some(OffsetDateTime::parse("2026-09-25T13:00:00Z", &Rfc3339).unwrap())
        );
        assert_eq!(readings.expiry, None);
        assert_eq!(readings.scores.get(&entry_id), Some(&10));
        assert_eq!(
            readings.reading("KPWM", "wind_speed").unwrap().observed,
            None
        );
        // A newly created event can legitimately be absent from the oracle.
        assert!(!oracle
            .event_readings(Uuid::now_v7())
            .await
            .unwrap()
            .is_final());

        let start = OffsetDateTime::parse("2026-09-24T09:06:40Z", &Rfc3339).unwrap();
        let end = OffsetDateTime::parse("2026-09-24T12:00:00Z", &Rfc3339).unwrap();
        let observations = oracle
            .observations(&["KPWM".into(), "KBTV".into()], start, end)
            .await
            .unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].temp_high, Some(46.94));
        assert_eq!(observations[0].temp_low, Some(41.0));
        assert_eq!(observations[0].wind_speed, None);
        assert_eq!(
            observations[0].latest_report,
            Some(OffsetDateTime::parse("2026-09-24T11:51:00Z", &Rfc3339).unwrap())
        );
        server.abort();
    }
}
