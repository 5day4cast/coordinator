//! `POST /api/v1/telemetry`: browser events from `shared/telemetry.js`, written as one
//! `ui_event …` log line each. See docs/REQUEST_CONTEXT.md.
//!
//! Nothing is stored. The body is parsed into a fixed set of fields, every string is scrubbed
//! of anything that looks like a key, an invoice or an email address, and the volume is capped
//! per browser tab and for the whole service.
use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::State,
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
};
use log::info;
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    api::request_context::{log_value, valid_request_id, valid_session_id, RequestContext},
    metrics::TELEMETRY_EVENTS_DROPPED,
    startup::AppState,
};

/// The largest body the endpoint reads; larger ones get 413.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// Events after the first 50 of a batch are dropped.
pub const MAX_EVENTS_PER_BATCH: usize = 50;
/// Events one browser tab may send per hour.
pub const MAX_EVENTS_PER_SESSION: u32 = 500;
const SESSION_WINDOW: Duration = Duration::from_secs(3600);
/// Events per second the whole service accepts, and the burst above it.
pub const GLOBAL_EVENTS_PER_SECOND: f64 = 50.0;
/// Sessions tracked at once; a new tab beyond this has its events dropped until old
/// windows expire.
const MAX_SESSIONS: usize = 50_000;
/// `text` on clicks is cut to this many characters.
const MAX_CLICK_TEXT_CHARS: usize = 40;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether pages render `<meta name="telemetry" content="on">` and the endpoint logs events.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Set once at start-up from `[telemetry] enabled`.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

#[derive(Deserialize)]
struct Batch {
    sid: String,
    #[serde(default)]
    rid: Option<String>,
    #[serde(default)]
    events: Vec<Event>,
}

/// Every field any event type has. Unknown fields are ignored; each type logs only its own.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Event {
    ev: String,
    t: Option<f64>,
    page: Option<String>,
    #[serde(rename = "ref")]
    referrer: Option<String>,
    ttfb: Option<f64>,
    dcl: Option<f64>,
    load: Option<f64>,
    lcp: Option<f64>,
    cls: Option<f64>,
    inp: Option<f64>,
    el: Option<String>,
    id: Option<String>,
    track: Option<String>,
    text: Option<String>,
    form: Option<String>,
    verb: Option<String>,
    path: Option<String>,
    status: Option<f64>,
    ms: Option<f64>,
    rid: Option<String>,
    msg: Option<String>,
    src: Option<String>,
    line: Option<f64>,
    name: Option<String>,
}

pub async fn post_telemetry(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !enabled() {
        return StatusCode::NO_CONTENT;
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let essence = content_type.split(';').next().unwrap_or("").trim();
    if !essence.eq_ignore_ascii_case("application/json")
        && !essence.eq_ignore_ascii_case("text/plain")
    {
        return StatusCode::BAD_REQUEST;
    }
    let Ok(batch) = serde_json::from_slice::<Batch>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    if !valid_session_id(&batch.sid) {
        return StatusCode::BAD_REQUEST;
    }
    let page_rid = batch
        .rid
        .as_deref()
        .filter(|rid| valid_request_id(rid))
        .unwrap_or("-");

    let total = batch.events.len();
    let wanted = total.min(MAX_EVENTS_PER_BATCH);
    let admitted = state
        .telemetry_caps
        .admit(&batch.sid, wanted, Instant::now());
    let mut dropped = total - admitted;
    for event in batch.events.into_iter().take(admitted) {
        match ui_event_line(&batch.sid, page_rid, &context.ip.to_string(), &event) {
            Some(line) => info!(target: "ui_event", "{line}"),
            None => dropped += 1,
        }
    }
    if dropped > 0 {
        TELEMETRY_EVENTS_DROPPED.inc_by(dropped as u64);
    }
    StatusCode::NO_CONTENT
}

/// `ui_event site=5day4cast rid=… sid=… ip=… ev=… page=… k=v …`, or `None` for an event type
/// the endpoint does not know.
fn ui_event_line(sid: &str, page_rid: &str, ip: &str, event: &Event) -> Option<String> {
    let mut line = format!(
        "ui_event site=5day4cast rid={} sid={} ip={} ev={} page={}",
        log_value(page_rid),
        log_value(sid),
        ip,
        log_value(&event.ev),
        text_field(&path_only(event.page.as_deref().unwrap_or("-"))),
    );
    number(&mut line, "t", event.t);
    match event.ev.as_str() {
        "page_view" => {
            number(&mut line, "ttfb", event.ttfb);
            number(&mut line, "dcl", event.dcl);
            number(&mut line, "load", event.load);
            push(&mut line, "ref", event.referrer.as_deref().map(path_only));
        }
        "vitals" => {
            number(&mut line, "lcp", event.lcp);
            number(&mut line, "inp", event.inp);
            if let Some(cls) = event.cls.filter(|cls| cls.is_finite()) {
                let _ = write!(line, " cls={:.3}", cls.clamp(0.0, 1000.0));
            }
        }
        "click" => {
            push(&mut line, "el", event.el.as_deref().map(Into::into));
            push(&mut line, "id", event.id.as_deref().map(Into::into));
            push(&mut line, "track", event.track.as_deref().map(Into::into));
            let text = event
                .text
                .as_deref()
                .map(|text| text.chars().take(MAX_CLICK_TEXT_CHARS).collect::<String>());
            push(&mut line, "text", text.map(Into::into));
        }
        "submit" => push(&mut line, "form", event.form.as_deref().map(Into::into)),
        "htmx" => {
            push(&mut line, "verb", event.verb.as_deref().map(Into::into));
            push(&mut line, "path", event.path.as_deref().map(path_only));
            number(&mut line, "status", event.status);
            number(&mut line, "ms", event.ms);
            // The response's own id; `rid=` already names the page.
            let rid = event.rid.as_deref().filter(|rid| valid_request_id(rid));
            push(&mut line, "hx_rid", rid.map(Into::into));
        }
        "js_error" => {
            push(&mut line, "msg", event.msg.as_deref().map(Into::into));
            let src = event.src.as_deref().map(|src| {
                let src = path_only(src);
                src.rsplit('/').next().unwrap_or_default().to_owned().into()
            });
            push(&mut line, "src", src);
            number(&mut line, "line", event.line);
        }
        "mark" => push(&mut line, "name", event.name.as_deref().map(Into::into)),
        _ => return None,
    }
    Some(line)
}

fn number(line: &mut String, key: &str, value: Option<f64>) {
    if let Some(value) = value.filter(|value| value.is_finite()) {
        let _ = write!(line, " {key}={}", value.round() as i64);
    }
}

fn push(line: &mut String, key: &str, value: Option<std::borrow::Cow<'_, str>>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        let _ = write!(line, " {key}={}", text_field(&value));
    }
}

