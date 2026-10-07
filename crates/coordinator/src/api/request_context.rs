//! Per-request ids, client addresses and the one `http …` line each request logs.
//! See docs/REQUEST_CONTEXT.md.
//!
//! The middleware builds a [`RequestContext`] for every request, stores it as a request
//! extension, and runs the rest of the request inside [`REQUEST_CONTEXT`], so the logger
//! appends ` rid=<id>` to every line written while handling it and outbound calls to our own
//! services carry it as `X-Parent-Request-Id`.
use std::{
    borrow::Cow,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Instant,
};

use axum::{
    extract::{ConnectInfo, FromRequestParts, MatchedPath, Request, State},
    http::{request::Parts, HeaderMap, HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};
use log::info;

use crate::config::HttpContextSettings;

/// Set by the edge proxy, echoed on every response.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");
/// The caller's request id on calls between our own services.
pub const PARENT_REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-parent-request-id");
/// The browser tab's session id, sent by the beacon and by htmx requests.
pub const SESSION_ID_HEADER: HeaderName = HeaderName::from_static("x-session-id");

/// Log targets whose lines carry their own `rid=` field, so the logger adds none.
pub const OWN_RID_TARGETS: &[&str] = &["http", "ui_event", "feedback"];

/// Values in log lines are cut to this many characters.
const MAX_LOG_VALUE_CHARS: usize = 200;

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub rid: String,
    pub prid: Option<String>,
    pub sid: Option<String>,
    pub ip: IpAddr,
    // shared cell the http line reads when the handler is done
    pub user: Arc<Mutex<Option<String>>>,
}

tokio::task_local! {
    pub static REQUEST_CONTEXT: RequestContext;
}

/// The context of the request being handled, or `None` outside a request.
pub fn current() -> Option<RequestContext> {
    REQUEST_CONTEXT.try_with(Clone::clone).ok()
}

/// The current request id, or `None` outside a request.
pub fn current_rid() -> Option<String> {
    REQUEST_CONTEXT.try_with(|context| context.rid.clone()).ok()
}

/// Note the authenticated user for the request's `http` line. Only the first 16 hex
/// characters of the public key are kept, and the first user recorded wins.
pub fn record_user(pubkey_hex: &str) {
    let prefix: String = pubkey_hex.chars().take(16).collect();
    if prefix.len() != 16 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return;
    }
    let _ = REQUEST_CONTEXT.try_with(|context| {
        if let Ok(mut user) = context.user.lock() {
            user.get_or_insert(prefix.to_ascii_lowercase());
        }
    });
}

impl<S: Send + Sync> FromRequestParts<S> for RequestContext {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<RequestContext>()
            .cloned()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Parsed `[http_context]` settings: which peers may vouch for the client address and
/// request id, and the header that carries the address.
#[derive(Clone, Debug)]
pub struct HttpContext {
    trusted_proxies: Vec<Cidr>,
    client_ip_header: HeaderName,
}

impl HttpContext {
    pub fn from_settings(settings: &HttpContextSettings) -> Result<Self, anyhow::Error> {
        settings.validate()?;
        Ok(Self {
            trusted_proxies: settings
                .trusted_proxies
                .iter()
                .map(|cidr| Cidr::parse(cidr))
                .collect::<Result<_, _>>()?,
            client_ip_header: HeaderName::try_from(settings.client_ip_header.trim())?,
        })
    }

    fn trusts(&self, peer: IpAddr) -> bool {
        let peer = peer.to_canonical();
        self.trusted_proxies.iter().any(|cidr| cidr.contains(peer))
    }

    /// The context for a request from `peer` with `headers`.
    pub fn context(&self, peer: IpAddr, headers: &HeaderMap) -> RequestContext {
        let trusted = self.trusts(peer);
        let rid = trusted
            .then(|| header(headers, &REQUEST_ID_HEADER).filter(|id| valid_request_id(id)))
            .flatten()
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let ip = trusted
            .then(|| header(headers, &self.client_ip_header))
            .flatten()
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
            .unwrap_or(peer)
            .to_canonical();
        RequestContext {
            rid,
            prid: header(headers, &PARENT_REQUEST_ID_HEADER)
                .filter(|id| valid_request_id(id))
                .map(str::to_owned),
            sid: header(headers, &SESSION_ID_HEADER)
                .filter(|id| valid_session_id(id))
                .map(str::to_owned),
            ip,
            user: Arc::new(Mutex::new(None)),
        }
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `^[0-9A-Za-z-]{8,64}$`
pub fn valid_request_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `^[A-Za-z0-9_-]{16,32}$`
pub fn valid_session_id(id: &str) -> bool {
    (16..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An address range from `[http_context] trusted_proxies`: `10.0.0.0/8`, `::1/128`, or a bare
/// address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(text: &str) -> Result<Self, anyhow::Error> {
        let text = text.trim();
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let network: IpAddr = address
            .parse()
            .map_err(|_| anyhow::anyhow!("{text:?} is not an address or CIDR range"))?;
        let bits = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= bits)
                .ok_or_else(|| anyhow::anyhow!("{text:?} has an invalid prefix length"))?,
            None => bits,
        };
        Ok(Self {
            network: network.to_canonical(),
            prefix,
        })
    }

    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.network, address.to_canonical()) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(network) & mask == u32::from(address) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(network) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}

