//! Browser assets bundled by `build.rs` and embedded in the binary.
//!
//! Each asset's URL contains a hash of its bytes, so a URL never changes
//! meaning and browsers may cache it for a year. A request for any other
//! `/assets/` path, including an old hash, is not found. Compression is left
//! to the router's `CompressionLayer`.

use axum::{
    extract::Path,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

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

pub async fn serve_asset(Path(file): Path<String>) -> Response {
    let Some(asset) = find(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(asset.content_type),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(CACHE_POLICY),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        asset.bytes,
    )
        .into_response()
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
