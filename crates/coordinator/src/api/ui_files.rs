//! Files served from the UI directory: the WASM package that wasm-pack builds
//! outside Cargo. Everything else the browser loads is embedded (see
//! `templates::assets`).
//!
//! tower-http's `ServeDir` does the serving: safe paths, content types,
//! conditional and range requests, and a `.gz` file beside the requested one
//! when the browser accepts gzip. Pages request the package with
//! `?v=<hash>`, the hash the server computed at startup, so a versioned
//! request may be cached for a year.

use std::{io::Read, path::Path, sync::Arc};

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue},
    middleware::{self, Next},
    response::Response,
    Router,
};
use sha2::{Digest, Sha256};
use tower_http::services::ServeDir;

/// Files that make up the browser wallet; their hash versions its URLs.
const PACKAGE_FILES: [&str; 2] = ["pkg/coordinator_wasm.js", "pkg/coordinator_wasm_bg.wasm"];

/// Hash of the WASM package in `ui_dir`, or an empty string when it is missing,
/// in which case pages request it unversioned and uncached.
pub fn package_version(ui_dir: &str) -> String {
    let mut hasher = Sha256::new();
    for file in PACKAGE_FILES {
        let Ok(mut reader) = std::fs::File::open(Path::new(ui_dir).join(file)) else {
            return String::new();
        };
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => hasher.update(&buffer[..read]),
                Err(_) => return String::new(),
            }
        }
    }
    hex::encode(hasher.finalize())[..16].to_owned()
}

/// `/ui/*`, served from `ui_dir`. `version` is [`package_version`].
pub fn router<S: Clone + Send + Sync + 'static>(ui_dir: &str, version: String) -> Router<S> {
    let files = ServeDir::new(ui_dir)
        .precompressed_gzip()
        .append_index_html_on_directories(false);
    Router::new()
        .nest_service("/ui", files)
        .layer(middleware::from_fn_with_state(
            Arc::new(version),
            cache_policy,
        ))
}

/// A request naming the package this server hashed may be cached for a year;
/// anything else is checked with the server before reuse.
async fn cache_policy(
    State(version): State<Arc<String>>,
    request: Request,
    next: Next,
) -> Response {
    let versioned =
        !version.is_empty() && request.uri().query() == Some(format!("v={version}").as_str());
    let mut response = next.run(request).await;
    if response.status().is_success() {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if versioned {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            }),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::StatusCode};
    use tower::ServiceExt;

    fn package() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("ui/pkg")).unwrap();
        std::fs::write(
            directory.path().join("ui/pkg/coordinator_wasm.js"),
            "export default async function init() {}",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("ui/pkg/coordinator_wasm_bg.wasm"),
            vec![0u8; 4096],
        )
        .unwrap();
        std::fs::write(directory.path().join("secret.txt"), "private data").unwrap();
        directory
    }

    async fn get(router: &Router, uri: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = Request::get(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        router
            .clone()
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn only_versioned_package_requests_are_cached_for_a_year() {
        let directory = package();
        let ui_dir = directory.path().join("ui");
        let ui_dir = ui_dir.to_str().unwrap();
        let version = package_version(ui_dir);
        let router: Router = router(ui_dir, version.clone());

        let response = get(
            &router,
            &format!("/ui/pkg/coordinator_wasm_bg.wasm?v={version}"),
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable"));

        for uri in [
            "/ui/pkg/coordinator_wasm_bg.wasm",
            "/ui/pkg/coordinator_wasm_bg.wasm?v=0000000000000000",
        ] {
            let response = get(&router, uri, &[]).await;
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-cache",
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn a_gzipped_copy_is_served_to_browsers_that_accept_it() {
        let directory = package();
        let ui_dir = directory.path().join("ui");
        std::fs::write(ui_dir.join("pkg/coordinator_wasm.js.gz"), b"gzipped bytes").unwrap();
        let router: Router = router(ui_dir.to_str().unwrap(), String::new());

        let gzip = get(
            &router,
            "/ui/pkg/coordinator_wasm.js",
            &[("accept-encoding", "br;q=1, gzip;q=0.5")],
        )
        .await;
        assert_eq!(gzip.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(
            to_bytes(gzip.into_body(), 100).await.unwrap(),
            "gzipped bytes"
        );

        let plain = get(
            &router,
            "/ui/pkg/coordinator_wasm.js",
            &[("accept-encoding", "gzip;q=0")],
        )
        .await;
        assert!(!plain.headers().contains_key(header::CONTENT_ENCODING));
    }

    #[tokio::test]
    async fn paths_outside_the_ui_directory_are_not_served() {
        let directory = package();
        let ui_dir = directory.path().join("ui");
        let router: Router = router(ui_dir.to_str().unwrap(), String::new());
        let secret = directory.path().join("secret.txt");
        for uri in [
            format!(
                "/ui/%2F{}",
                secret.to_str().unwrap().trim_start_matches('/')
            ),
            "/ui/pkg%2F..%2F..%2Fsecret.txt".to_owned(),
            "/ui/pkg/../../secret.txt".to_owned(),
            "/ui/pkg/".to_owned(),
        ] {
            let response = get(&router, &uri, &[]).await;
            assert!(!response.status().is_success(), "{uri}");
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            assert!(!String::from_utf8_lossy(&body).contains("private data"));
        }
    }

    #[test]
    fn the_package_version_follows_its_files() {
        let directory = package();
        let ui_dir = directory.path().join("ui");
        let ui_dir = ui_dir.to_str().unwrap();
        let version = package_version(ui_dir);
        assert_eq!(version.len(), 16);
        std::fs::write(
            Path::new(ui_dir).join("pkg/coordinator_wasm_bg.wasm"),
            [1u8; 8],
        )
        .unwrap();
        assert_ne!(package_version(ui_dir), version);
        assert_eq!(package_version("/nonexistent"), "");
    }
}