/// Builds the request context, runs the request inside it, echoes `X-Request-Id`, and writes
/// one `http …` line after the response.
///
/// The peer address comes from `ConnectInfo`. A router driven without it (tests) uses the
/// unspecified address.
pub async fn request_context_middleware(
    State(settings): State<Arc<HttpContext>>,
    mut req: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let context = settings.context(peer, req.headers());
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned());
    req.extensions_mut().insert(context.clone());

    let mut response = REQUEST_CONTEXT.scope(context.clone(), next.run(req)).await;

    if let Ok(value) = HeaderValue::from_str(&context.rid) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    if logged_path(&path) {
        let user = context.user.lock().ok().and_then(|user| user.clone());
        info!(
            target: "http",
            "{}",
            http_line(
                &context,
                method.as_str(),
                route.as_deref().unwrap_or(&path),
                response.status().as_u16(),
                started.elapsed().as_millis(),
                user.as_deref(),
            )
        );
    }
    response
}

/// Metrics, health checks and static assets get no `http` line.
pub fn logged_path(path: &str) -> bool {
    !(path == "/metrics"
        || path.starts_with("/health")
        || path.starts_with("/api/v1/health")
        || path.starts_with("/assets/")
        || path.starts_with("/ui/pkg/")
        || path.starts_with("/static/"))
}

/// `http rid=… prid=… sid=… ip=… method=… route=… status=… ms=… user=…`
pub fn http_line(
    context: &RequestContext,
    method: &str,
    route: &str,
    status: u16,
    ms: u128,
    user: Option<&str>,
) -> String {
    format!(
        "http rid={} prid={} sid={} ip={} method={} route={} status={} ms={} user={}",
        log_value(&context.rid),
        log_value(context.prid.as_deref().unwrap_or("-")),
        log_value(context.sid.as_deref().unwrap_or("-")),
        context.ip,
        log_value(method),
        log_value(route),
        status,
        ms,
        log_value(user.unwrap_or("-")),
    )
}

