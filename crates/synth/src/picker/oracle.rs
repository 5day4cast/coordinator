//! What the picker asks the oracle: which stations it can attest, where they are, and their
//! forecasts.

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Stations named in one forecast request, well under the oracle's limit and a URL's length.
pub const FORECAST_BATCH: usize = 50;
/// Forecast requests in flight at once.
const FORECAST_REQUESTS_AT_ONCE: usize = 4;

/// A station, as the oracle lists it. The eligible list says more; only this is used.
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

#[derive(Clone)]
pub struct OracleClient {
    http: Client,
    base_url: String,
}

impl OracleClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            http: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// The stations whose observations were clean on enough of the last `days` days to attest a
    /// window of `window_hours`; None if the oracle does not offer the list.
    pub async fn eligible(&self, days: u32, window_hours: u64) -> Result<Option<Vec<StationInfo>>> {
        let response = self
            .http
            .get(format!("{}/stations/eligible", self.base_url))
            .query(&[("days", days as u64), ("window_hours", window_hours)])
            .send()
            .await
            .context("asking the oracle for its eligible stations")?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let stations = response
            .error_for_status()
            .context("the oracle's eligible stations")?
            .json()
            .await
            .context("reading the oracle's eligible stations")?;
        Ok(Some(stations))
    }

    /// Every station the oracle knows, with where it is.
    pub async fn stations(&self) -> Result<Vec<StationInfo>> {
        self.http
            .get(format!("{}/stations", self.base_url))
            .send()
            .await
            .context("asking the oracle for its stations")?
            .error_for_status()
            .context("the oracle's stations")?
            .json()
            .await
            .context("reading the oracle's stations")
    }

    /// The forecasts for `station_ids` between `start` and `end`, asked for [`FORECAST_BATCH`]
    /// stations at a time. A batch that fails is logged and left out, so its stations have no
    /// forecast.
    pub async fn forecasts(
        &self,
        station_ids: &[String],
        start: OffsetDateTime,
        end: OffsetDateTime,
    ) -> Result<Vec<Forecast>> {
        let (start, end) = (start.format(&Rfc3339)?, end.format(&Rfc3339)?);
        let batches = futures::stream::iter(station_ids.chunks(FORECAST_BATCH))
            .map(|batch| self.forecast_batch(batch.join(","), &start, &end))
            .buffer_unordered(FORECAST_REQUESTS_AT_ONCE)
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
        self.http
            .get(format!("{}/stations/forecasts", self.base_url))
            .query(&[
                ("station_ids", ids.as_str()),
                ("start", start),
                ("end", end),
            ])
            .send()
            .await
            .context("asking the oracle for forecasts")?
            .error_for_status()
            .context("the oracle's forecasts")?
            .json()
            .await
            .context("reading the oracle's forecasts")
    }
}
