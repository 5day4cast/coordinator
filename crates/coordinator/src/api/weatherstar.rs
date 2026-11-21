//! WeatherStar 4000+ (`vendor/ws4kp`), embedded in the binary and served under
//! [`WEATHERSTAR_PREFIX`] for the lobby's regional forecast frame.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

include!(concat!(env!("OUT_DIR"), "/weatherstar.rs"));

pub const WEATHERSTAR_PREFIX: &str = "/weatherstar/";

/// A WeatherStar file by its path, `index.html` for the folder itself. Its files keep their
/// names across versions, so browsers keep them a day rather than a year.
pub async fn serve_weatherstar(path: Option<Path<String>>) -> Response {
    let path = path.map(|Path(path)| path).unwrap_or_default();
    let path = if path.is_empty() { "index.html" } else { &path };
    match WEATHERSTAR.iter().find(|(file, ..)| *file == path) {
        Some((_, content_type, bytes)) => (
            [
                (header::CONTENT_TYPE, *content_type),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            *bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_folder_serves_its_page_and_unknown_files_are_not_found() {
        let router = Router::new()
            .route("/weatherstar/", get(serve_weatherstar))
            .route("/weatherstar/{*path}", get(serve_weatherstar));
        let get = |uri: &str| {
            router
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        };
        let page = get("/weatherstar/").await.unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(
            page.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let font = get("/weatherstar/fonts/Star4000.woff").await.unwrap();
        assert_eq!(font.headers()[header::CONTENT_TYPE], "font/woff");
        assert_eq!(
            get("/weatherstar/../Cargo.toml").await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        // No inline script: the page runs under its own strict policy.
        let html = std::str::from_utf8(
            WEATHERSTAR
                .iter()
                .find(|(file, ..)| *file == "index.html")
                .unwrap()
                .2,
        )
        .unwrap();
        for script in html.split("<script").skip(1) {
            assert!(
                script.contains(" src="),
                "inline script: <script{}",
                &script[..80.min(script.len())]
            );
        }
    }
}
