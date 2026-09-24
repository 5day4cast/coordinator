//! Browser assets bundled by `build.rs` and embedded in the binary.
//!
//! Each asset's URL contains a hash of its bytes, so a URL never changes
//! meaning and browsers may cache it for a year. A request for any other
//! `/assets/` path, including an old hash, is not found.

use axum::{
    extract::Path,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

/// One embedded file: its hashed URL, type, and bytes (plain and gzipped).
pub struct Asset {
    pub url: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
    pub gzip: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const CACHE_POLICY: &str = "public, max-age=31536000, immutable";

/// The asset served at `/assets/{file}`.
pub fn find(file: &str) -> Option<&'static Asset> {
    ALL.iter()
        .find(|asset| asset.url.strip_prefix("/assets/") == Some(file))
}

pub async fn serve_asset(Path(file): Path<String>, headers: HeaderMap) -> Response {
    let Some(asset) = find(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let accepts_gzip = crate::api::ui_files::accepts_gzip(&headers);
    let mut response = if accepts_gzip {
        ([(header::CONTENT_ENCODING, "gzip")], asset.gzip).into_response()
    } else {
        asset.bytes.into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(CACHE_POLICY),
    );
    headers.insert(
        header::VARY,
        header::HeaderValue::from_static("accept-encoding"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, routing::get, Router};
    use std::io::Read;
    use tower::ServiceExt;

    fn router() -> Router {
        Router::new().route("/assets/{file}", get(serve_asset))
    }

    async fn get_asset(url: &str, gzip: bool) -> Response {
        let mut request = axum::http::Request::get(url);
        if gzip {
            request = request.header(header::ACCEPT_ENCODING, "br, gzip");
        }
        router()
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn hashed_urls_serve_embedded_bytes_for_a_year() {
        for asset in ALL {
            assert!(!asset.bytes.is_empty(), "{} is empty", asset.url);
            let response = get_asset(asset.url, false).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], asset.content_type);
            assert_eq!(response.headers()[header::CACHE_CONTROL], CACHE_POLICY);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(body.as_ref(), asset.bytes);
        }
    }

    #[tokio::test]
    async fn gzip_is_served_to_browsers_that_accept_it() {
        let response = get_asset(APP_JS.url, true).await;
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(body.as_ref())
            .read_to_end(&mut plain)
            .unwrap();
        assert_eq!(plain, APP_JS.bytes);
    }

    #[tokio::test]
    async fn unknown_hashes_and_unhashed_names_are_not_found() {
        for url in [
            "/assets/app.js",
            "/assets/app.0000000000000000.js",
            "/assets/styles.css",
        ] {
            let response = get_asset(url, false).await;
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
