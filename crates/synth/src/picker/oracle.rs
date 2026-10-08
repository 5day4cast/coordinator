//! What the picker asks the oracle: which stations it can attest, where they are, and their
//! forecasts.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::Semaphore;

/// Stations named in one forecast request, well under the oracle's limit and a URL's length.
pub const FORECAST_BATCH: usize = 50;
/// Requests to the oracle in flight at once, from every lane together. The oracle sheds what
/// is over its own limit with a 503, and lanes that start together would otherwise each send
/// this many.
const REQUESTS_AT_ONCE: usize = 4;
/// Tries at a request the oracle shed or failed, the first included.
const ATTEMPTS: u32 = 4;
/// The wait before the second try, doubled before each one after it.
const FIRST_RETRY: Duration = Duration::from_secs(2);
/// The oracle's one-request eligible forecasts take windows of 1 to 48 whole hours.
const DISCOVERY_MAX_HOURS: i64 = 48;

/// How long to wait before trying a request again after `attempts` tries, the last of which got
/// `status`, or None for a transport error: None if it should not be tried again. A shed request
/// (503), too many requests (429) and any other server error are tried again; a refusal of the
/// request itself is not. `jitter_ms` spreads lanes that failed together.
pub fn retry_after(attempts: u32, status: Option<StatusCode>, jitter_ms: u64) -> Option<Duration> {
    let retryable = status
        .is_none_or(|status| status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error());
    (retryable && attempts < ATTEMPTS).then(|| {
        FIRST_RETRY * 2u32.pow(attempts.saturating_sub(1)) + Duration::from_millis(jitter_ms)
    })
}

/// Whether the oracle's one-request eligible forecasts take the window `start` to `end` at
/// `now`: it starts in the future, within seven days, and lasts 1 to 48 whole hours.
pub fn discovery_window_fits(
    now: OffsetDateTime,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> bool {
    let length = end - start;
    // A minute's margin, so the window is still in the future when the oracle reads it.
    start > now + time::Duration::MINUTE
        && start <= now + time::Duration::days(7)
        && length >= time::Duration::HOUR
        && length <= time::Duration::hours(DISCOVERY_MAX_HOURS)
        && length.whole_seconds() % 3600 == 0
        && length.subsec_nanoseconds() == 0
}

/// The observation evidence behind an eligible station. Missing evidence cannot authorize a pick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    pub clean_days: u32,
    pub days_checked: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_report: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub forecast_through: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub coverage_checked_at: OffsetDateTime,
    pub recent_window_hours: u32,
    pub max_report_gap_seconds: u64,
}
impl Coverage {
    pub fn current(&self, days: u32, hours: u64, now: OffsetDateTime) -> bool {
        self.days_checked == days
            && days > 0
            && self.clean_days <= days
            && self.clean_days >= days - days / 10
            && u64::from(self.recent_window_hours) == hours
            && self.coverage_checked_at <= now
            && now - self.coverage_checked_at < time::Duration::minutes(20)
            && self.last_report <= now
            && now - self.last_report < time::Duration::minutes(90)
            && self.forecast_through >= now + time::Duration::hours(hours as i64)
    }
    /// A station with no missed-report allowance used has more coverage margin.
    pub fn missed_report(&self) -> bool {
        self.max_report_gap_seconds > 90 * 60
    }
}

/// A station, including the oracle's coverage evidence when listed as eligible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StationInfo {
    pub station_id: String,
    #[serde(default)]
    pub station_name: String,
    #[serde(default)]
    pub state: String,
    /// The airport's code, for an airport people know by one.
    #[serde(default)]
    pub iata_id: Option<String>,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    #[serde(flatten)]
    pub coverage: Option<Coverage>,
}

impl StationInfo {
    /// A station known only by its id, for one the oracle does not list.
    pub fn unknown(station_id: &str) -> Self {
        Self {
            station_id: station_id.to_string(),
            station_name: String::new(),
            state: String::new(),
            iata_id: None,
            latitude: None,
            longitude: None,
            coverage: None,
        }
    }

    pub fn known_airport(&self) -> bool {
        self.iata_id
            .as_deref()
            .is_some_and(|iata| !iata.trim().is_empty())
    }

    pub fn location(&self) -> Option<(f64, f64)> {
        Some((self.latitude?, self.longitude?))
    }
}

/// One station's forecast for one period, the parts the picker scores.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Forecast {
    pub station_id: String,
    /// °F
    pub temp_low: i64,
    /// °F
    pub temp_high: i64,
    /// Knots
    #[serde(default)]
    pub wind_speed: Option<i64>,
    /// Percent
    #[serde(default)]
    pub precip_chance: Option<i64>,
    /// Inches
    #[serde(default)]
    pub rain_amt: Option<f64>,
    #[serde(default)]
    pub snow_amt: Option<f64>,
    #[serde(default)]
    pub ice_amt: Option<f64>,
}

