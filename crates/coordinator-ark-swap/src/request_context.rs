//! Request ids for ark-swapd's log, as in the coordinator's `api::request_context`.
//!
//! Every request gets a fresh id, echoed in `X-Request-Id`, and one `http …` line after its
//! response. The caller's id arrives as `X-Parent-Request-Id` and is logged as `prid=`. Lines
//! logged while handling a request end with ` rid=<id>`. The client address is the TCP peer:
//! only the coordinator calls this service.
use std::{net::SocketAddr, time::Instant};

use axum::{
    extract::{ConnectInfo, MatchedPath, Request},
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");
const PARENT_REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-parent-request-id");

tokio::task_local! {
    /// The id of the request being handled.
    pub static REQUEST_ID: String;
}

/// `^[0-9A-Za-z-]{8,64}$`
fn valid_request_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub async fn request_context(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let rid = uuid::Uuid::now_v7().to_string();
    let prid = request
        .headers()
        .get(PARENT_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|id| valid_request_id(id))
        .unwrap_or("-")
        .to_owned();
    let ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip().to_canonical().to_string())
        .unwrap_or_else(|| "-".to_owned());
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned())
        .unwrap_or_else(|| path.clone());

    let mut response = REQUEST_ID.scope(rid.clone(), next.run(request)).await;

    if let Ok(value) = HeaderValue::from_str(&rid) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    if !path.starts_with("/health") {
        log::info!(
            target: "http",
            "{}",
            http_line(&rid, &prid, &ip, method.as_str(), &route, response.status().as_u16(), started.elapsed().as_millis())
        );
    }
    response
}

/// `http rid=… prid=… sid=- ip=… method=… route=… status=… ms=… user=-`. Routes are templates
/// or paths, so they hold no spaces, quotes or `=`.
fn http_line(
    rid: &str,
    prid: &str,
    ip: &str,
    method: &str,
    route: &str,
    status: u16,
    ms: u128,
) -> String {
    format!("http rid={rid} prid={prid} sid=- ip={ip} method={method} route={route} status={status} ms={ms} user=-")
}

/// The current request's id, for the logger, unless the line is the request line itself.
pub fn log_suffix(target: &str) -> Option<String> {
    if target == "http" {
        return None;
    }
    REQUEST_ID.try_with(Clone::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines_follow_the_contract() {
        assert_eq!(
            http_line(
                "rid-12345678",
                "parent-1234",
                "127.0.0.1",
                "GET",
                "/v1/swaps/{id}",
                200,
                4
            ),
            "http rid=rid-12345678 prid=parent-1234 sid=- ip=127.0.0.1 method=GET \
             route=/v1/swaps/{id} status=200 ms=4 user=-"
        );
        assert!(valid_request_id("0192f1e0-aaaa-7bbb-8ccc-123456789abc"));
        assert!(!valid_request_id("bad id!"));
        assert!(!valid_request_id("short"));
    }

    #[test]
    fn lines_inside_a_request_carry_its_id() {
        assert_eq!(log_suffix("ark_swapd::swap"), None);
        REQUEST_ID.sync_scope("rid-12345678".to_owned(), || {
            assert_eq!(
                log_suffix("ark_swapd::swap").as_deref(),
                Some("rid-12345678")
            );
            assert_eq!(log_suffix("http"), None);
        });
    }
}