/// A scrubbed, quoted log value.
fn text_field(value: &str) -> String {
    log_value(&scrub(value)).into_owned()
}

/// The part of a URL or path before any query or fragment.
fn path_only(value: &str) -> std::borrow::Cow<'_, str> {
    let end = value.find(['?', '#']).unwrap_or(value.len());
    value[..end].into()
}

/// `[redacted]` for anything that looks like a key, an invoice, an LNURL, a long hex string
/// or an email address; the value itself otherwise.
pub fn scrub(value: &str) -> std::borrow::Cow<'_, str> {
    const MARKERS: &[&str] = &[
        "nsec1", "npub1", "lnbc", "lntb", "lntbs", "lnurl", "xprv", "tprv",
    ];
    let lower = value.to_ascii_lowercase();
    let mut hex_run = 0;
    let long_hex = value.chars().any(|c| {
        hex_run = if c.is_ascii_hexdigit() {
            hex_run + 1
        } else {
            0
        };
        hex_run >= 40
    });
    if long_hex
        || MARKERS.iter().any(|marker| lower.contains(marker))
        || value.split_whitespace().any(looks_like_email)
    {
        "[redacted]".into()
    } else {
        value.into()
    }
}

fn looks_like_email(word: &str) -> bool {
    let Some((local, domain)) = word.rsplit_once('@') else {
        return false;
    };
    let domain = domain.trim_end_matches(|c: char| !c.is_alphanumeric());
    !local.is_empty()
        && domain
            .split_once('.')
            .is_some_and(|(name, rest)| !name.is_empty() && !rest.is_empty())
}

/// The per-session and global event caps.
pub struct TelemetryCaps {
    state: Mutex<CapsState>,
}

struct CapsState {
    sessions: HashMap<String, (Instant, u32)>,
    tokens: f64,
    refilled: Instant,
}

impl Default for TelemetryCaps {
    fn default() -> Self {
        Self {
            state: Mutex::new(CapsState {
                sessions: HashMap::new(),
                tokens: GLOBAL_EVENTS_PER_SECOND,
                refilled: Instant::now(),
            }),
        }
    }
}

