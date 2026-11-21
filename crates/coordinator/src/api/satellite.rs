//! NOAA's latest GOES-19 satellite loop of the country, the public pages' moving background
//! (`layouts/base`). The server fetches it at most every [`REFRESH_AFTER`] and serves its own
//! copy, so visitors share one download and never contact NOAA.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use crate::{startup::AppState, templates::assets::serve_bytes};

pub const SATELLITE_PATH: &str = "/satellite.mp4";

/// Band 7, shortwave infrared, which shows clouds by day and night. About four hours of frames;
/// NOAA renews the file about hourly.
const SOURCE: &str =
    "https://cdn.star.nesdis.noaa.gov/GOES19/ABI/CONUS/07/GOES19-CONUS-07-625x375.mp4";
const REFRESH_AFTER: Duration = Duration::from_secs(10 * 60);
/// How long a request waits for a due download before it takes the older copy, so the first
/// visitor after a quiet night never gets the night's loop.
const WAIT: Duration = Duration::from_secs(20);
/// NOAA's loop is about 3.6 MB.
const MAX_BYTES: usize = 20_000_000;

/// The cached loop, or 503 until a first download succeeds. Byte ranges are served, since
/// Safari plays a video only from a server that answers them.
pub async fn serve_satellite(State(state): State<Arc<AppState>>, request: HeaderMap) -> Response {
    let cached = state
        .satellite
        .get_fresh((), REFRESH_AFTER, WAIT, fetch)
        .await;
    match cached.value() {
        Some(video) => serve_bytes(&request, video.clone(), "video/mp4", "public, max-age=600"),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn fetch() -> anyhow::Result<Bytes> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut response = client.get(SOURCE).send().await?.error_for_status()?;
    let mut video = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        video.extend_from_slice(&chunk);
        anyhow::ensure!(
            video.len() <= MAX_BYTES,
            "NOAA's satellite loop is over {MAX_BYTES} bytes"
        );
    }
    Ok(video.into())
}
