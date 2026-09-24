//! The dashboard's stylesheet and script, bundled by build.rs and served at content-hashed URLs
//! that browsers may cache for good.

use axum::{
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const CACHE_POLICY: &str = "public, max-age=31536000, immutable";

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(
            CSS_URL,
            get(|| async { asset("text/css; charset=utf-8", CSS_BYTES) }),
        )
        .route(
            JS_URL,
            get(|| async { asset("text/javascript; charset=utf-8", JS_BYTES) }),
        )
}

fn asset(content_type: &'static str, bytes: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, CACHE_POLICY),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn get(url: &str) -> Response {
        router::<()>()
            .oneshot(Request::get(url).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_bundles_are_served_at_their_hashed_urls_for_good() {
        for (url, bytes) in [(CSS_URL, CSS_BYTES), (JS_URL, JS_BYTES)] {
            let response = get(url).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], CACHE_POLICY);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(body.as_ref(), bytes);
        }
        assert_eq!(
            get("/assets/synth.css").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    /// htmx and its SSE extension come first and whole, then synth's own script, minified.
    #[test]
    fn the_script_carries_htmx_its_sse_extension_and_the_copy_button() {
        let js = std::str::from_utf8(JS_BYTES).unwrap();
        let htmx = js.find("var htmx=").expect("htmx is bundled");
        let sse = js
            .find("defineExtension(\"sse\"")
            .expect("the SSE extension is bundled");
        let copy = js.find("clipboard").expect("the copy button is bundled");
        assert!(htmx < sse && sse < copy);
        let css = std::str::from_utf8(CSS_BYTES).unwrap();
        assert!(
            css.contains(".flow{"),
            "minified: {}",
            &css[..80.min(css.len())]
        );
    }
}
