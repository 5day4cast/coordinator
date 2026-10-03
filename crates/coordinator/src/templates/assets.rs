//! Browser assets bundled by `build.rs` and embedded in the binary.
//!
//! Each asset's URL contains a hash of its bytes, so a URL never changes
//! meaning and browsers may cache it for a year. A request for any other
//! `/assets/` path, including an old hash, is not found. Compression is left
//! to the router's `CompressionLayer`. A single byte range is served as asked.

use axum::{
    extract::Path,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use std::ops::RangeInclusive;

/// One embedded file: its hashed URL, type and bytes.
pub struct Asset {
    pub url: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const CACHE_POLICY: &str = "public, max-age=31536000, immutable";

/// The asset served at `/assets/{file}`.
pub fn find(file: &str) -> Option<&'static Asset> {
    ALL.iter()
        .find(|asset| asset.url.strip_prefix("/assets/") == Some(file))
}

pub async fn serve_asset(Path(file): Path<String>, request: HeaderMap) -> Response {
    let Some(asset) = find(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = asset.bytes.len();
    let (status, body, range) = match request
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| byte_range(value, len))
    {
        None | Some(Range::Ignored) => (StatusCode::OK, asset.bytes, None),
        Some(Range::Bytes(range)) => (
            StatusCode::PARTIAL_CONTENT,
            &asset.bytes[range.clone()],
            Some(format!("bytes {}-{}/{len}", range.start(), range.end())),
        ),
        Some(Range::Unsatisfiable) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            &[][..],
            Some(format!("bytes */{len}")),
        ),
    };
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // Stated outright: a cache in front of the site serves byte ranges of a
    // stored copy only when it knows the whole length.
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    if let Some(range) = range.and_then(|range| HeaderValue::from_str(&range).ok()) {
        headers.insert(header::CONTENT_RANGE, range);
    }
    response
}

/// What a `Range` header asks of a `len`-byte asset.
#[derive(Debug, PartialEq, Eq)]
enum Range {
    Bytes(RangeInclusive<usize>),
    /// Starts past the end.
    Unsatisfiable,
    /// Not one byte range this serves, such as several ranges: the whole asset instead.
    Ignored,
}

/// `bytes=0-99`, `bytes=100-` (to the end) or `bytes=-100` (the last 100 bytes).
fn byte_range(header: &str, len: usize) -> Range {
    let Some((first, last)) = header
        .strip_prefix("bytes=")
        .filter(|spec| !spec.contains(','))
        .and_then(|spec| spec.split_once('-'))
    else {
        return Range::Ignored;
    };
    let number = |text: &str| text.trim().parse::<usize>().ok();
    let (start, end) = match (first.trim().is_empty(), number(first), number(last)) {
        (true, _, Some(suffix)) if suffix > 0 => {
            (len.saturating_sub(suffix), len.saturating_sub(1))
        }
        (false, Some(start), None) if last.trim().is_empty() => (start, len.saturating_sub(1)),
        (false, Some(start), Some(end)) if start <= end => (start, end.min(len.saturating_sub(1))),
        _ => return Range::Ignored,
    };
    if len == 0 || start >= len {
        Range::Unsatisfiable
    } else {
        Range::Bytes(start..=end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, routing::get, Router};
    use tower::ServiceExt;

    fn router() -> Router {
        Router::new().route("/assets/{file}", get(serve_asset))
    }

    async fn get_asset(url: &str) -> Response {
        router()
            .oneshot(
                axum::http::Request::get(url)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn hashed_urls_serve_embedded_bytes_for_a_year() {
        for asset in ALL {
            assert!(!asset.bytes.is_empty(), "{} is empty", asset.url);
            let response = get_asset(asset.url).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], asset.content_type);
            assert_eq!(response.headers()[header::CACHE_CONTROL], CACHE_POLICY);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(body.as_ref(), asset.bytes);
        }
    }

    #[tokio::test]
    async fn unknown_hashes_and_unhashed_names_are_not_found() {
        for url in [
            "/assets/app.js",
            "/assets/app.0000000000000000.js",
            "/assets/styles.css",
        ] {
            let response = get_asset(url).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{url}");
            assert!(!response.headers().contains_key(header::CACHE_CONTROL));
        }
    }

    #[tokio::test]
    async fn a_byte_range_is_served_as_asked() {
        let asset = &USA_MAP_SVG;
        let len = asset.bytes.len();
        let get = |range: &'static str| {
            router().oneshot(
                axum::http::Request::get(asset.url)
                    .header(header::RANGE, range)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
        };

        let first = get("bytes=0-1").await.unwrap();
        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            first.headers()[header::CONTENT_RANGE],
            format!("bytes 0-1/{len}").as_str()
        );
        assert_eq!(first.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(first.headers()[header::CONTENT_TYPE], "image/svg+xml");
        let body = to_bytes(first.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), &asset.bytes[..2]);

        let tail = get("bytes=-10").await.unwrap();
        let body = to_bytes(tail.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), &asset.bytes[len - 10..]);

        let rest = get("bytes=10-").await.unwrap();
        assert_eq!(
            rest.headers()[header::CONTENT_RANGE],
            format!("bytes 10-{}/{len}", len - 1).as_str()
        );

        let past = get("bytes=999999999-").await.unwrap();
        assert_eq!(past.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            past.headers()[header::CONTENT_RANGE],
            format!("bytes */{len}").as_str()
        );

        // Several ranges, or nonsense, get the whole asset.
        for whole in ["bytes=0-1,4-5", "items=0-1", "bytes=5-2"] {
            let response = get(whole).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{whole}");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(body.len(), len);
        }
    }

    #[test]
    fn urls_carry_a_content_hash() {
        for asset in ALL {
            let name = asset.url.strip_prefix("/assets/").unwrap();
            let hash = name.split('.').nth(1).unwrap();
            assert_eq!(hash.len(), 16, "{}", asset.url);
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}
