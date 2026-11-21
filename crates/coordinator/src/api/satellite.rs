//! NOAA's latest GOES-19 satellite loop of the country, the public pages' moving background
//! (`layouts/base`). The server fetches it at most every [`REFRESH_AFTER`] and serves its own
//! copy, so visitors share one download and never contact NOAA.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::State,
    http::{
        header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH, IF_RANGE, RANGE},
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

use crate::{startup::AppState, templates::assets::serve_bytes};

pub const SATELLITE_PATH: &str = "/satellite.mp4";

/// Band 7, shortwave infrared, which shows clouds by day and night. About four hours of frames;
/// NOAA renews the file about hourly.
const SOURCE: &str =
    "https://cdn.star.nesdis.noaa.gov/GOES19/ABI/CONUS/07/GOES19-CONUS-07-625x375.mp4";
const REFRESH_AFTER: Duration = Duration::from_secs(30 * 60);
/// Browsers keep their copy as long as the server keeps its own, then ask whether it changed.
const CACHE_POLICY: &str = "public, max-age=1800";
/// How long a request waits for a due download before it takes the older copy, so the first
/// visitor after a quiet night never gets the night's loop.
const WAIT: Duration = Duration::from_secs(20);
/// NOAA's loop is about 3.6 MB.
const MAX_BYTES: usize = 20_000_000;

/// A download of the loop. Its ETag is a hash of its bytes, so a copy fetched again unchanged
/// keeps the tag browsers already hold.
pub struct Loop {
    video: Bytes,
    etag: HeaderValue,
}

impl Loop {
    fn new(video: Bytes) -> anyhow::Result<Self> {
        let hash = Sha256::digest(&video);
        let etag = HeaderValue::from_str(&format!("\"{}\"", hex::encode(&hash[..8])))?;
        Ok(Self { video, etag })
    }
}

/// The cached loop, or 503 until a first download succeeds.
pub async fn serve_satellite(State(state): State<Arc<AppState>>, request: HeaderMap) -> Response {
    let cached = state
        .satellite
        .get_fresh((), REFRESH_AFTER, WAIT, fetch)
        .await;
    match cached.value() {
        Some(video) => respond(request, video),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// 304 to a browser holding this copy. Otherwise the loop, or the byte range asked for, since
/// Safari plays a video only from a server that answers them.
fn respond(mut request: HeaderMap, video: &Loop) -> Response {
    let holds = |header| {
        request
            .get(header)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|tags| {
                tags.split(',')
                    .map(str::trim)
                    .any(|tag| tag.as_bytes() == video.etag.as_bytes())
            })
    };
    if holds(IF_NONE_MATCH) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (ETAG, video.etag.clone()),
                (CACHE_CONTROL, HeaderValue::from_static(CACHE_POLICY)),
            ],
        )
            .into_response();
    }
    // A range to finish an older copy would splice two loops; that browser gets this one whole.
    if request.contains_key(IF_RANGE) && !holds(IF_RANGE) {
        request.remove(RANGE);
    }
    let mut response = serve_bytes(&request, video.video.clone(), "video/mp4", CACHE_POLICY);
    response.headers_mut().insert(ETAG, video.etag.clone());
    response
}

async fn fetch() -> anyhow::Result<Loop> {
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
    Loop::new(video.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_holding_the_copy_gets_304_and_an_older_copy_is_never_finished() {
        let video = Loop::new(Bytes::from_static(b"0123456789")).unwrap();
        let tag = video.etag.to_str().unwrap().to_owned();
        let ask = |headers: &[(axum::http::HeaderName, &str)]| {
            let mut request = HeaderMap::new();
            for (name, value) in headers {
                request.insert(name, HeaderValue::from_str(value).unwrap());
            }
            respond(request, &video)
        };

        let whole = ask(&[]);
        assert_eq!(whole.status(), StatusCode::OK);
        assert_eq!(whole.headers()[ETAG], tag.as_str());

        let held = ask(&[(IF_NONE_MATCH, format!("\"older\", {tag}").as_str())]);
        assert_eq!(held.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(held.headers()[CACHE_CONTROL], CACHE_POLICY);
        assert_eq!(
            ask(&[(IF_NONE_MATCH, "\"older\"")]).status(),
            StatusCode::OK
        );

        let rest = ask(&[(RANGE, "bytes=4-"), (IF_RANGE, tag.as_str())]);
        assert_eq!(rest.status(), StatusCode::PARTIAL_CONTENT);
        let older = ask(&[(RANGE, "bytes=4-"), (IF_RANGE, "\"older\"")]);
        assert_eq!(older.status(), StatusCode::OK);
        assert_eq!(older.headers()[ETAG], tag.as_str());
    }
}