/// A value for a `key=value` log field: control characters stripped, at most 200 characters,
/// and double-quoted (with `"` and `\` escaped) when it holds a space, `"` or `=`.
pub fn log_value(value: &str) -> Cow<'_, str> {
    let needs_quotes = |value: &str| value.is_empty() || value.contains([' ', '"', '=']);
    let value: Cow<'_, str> = if value.chars().any(char::is_control)
        || value.chars().nth(MAX_LOG_VALUE_CHARS).is_some()
    {
        Cow::Owned(
            value
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_LOG_VALUE_CHARS)
                .collect(),
        )
    } else {
        Cow::Borrowed(value)
    };
    if !needs_quotes(&value) {
        return value;
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    Cow::Owned(quoted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(trusted: &[&str]) -> HttpContext {
        HttpContext::from_settings(&HttpContextSettings {
            trusted_proxies: trusted.iter().map(|s| s.to_string()).collect(),
            ..HttpContextSettings::default()
        })
        .unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::try_from(*name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    const PROXY: &str = "10.1.2.3";
    const VISITOR: &str = "203.0.113.7";

    #[test]
    fn trusted_proxies_vouch_for_the_client_and_request_id() {
        let settings = context(&["10.0.0.0/8"]);
        let sent = headers(&[
            ("x-request-id", "0192f1e0-aaaa-7bbb-8ccc-123456789abc"),
            ("x-real-ip", VISITOR),
        ]);
        let ctx = settings.context(PROXY.parse().unwrap(), &sent);
        assert_eq!(ctx.rid, "0192f1e0-aaaa-7bbb-8ccc-123456789abc");
        assert_eq!(ctx.ip, VISITOR.parse::<IpAddr>().unwrap());

        // Anyone else gets a fresh id and their own address.
        let ctx = settings.context(VISITOR.parse().unwrap(), &sent);
        assert_ne!(ctx.rid, "0192f1e0-aaaa-7bbb-8ccc-123456789abc");
        assert!(valid_request_id(&ctx.rid));
        assert_eq!(ctx.ip, VISITOR.parse::<IpAddr>().unwrap());

        // Nobody is trusted by default.
        let ctx = context(&[]).context(PROXY.parse().unwrap(), &sent);
        assert_eq!(ctx.ip, PROXY.parse::<IpAddr>().unwrap());
    }

    #[test]
    fn forwarded_values_are_validated() {
        let settings = context(&[PROXY]);
        let sent = headers(&[
            ("x-request-id", "short"),
            ("x-real-ip", "not an address"),
            ("x-parent-request-id", "bad id!"),
            ("x-session-id", "too-short"),
            ("x-forwarded-for", VISITOR),
            ("cf-connecting-ip", VISITOR),
        ]);
        let ctx = settings.context(PROXY.parse().unwrap(), &sent);
        assert_ne!(ctx.rid, "short");
        assert_eq!(ctx.ip, PROXY.parse::<IpAddr>().unwrap());
        assert_eq!(ctx.prid, None);
        assert_eq!(ctx.sid, None);

        let sent = headers(&[
            ("x-parent-request-id", "parent-1234"),
            ("x-session-id", "AbCdEfGhIjKlMnOpQrStUv"),
        ]);
        let ctx = settings.context(VISITOR.parse().unwrap(), &sent);
        assert_eq!(ctx.prid.as_deref(), Some("parent-1234"));
        assert_eq!(ctx.sid.as_deref(), Some("AbCdEfGhIjKlMnOpQrStUv"));
    }

    #[test]
    fn ranges_match_mapped_and_plain_addresses() {
        let range = Cidr::parse("127.0.0.0/8").unwrap();
        assert!(range.contains("127.0.0.1".parse().unwrap()));
        assert!(range.contains("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!range.contains("128.0.0.1".parse().unwrap()));
        assert!(Cidr::parse("::1").unwrap().contains("::1".parse().unwrap()));
        assert!(Cidr::parse("0.0.0.0/0")
            .unwrap()
            .contains("8.8.8.8".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("proxy").is_err());
    }

    #[test]
    fn users_are_recorded_as_a_short_prefix_once() {
        let ctx = context(&[]).context(PROXY.parse().unwrap(), &HeaderMap::new());
        let cell = ctx.user.clone();
        REQUEST_CONTEXT.sync_scope(ctx, || {
            record_user("not hex at all, sixteen+");
            record_user(&"AB".repeat(32));
            record_user(&"cd".repeat(32));
        });
        assert_eq!(cell.lock().unwrap().as_deref(), Some("abababababababab"));
        // Outside a request nothing happens.
        record_user(&"ab".repeat(32));
        assert!(current().is_none());
    }

    #[test]
    fn http_lines_follow_the_contract() {
        let mut ctx = context(&[]).context(VISITOR.parse().unwrap(), &HeaderMap::new());
        ctx.rid = "rid-12345678".into();
        assert_eq!(
            http_line(&ctx, "GET", "/api/v1/competitions/{id}", 200, 37, None),
            "http rid=rid-12345678 prid=- sid=- ip=203.0.113.7 method=GET \
             route=/api/v1/competitions/{id} status=200 ms=37 user=-"
        );
        ctx.sid = Some("AbCdEfGhIjKlMnOpQrStUv".into());
        assert!(http_line(&ctx, "POST", "/x y", 500, 1, Some("abababababababab"))
            .ends_with("sid=AbCdEfGhIjKlMnOpQrStUv ip=203.0.113.7 method=POST route=\"/x y\" status=500 ms=1 user=abababababababab"));
    }

    #[test]
    fn log_values_are_quoted_stripped_and_cut() {
        assert_eq!(log_value("plain"), "plain");
        assert_eq!(log_value("two words"), "\"two words\"");
        assert_eq!(log_value("a=b"), "\"a=b\"");
        assert_eq!(log_value("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(log_value("back\\slash"), "back\\slash");
        assert_eq!(log_value("new\nline"), "newline");
        assert_eq!(log_value(""), "\"\"");
        assert_eq!(log_value(&"x".repeat(300)).chars().count(), 200);
    }

    #[test]
    fn static_and_health_paths_are_not_logged() {
        for path in [
            "/metrics",
            "/api/v1/health_check",
            "/assets/app.js",
            "/ui/pkg/x.wasm",
            "/static/a.css",
        ] {
            assert!(!logged_path(path), "{path}");
        }
        assert!(logged_path("/"));
        assert!(logged_path("/api/v1/competitions"));
    }
}