/// The oracle's eligible stations and their forecasts, from one request.
#[derive(Debug, Deserialize)]
struct EligibleForecasts {
    stations: Vec<StationInfo>,
    forecasts: Vec<Forecast>,
}

#[derive(Clone)]
pub struct OracleClient {
    http: Client,
    base_url: String,
    /// Shared by every lane's picks, so together they keep to [`REQUESTS_AT_ONCE`].
    slots: Arc<Semaphore>,
}

impl OracleClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            http: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_string(),
            slots: Arc::new(Semaphore::new(REQUESTS_AT_ONCE)),
        }
    }

    /// GET `path` with `query`, taking one of the shared slots for each try and trying again,
    /// after a growing wait, when the oracle sheds or fails it. None if the oracle has no such
    /// route.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Option<T>> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let (status, error) = {
                let _slot = self
                    .slots
                    .acquire()
                    .await
                    .context("the oracle's request slots closed")?;
                match self
                    .http
                    .get(format!("{}{path}", self.base_url))
                    .query(query)
                    .send()
                    .await
                {
                    Ok(response) if response.status() == StatusCode::NOT_FOUND => return Ok(None),
                    Ok(response) if response.status().is_success() => {
                        return response
                            .json()
                            .await
                            .map(Some)
                            .with_context(|| format!("reading the oracle's {path}"));
                    }
                    Ok(response) => {
                        let status = response.status();
                        (
                            Some(status),
                            anyhow::anyhow!("the oracle answered {path} with {status}"),
                        )
                    }
                    Err(error) => (
                        None,
                        anyhow::Error::new(error).context(format!("asking the oracle for {path}")),
                    ),
                }
            };
            let Some(wait) = retry_after(attempts, status, rand::random_range(0..1000)) else {
                return Err(error);
            };
            log::warn!(
                "Trying {path} again in {} ms, after {attempts} tries: {error:#}",
                wait.as_millis()
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// The stations whose observations were clean on enough of the last `days` days to attest a
    /// window of `window_hours`; None if the oracle does not offer the list.
    pub async fn eligible(&self, days: u32, window_hours: u64) -> Result<Option<Vec<StationInfo>>> {
        self.get(
            "/stations/eligible",
            &[
                ("days", days.to_string()),
                ("window_hours", window_hours.to_string()),
            ],
        )
        .await
        .context("the oracle's eligible stations")
    }

    /// The eligible stations whose forecasts reach `end`, and their forecasts from `start` to
    /// `end`, in one request: the oracle judges eligibility over `days` days and the window's
    /// length. None if the oracle does not offer it, or the window is not one it takes (see
    /// [`discovery_window_fits`]); ask for the list and the forecasts apart then.
    pub async fn eligible_forecasts(
        &self,
        days: u32,
        start: OffsetDateTime,
        end: OffsetDateTime,
    ) -> Result<Option<(Vec<StationInfo>, Vec<Forecast>)>> {
        if !discovery_window_fits(OffsetDateTime::now_utc(), start, end) {
            return Ok(None);
        }
        let found: Option<EligibleForecasts> = self
            .get(
                "/stations/eligible/forecasts",
                &[
                    ("days", days.to_string()),
                    ("start", start.format(&Rfc3339)?),
                    ("end", end.format(&Rfc3339)?),
                ],
            )
            .await
            .context("the oracle's eligible stations and forecasts")?;
        Ok(found.map(|found| (found.stations, found.forecasts)))
    }

    /// Every station the oracle knows, with where it is.
    pub async fn stations(&self) -> Result<Vec<StationInfo>> {
        self.get("/stations", &[])
            .await
            .context("the oracle's stations")?
            .context("the oracle does not list its stations")
    }

    /// The forecasts for `station_ids` between `start` and `end`, asked for [`FORECAST_BATCH`]
    /// stations at a time. A batch that still fails after its retries is logged and left out, so
    /// its stations have no forecast.
    pub async fn forecasts(
        &self,
        station_ids: &[String],
        start: OffsetDateTime,
        end: OffsetDateTime,
    ) -> Result<Vec<Forecast>> {
        let (start, end) = (start.format(&Rfc3339)?, end.format(&Rfc3339)?);
        // The requests are built up front: a closure over a chunk of ids would need a lifetime
        // the stream combinators cannot express.
        let requests: Vec<_> = station_ids
            .chunks(FORECAST_BATCH)
            .map(|batch| self.forecast_batch(batch.join(","), &start, &end))
            .collect();
        let batches = futures::stream::iter(requests)
            .buffer_unordered(REQUESTS_AT_ONCE)
            .collect::<Vec<_>>()
            .await;
        let mut forecasts = Vec::new();
        for batch in batches {
            match batch {
                Ok(rows) => forecasts.extend(rows),
                Err(error) => log::warn!("Leaving out stations without a forecast: {error:#}"),
            }
        }
        Ok(forecasts)
    }

    async fn forecast_batch(&self, ids: String, start: &str, end: &str) -> Result<Vec<Forecast>> {
        self.get(
            "/stations/forecasts",
            &[
                ("station_ids", ids),
                ("start", start.to_string()),
                ("end", end.to_string()),
            ],
        )
        .await
        .context("the oracle's forecasts")?
        .context("the oracle does not offer forecasts")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn shed_and_failed_requests_are_tried_again_after_a_growing_wait() {
        let shed = Some(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry_after(1, shed, 0), Some(Duration::from_secs(2)));
        assert_eq!(retry_after(2, shed, 0), Some(Duration::from_secs(4)));
        assert_eq!(
            retry_after(3, shed, 250),
            Some(Duration::from_millis(8_250))
        );
        assert_eq!(retry_after(ATTEMPTS, shed, 0), None, "four tries at most");
        assert!(retry_after(1, Some(StatusCode::INTERNAL_SERVER_ERROR), 0).is_some());
        assert!(retry_after(1, Some(StatusCode::BAD_GATEWAY), 0).is_some());
        assert!(retry_after(1, Some(StatusCode::TOO_MANY_REQUESTS), 0).is_some());
        assert!(retry_after(1, None, 0).is_some(), "a transport error");
        assert_eq!(retry_after(1, Some(StatusCode::BAD_REQUEST), 0), None);
        assert_eq!(retry_after(1, Some(StatusCode::NOT_FOUND), 0), None);
    }

    #[test]
    fn one_request_discovery_takes_future_windows_of_up_to_two_days() {
        let now = datetime!(2026-10-03 15:42 UTC);
        let start = datetime!(2026-10-04 00:00 UTC);
        assert!(discovery_window_fits(
            now,
            start,
            start + time::Duration::hours(12)
        ));
        assert!(discovery_window_fits(
            now,
            start,
            start + time::Duration::DAY
        ));
        assert!(discovery_window_fits(
            now,
            start,
            start + time::Duration::hours(48)
        ));
        // Longer windows, odd lengths and windows already under way ask apart.
        assert!(!discovery_window_fits(
            now,
            start,
            start + time::Duration::days(3)
        ));
        assert!(!discovery_window_fits(
            now,
            start,
            start + time::Duration::hours(24) + time::Duration::SECOND
        ));
        assert!(!discovery_window_fits(now, now, now + time::Duration::DAY));
        let far = now + time::Duration::days(8);
        assert!(!discovery_window_fits(now, far, far + time::Duration::DAY));
    }

    /// Lanes asking at once share the slots: the oracle never sees more than four requests in
    /// flight, and a request it sheds is tried again rather than left out.
    #[tokio::test]
    async fn lanes_together_keep_to_four_requests_and_retry_what_is_shed() {
        use axum::{extract::State, http::StatusCode as Status, routing::get, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct Load {
            in_flight: AtomicUsize,
            most: AtomicUsize,
            seen: AtomicUsize,
        }
        async fn forecasts(
            State(load): State<Arc<Load>>,
        ) -> Result<Json<Vec<serde_json::Value>>, Status> {
            let now = load.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            load.most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            load.in_flight.fetch_sub(1, Ordering::SeqCst);
            // Shed the first request, as the oracle did at the burst.
            if load.seen.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(Status::SERVICE_UNAVAILABLE);
            }
            Ok(Json(vec![serde_json::json!({
                "station_id": "KDEN", "temp_low": 30, "temp_high": 70
            })]))
        }
        let load = Arc::new(Load::default());
        let app = Router::new()
            .route("/stations/forecasts", get(forecasts))
            .with_state(load.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let oracle = OracleClient::new(&url);
        let ids: Vec<String> = (0..FORECAST_BATCH * 3)
            .map(|i| format!("K{i:03}"))
            .collect();
        let start = OffsetDateTime::now_utc() + time::Duration::DAY;
        let end = start + time::Duration::DAY;
        let lanes =
            futures::future::join_all((0..3).map(|_| oracle.forecasts(&ids, start, end))).await;
        assert!(load.most.load(Ordering::SeqCst) <= REQUESTS_AT_ONCE);
        let rows: usize = lanes.into_iter().map(|rows| rows.unwrap().len()).sum();
        assert_eq!(rows, 9, "every batch answered, the shed one on its retry");
    }
}
