//! Weather discovery uses the oracle's settlement eligibility and the requested forecast window.
use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, Date, OffsetDateTime};

use tokio_util::{sync::CancellationToken, task::TaskTracker};

use super::{
    oracle_weather::Station,
    refresh_cache::{Cached, RefreshCache},
};

#[derive(Debug, Clone, Deserialize)]
pub struct EligibleStation {
    #[serde(flatten)]
    pub station: Station,
    pub clean_days: u32,
    pub days_checked: u32,
    pub last_report: String,
    pub forecast_through: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Forecast {
    pub station_id: String,
    pub temp_high: i64,
    pub temp_low: i64,
    pub wind_speed: Option<i64>,
    pub precip_chance: Option<i64>,
    pub wind_direction: Option<i64>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub eligible: EligibleStation,
    pub high: i64,
    pub low: i64,
    pub wind_knots: Option<i64>,
    pub rain_chance: Option<i64>,
    pub forecasts: Vec<Forecast>,
}

#[derive(Debug)]
pub struct Discovery {
    pub candidates: Vec<Candidate>,
    pub eligible_count: usize,
    pub missing_forecasts: usize,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Filters {
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub window: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub weather: String,
    /// ICAO station id and radius, to compare stations near a weather system.
    #[serde(default)]
    pub near: String,
    pub radius_km: Option<u32>,
    pub history_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Window {
    pub history_days: u32,
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
}

impl Filters {
    pub fn window(&self, now: OffsetDateTime) -> anyhow::Result<Window> {
        let day = if self.day.is_empty() {
            (now + time::Duration::DAY).date()
        } else {
            Date::parse(
                &self.day,
                &time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")?,
            )?
        };
        let midnight = day.midnight().assume_utc();
        let (start, hours) = match self.window.as_str() {
            "" | "full" => (midnight, 24),
            "day" => (midnight + time::Duration::hours(12), 12),
            "night" => (midnight, 12),
            "two_days" => (midnight, 48),
            _ => anyhow::bail!("Choose a full day, daytime, nighttime, or two-day window"),
        };
        ensure!(
            start > now && start <= now + time::Duration::days(6),
            "Choose a future window within six days"
        );
        ensure!(
            self.location.len() <= 100 && self.near.len() <= 8,
            "Location filter is too long"
        );
        ensure!(
            matches!(
                self.weather.as_str(),
                "" | "swing" | "wind" | "rain" | "hot" | "cold"
            ),
            "Unknown weather order"
        );
        ensure!(
            self.radius_km.is_none_or(|r| (25..=3000).contains(&r)),
            "Radius must be 25–3000 km"
        );
        ensure!(
            (1..=31).contains(&self.history_days.unwrap_or(3)),
            "History must be 1–31 days"
        );
        Ok(Window {
            history_days: self.history_days.unwrap_or(3),
            start,
            end: start + time::Duration::hours(hours),
        })
    }

    pub fn select<'a>(&self, data: &'a Discovery) -> anyhow::Result<Vec<&'a Candidate>> {
        let location = self.location.trim().to_lowercase();
        let near = self.near.trim().to_uppercase();
        let center = if near.is_empty() {
            None
        } else {
            Some(
                &data
                    .candidates
                    .iter()
                    .find(|c| c.eligible.station.station_id == near)
                    .context("The nearby station must have an eligible forecast for this window")?
                    .eligible
                    .station,
            )
        };
        let mut candidates: Vec<_> = data
            .candidates
            .iter()
            .filter(|c| {
                let s = &c.eligible.station;
                let matches = [&s.station_id, &s.station_name, &s.state, &s.iata_id]
                    .iter()
                    .any(|text| text.to_lowercase().contains(&location));
                matches
                    && center.is_none_or(|center| {
                        distance_km(center, s) <= f64::from(self.radius_km.unwrap_or(500))
                    })
            })
            .collect();
        candidates.sort_by(|a, b| {
            let value = |c: &Candidate| match self.weather.as_str() {
                "wind" => c.wind_knots,
                "rain" => c.rain_chance,
                "hot" => Some(c.high),
                "cold" => Some(-c.low),
                _ => Some(c.high - c.low),
            };
            value(b).cmp(&value(a)).then_with(|| {
                a.eligible
                    .station
                    .station_id
                    .cmp(&b.eligible.station.station_id)
            })
        });
        Ok(candidates)
    }
}

fn distance_km(a: &Station, b: &Station) -> f64 {
    let lat = ((b.latitude - a.latitude).to_radians() / 2.0).sin().powi(2);
    let lon = ((b.longitude - a.longitude).to_radians() / 2.0)
        .sin()
        .powi(2);
    12_742.0
        * (lat + a.latitude.to_radians().cos() * b.latitude.to_radians().cos() * lon)
            .clamp(0.0, 1.0)
            .sqrt()
            .asin()
}

#[derive(Debug, Deserialize)]
struct DiscoveryForecasts {
    stations: Vec<EligibleStation>,
    forecasts: Vec<Forecast>,
}

pub struct WeatherDiscovery {
    http: reqwest::Client,
    base: String,
    cache: Arc<RefreshCache<Window, Discovery>>,
}

impl WeatherDiscovery {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(30))
                .build()?,
            base: base.trim_end_matches('/').to_owned(),
            cache: Arc::new(RefreshCache::new()),
        })
    }

    /// Prepare tomorrow's default view before an operator opens the page.
    /// The minute tick refreshes near expiry and follows the UTC date rollover.
    pub fn spawn_refresher(self: &Arc<Self>, tracker: &TaskTracker, cancel: CancellationToken) {
        let service = self.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if let Ok(window) = Filters::default().window(OffsetDateTime::now_utc()) {
                            let fetcher = service.clone();
                            service.cache.refresh(window.clone(), Duration::from_secs(240), move || async move {
                                tokio::time::timeout(Duration::from_secs(60), fetcher.fetch(&window)).await?
                            });
                        }
                    }
                }
            }
        });
    }

    pub async fn read(self: &Arc<Self>, window: Window) -> Cached<Discovery> {
        let service = self.clone();
        self.cache
            .get(
                window.clone(),
                Duration::from_secs(300),
                Duration::from_millis(1500),
                move || async move {
                    tokio::time::timeout(Duration::from_secs(60), service.fetch(&window)).await?
                },
            )
            .await
    }

    pub async fn eligible(&self, window: &Window) -> anyhow::Result<Vec<EligibleStation>> {
        ensure!(
            (1..=31).contains(&window.history_days),
            "History must be 1–31 days"
        );
        let hours = (window.end - window.start).whole_hours().clamp(1, 24);
        let stations: Vec<EligibleStation> = self
            .http
            .get(format!("{}/stations/eligible", self.base))
            .query(&[
                ("days", i64::from(window.history_days)),
                ("window_hours", hours),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        // The endpoint checks coverage starting now; the UI must check its future window too.
        Ok(stations
            .into_iter()
            .filter(|s| {
                OffsetDateTime::parse(&s.forecast_through, &Rfc3339)
                    .is_ok_and(|through| through >= window.end)
            })
            .collect())
    }

    async fn fetch(&self, window: &Window) -> anyhow::Result<Discovery> {
        let response: DiscoveryForecasts = self
            .http
            .get(format!("{}/stations/eligible/forecasts", self.base))
            .query(&[
                ("days", window.history_days.to_string()),
                ("start", window.start.format(&Rfc3339)?),
                ("end", window.end.format(&Rfc3339)?),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            response.stations.len() <= 5000,
            "Oracle returned too many eligible stations"
        );
        // Keep the future-window check here as well as in Oracle. Creation always
        // rechecks the original eligible endpoint instead of trusting this cache.
        let eligible: Vec<_> = response
            .stations
            .into_iter()
            .filter(|station| {
                OffsetDateTime::parse(&station.forecast_through, &Rfc3339)
                    .is_ok_and(|through| through >= window.end)
            })
            .collect();
        let mut forecasts: HashMap<String, Vec<Forecast>> = HashMap::new();
        for forecast in response.forecasts {
            forecasts
                .entry(forecast.station_id.clone())
                .or_default()
                .push(forecast);
        }
        let eligible_count = eligible.len();
        let candidates: Vec<_> = eligible
            .into_iter()
            .filter_map(|eligible| {
                let rows = forecasts.get(&eligible.station.station_id)?;
                Some(Candidate {
                    eligible,
                    high: rows.iter().map(|f| f.temp_high).max()?,
                    low: rows.iter().map(|f| f.temp_low).min()?,
                    wind_knots: rows.iter().filter_map(|f| f.wind_speed).max(),
                    rain_chance: rows.iter().filter_map(|f| f.precip_chance).max(),
                    forecasts: rows.clone(),
                })
            })
            .collect();
        Ok(Discovery {
            missing_forecasts: eligible_count - candidates.len(),
            candidates,
            eligible_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovery_requires_future_coverage_and_preserves_unknown_weather() {
        use axum::{extract::Query, routing::get, Json, Router};
        use serde_json::json;
        let app = Router::new().route("/stations/eligible/forecasts", get(|Query(query): Query<HashMap<String, String>>| async move {
            assert_eq!(query["days"], "3");
            assert_eq!(query["start"], "2026-10-03T00:00:00Z");
            assert_eq!(query["end"], "2026-10-04T00:00:00Z");
            Json(json!({
                "stations": [
                    {"station_id":"KSEA","station_name":"Seattle","state":"WA","iata_id":"SEA","latitude":47.45,"longitude":-122.3,"clean_days":3,"days_checked":3,"last_report":"2026-10-02T10:00:00Z","forecast_through":"2026-10-05T00:00:00Z"},
                    {"station_id":"KPDX","station_name":"Portland","state":"OR","iata_id":"PDX","latitude":45.58,"longitude":-122.6,"clean_days":3,"days_checked":3,"last_report":"2026-10-02T10:00:00Z","forecast_through":"2026-10-03T00:00:00Z"}
                ],
                "forecasts": [
                    {"station_id":"KSEA", "temp_high":68, "temp_low":50},
                    {"station_id":"KPDX", "temp_high":95, "temp_low":50},
                    {"station_id":"KORD", "temp_high":100, "temp_low":30}
                ]
            }))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let service = WeatherDiscovery::new(&url).unwrap();
        let now = OffsetDateTime::parse("2026-10-02T10:00:00Z", &Rfc3339).unwrap();
        let window = Filters::default().window(now).unwrap();
        let data = service.fetch(&window).await.unwrap();
        assert_eq!(data.eligible_count, 1);
        assert_eq!(data.candidates[0].wind_knots, None);
        assert_eq!(data.candidates[0].rain_chance, None);
        assert_eq!(
            Filters {
                location: "washington".into(),
                ..Default::default()
            }
            .select(&data)
            .unwrap()
            .len(),
            0
        );
        assert_eq!(
            Filters {
                location: "wa".into(),
                ..Default::default()
            }
            .select(&data)
            .unwrap()
            .len(),
            1
        );
        assert!(Filters {
            near: "KPDX".into(),
            ..Default::default()
        }
        .select(&data)
        .is_err());
        let mut other_history = window.clone();
        other_history.history_days = 30;
        assert_ne!(window, other_history);
        server.abort();
    }

    #[tokio::test]
    async fn discovery_negotiates_and_decodes_compressed_oracle_responses() {
        use axum::{
            http::{header, HeaderMap},
            routing::get,
            Router,
        };
        // Gzip of {"stations":[],"forecasts":[]}; the wire body must be decoded
        // before JSON parsing. Without gzip support this regression fails.
        const BODY: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 171, 86, 42, 46, 73, 44, 201, 204, 207, 43, 86, 178,
            138, 142, 213, 81, 74, 203, 47, 74, 77, 78, 44, 46, 1, 115, 107, 1, 222, 124, 41, 140,
            30, 0, 0, 0,
        ];
        let app = Router::new().route(
            "/stations/eligible/forecasts",
            get(|headers: HeaderMap| async move {
                assert!(headers[header::ACCEPT_ENCODING]
                    .to_str()
                    .unwrap()
                    .split(',')
                    .any(|encoding| encoding.trim() == "gzip"));
                (
                    [
                        (header::CONTENT_ENCODING, "gzip"),
                        (header::CONTENT_TYPE, "application/json"),
                    ],
                    BODY,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service =
            WeatherDiscovery::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let window = Filters::default()
            .window(OffsetDateTime::now_utc())
            .unwrap();
        let data = service.fetch(&window).await.unwrap();
        assert_eq!(data.eligible_count, 0);
        assert!(data.candidates.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn warmer_prepares_default_view_and_stops_on_shutdown() {
        use axum::{routing::get, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let app = Router::new().route(
            "/stations/eligible/forecasts",
            get(move || {
                let observed = observed.clone();
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"stations":[],"forecasts":[]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service = Arc::new(
            WeatherDiscovery::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let tracker = TaskTracker::new();
        let cancel = CancellationToken::new();
        service.spawn_refresher(&tracker, cancel.clone());
        let window = Filters::default()
            .window(OffsetDateTime::now_utc())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while service.cache.peek(&window).is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(service.read(window).await.latest.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cancel.cancel();
        tracker.close();
        tokio::time::timeout(Duration::from_secs(1), tracker.wait())
            .await
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn unavailable_bulk_discovery_is_not_replaced_by_partial_or_unverified_data() {
        use axum::{http::StatusCode, routing::get, Router};
        let app = Router::new().route(
            "/stations/eligible/forecasts",
            get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service = Arc::new(
            WeatherDiscovery::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let window = Filters::default()
            .window(OffsetDateTime::now_utc())
            .unwrap();
        let data = service.read(window).await;
        assert!(data.latest.is_none());
        assert!(!data.refreshing);
        server.abort();
    }

    #[test]
    fn half_day_windows_are_aligned_and_invalid_windows_are_rejected() {
        let now = OffsetDateTime::parse("2026-10-02T10:00:00Z", &Rfc3339).unwrap();
        let filters = Filters {
            day: "2026-10-03".into(),
            window: "day".into(),
            ..Default::default()
        };
        let window = filters.window(now).unwrap();
        assert_eq!(window.start.hour(), 12);
        assert_eq!((window.end - window.start).whole_hours(), 12);
        assert!(Filters {
            day: "2026-10-01".into(),
            ..Default::default()
        }
        .window(now)
        .is_err());
        assert!(Filters {
            window: "arbitrary".into(),
            ..Default::default()
        }
        .window(now)
        .is_err());
    }
}