impl TelemetryCaps {
    /// How many of `wanted` events from `sid` may be logged at `now`.
    pub fn admit(&self, sid: &str, wanted: usize, now: Instant) -> usize {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        let elapsed = now.saturating_duration_since(state.refilled).as_secs_f64();
        state.tokens =
            (state.tokens + elapsed * GLOBAL_EVENTS_PER_SECOND).min(GLOBAL_EVENTS_PER_SECOND);
        state.refilled = now;

        if !state.sessions.contains_key(sid) && state.sessions.len() >= MAX_SESSIONS {
            state
                .sessions
                .retain(|_, (start, _)| now.saturating_duration_since(*start) < SESSION_WINDOW);
            if state.sessions.len() >= MAX_SESSIONS {
                return 0;
            }
        }
        let tokens = state.tokens.floor() as usize;
        let session = state.sessions.entry(sid.to_owned()).or_insert((now, 0));
        if now.saturating_duration_since(session.0) >= SESSION_WINDOW {
            *session = (now, 0);
        }
        let left = MAX_EVENTS_PER_SESSION.saturating_sub(session.1) as usize;
        let admitted = wanted.min(left).min(tokens);
        session.1 += admitted as u32;
        state.tokens -= admitted as f64;
        admitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "AbCdEfGhIjKlMnOpQrStUv";

    fn event(json: &str) -> Event {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn secrets_and_contact_details_are_redacted() {
        for value in [
            "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq",
            "my npub1abc",
            "LNBC10u1pj...",
            "lntbs1500n1p",
            "LNURL1DP68GURN8GHJ7",
            "xprv9s21ZrQH143K",
            "tprv8ZgxMBicQKsP",
            &"ab".repeat(20),
            "write to someone@example.com please",
            "a.b@mail.example.org",
        ] {
            assert_eq!(scrub(value), "[redacted]", "{value}");
        }
        for value in [
            "Sign in",
            "Pay with Zeus",
            "/competitions/0192f1e0-aaaa-7bbb",
            "@handle",
            "deadbeef",
        ] {
            assert_eq!(scrub(value), value);
        }
    }

    #[test]
    fn sessions_get_500_events_an_hour() {
        let caps = TelemetryCaps::default();
        let start = Instant::now();
        let mut admitted = 0;
        // Spread over time so the global bucket is never the limit.
        for second in 0..20 {
            admitted += caps.admit(SID, 50, start + Duration::from_secs(second * 2));
        }
        assert_eq!(admitted, MAX_EVENTS_PER_SESSION as usize);
        assert_eq!(caps.admit(SID, 1, start + Duration::from_secs(60)), 0);
        // Another tab has its own allowance.
        assert_eq!(
            caps.admit(
                "ZyXwVuTsRqPoNmLkJiHgFe",
                10,
                start + Duration::from_secs(60)
            ),
            10
        );
        // A new hour starts a new window.
        assert_eq!(caps.admit(SID, 5, start + Duration::from_secs(3700)), 5);
    }

    #[test]
    fn the_whole_service_takes_50_events_a_second() {
        let caps = TelemetryCaps::default();
        let start = Instant::now();
        let mut admitted = 0;
        for tab in 0..10 {
            admitted += caps.admit(&format!("{SID}{tab}"), 50, start);
        }
        assert_eq!(admitted, 50);
        let later = start + Duration::from_millis(500);
        assert_eq!(caps.admit("ZyXwVuTsRqPoNmLkJiHgFe", 50, later), 25);
    }

    #[test]
    fn lines_carry_only_known_fields() {
        let line = ui_event_line(
            SID,
            "rid-12345678",
            "203.0.113.7",
            &event(
                r#"{"ev":"click","t":1234.6,"page":"/entries?id=1","el":"button","id":"loginBtn",
                    "text":"Sign in to your account and look at everything here","value":"secret"}"#,
            ),
        )
        .unwrap();
        assert_eq!(
            line,
            "ui_event site=5day4cast rid=rid-12345678 sid=AbCdEfGhIjKlMnOpQrStUv \
             ip=203.0.113.7 ev=click page=/entries t=1235 el=button id=loginBtn \
             text=\"Sign in to your account and look at ever\""
        );

        let line = ui_event_line(
            SID,
            "-",
            "::1",
            &event(
                r#"{"ev":"htmx","t":5,"page":"/","verb":"GET","path":"/entries?x=1",
                    "status":200,"ms":37.2,"rid":"0192f1e0-aaaa-7bbb-8ccc-123456789abc"}"#,
            ),
        )
        .unwrap();
        assert!(line.ends_with(
            "ev=htmx page=/ t=5 verb=GET path=/entries status=200 ms=37 \
             hx_rid=0192f1e0-aaaa-7bbb-8ccc-123456789abc"
        ));

        let line = ui_event_line(
            SID,
            "-",
            "::1",
            &event(r#"{"ev":"vitals","page":"/","lcp":812,"cls":0.04213,"inp":96}"#),
        )
        .unwrap();
        assert!(line.ends_with("lcp=812 inp=96 cls=0.042"));

        let line = ui_event_line(
            SID,
            "-",
            "::1",
            &event(
                r#"{"ev":"js_error","page":"/","msg":"bad lnbc1xyz","src":"https://x/assets/app-1.js?v=2","line":10}"#,
            ),
        )
        .unwrap();
        assert!(line.ends_with("msg=[redacted] src=app-1.js line=10"));

        assert!(ui_event_line(SID, "-", "::1", &event(r#"{"ev":"keystroke"}"#)).is_none());
    }

    #[test]
    fn batches_must_be_well_formed() {
        assert!(serde_json::from_str::<Batch>(r#"{"sid":"x","events":[]}"#).is_ok());
        assert!(serde_json::from_str::<Batch>(r#"{"events":[]}"#).is_err());
        assert!(serde_json::from_str::<Batch>(
            r#"{"sid":"x","events":[{"ev":"click","t":"soon"}]}"#
        )
        .is_err());
        let batch: Batch = serde_json::from_str(
            r#"{"sid":"x","rid":null,"extra":1,"events":[{"ev":"mark","name":"login_ok"}]}"#,
        )
        .unwrap();
        assert_eq!(batch.events[0].name.as_deref(), Some("login_ok"));
    }
}
