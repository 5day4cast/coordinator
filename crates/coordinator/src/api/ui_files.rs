//! Files served from the UI directory: the WASM package that wasm-pack builds
//! outside Cargo. Everything else the browser loads is embedded (see
//! `templates::assets`).
//!
//! Pages request the package with `?v=<hash>`, the hash the server computed at
//! startup, so a versioned request may be cached for a year. Files are kept in
//! memory with a gzipped copy, which shrinks the WASM module to about a third;
//! a file that changes on disk is read again.

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::SystemTime,
};

use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use flate2::{write::GzEncoder, Compression};
use sha2::{Digest, Sha256};

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

struct UiFile {
    modified: Option<SystemTime>,
    len: u64,
    bytes: Bytes,
    gzip: Option<Bytes>,
}

static FILES: LazyLock<Mutex<HashMap<PathBuf, Arc<UiFile>>>> = LazyLock::new(Default::default);

pub fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|coding| coding.trim().starts_with("gzip")))
}

/// Serve `path` from `ui_dir`. `versioned` marks a request carrying the
/// package hash, which may be cached for a year.
pub async fn respond(ui_dir: &str, path: &str, versioned: bool, gzip: bool) -> Response {
    // Axum percent-decodes the wildcard before extraction. An encoded leading
    // slash would make Path::join discard ui_dir, even without any '..'.
    if path.is_empty()
        || !Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return (StatusCode::BAD_REQUEST, "Bad request").into_response();
    }
    let file_path = Path::new(ui_dir).join(path);
    let file = match load(file_path).await {
        Some(file) => file,
        None => return (StatusCode::NOT_FOUND, "Not found").into_response(),
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_type(path))
        .header(header::VARY, "accept-encoding")
        .header(
            header::CACHE_CONTROL,
            if versioned {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
        );
    let body = match (&file.gzip, gzip) {
        (Some(compressed), true) => {
            builder = builder.header(header::CONTENT_ENCODING, "gzip");
            compressed.clone()
        }
        _ => file.bytes.clone(),
    };
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Server error").into_response())
}

async fn load(path: PathBuf) -> Option<Arc<UiFile>> {
    let metadata = tokio::fs::metadata(&path).await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata.modified().ok();
    if let Some(cached) = FILES.lock().ok()?.get(&path) {
        if cached.modified == modified && cached.len == metadata.len() {
            return Some(cached.clone());
        }
    }
    let bytes = Bytes::from(tokio::fs::read(&path).await.ok()?);
    let compressible = !matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "woff" | "woff2" | "ico")
    );
    let gzip = if compressible && bytes.len() > 1024 {
        let plain = bytes.clone();
        tokio::task::spawn_blocking(move || {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&plain).ok()?;
            encoder.finish().ok().map(Bytes::from)
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let file = Arc::new(UiFile {
        modified,
        len: metadata.len(),
        bytes,
        gzip,
    });
    if let Ok(mut files) = FILES.lock() {
        files.insert(path, file.clone());
    }
    Some(file)
}

fn mime_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" | "ts" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    fn package() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("pkg")).unwrap();
        std::fs::write(
            directory.path().join("pkg/coordinator_wasm.js"),
            "export default async function init() {}".repeat(100),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("pkg/coordinator_wasm_bg.wasm"),
            vec![0u8; 4096],
        )
        .unwrap();
        directory
    }

    #[tokio::test]
    async fn versioned_package_requests_are_cached_and_gzipped() {
        let directory = package();
        let ui_dir = directory.path().to_str().unwrap();
        let response = respond(ui_dir, "pkg/coordinator_wasm_bg.wasm", true, true).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable"));
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(body.as_ref())
            .read_to_end(&mut plain)
            .unwrap();
        assert_eq!(plain, vec![0u8; 4096]);

        let unversioned = respond(ui_dir, "pkg/coordinator_wasm_bg.wasm", false, false).await;
        assert_eq!(unversioned.headers()[header::CACHE_CONTROL], "no-cache");
        assert!(!unversioned.headers().contains_key(header::CONTENT_ENCODING));
    }

    #[tokio::test]
    async fn a_changed_file_is_read_again() {
        let directory = package();
        let ui_dir = directory.path().to_str().unwrap();
        let file = directory.path().join("pkg/notes.txt");
        std::fs::write(&file, "first").unwrap();
        let first = respond(ui_dir, "pkg/notes.txt", false, false).await;
        assert_eq!(to_bytes(first.into_body(), 100).await.unwrap(), "first");
        std::fs::write(&file, "second!").unwrap();
        let second = respond(ui_dir, "pkg/notes.txt", false, false).await;
        assert_eq!(to_bytes(second.into_body(), 100).await.unwrap(), "second!");
    }

    #[test]
    fn the_package_version_follows_its_files() {
        let directory = package();
        let ui_dir = directory.path().to_str().unwrap();
        let version = package_version(ui_dir);
        assert_eq!(version.len(), 16);
        std::fs::write(directory.path().join("pkg/coordinator_wasm_bg.wasm"), [1u8; 8]).unwrap();
        assert_ne!(package_version(ui_dir), version);
        assert_eq!(package_version("/nonexistent"), "");
    }
}
