//! Visitor logs for the operator's Visitors page, read from Loki's `query_range` through a
//! Grafana-shaped route. Only the fixed queries of [`Search::query`] are sent, built from
//! validated values; there is no way to send other LogQL.
//!
//! Lines come in two shapes, both parsed here:
//! - the edge proxy's access lines, JSON written by the log shipper (ip, geo, method, path,
//!   status, duration in seconds, agent, and the request id), with `site` and `client` labels;
//! - the apps' text lines: an optional `[time LEVEL] target: ` prefix, ANSI colours, and
//!   `key=value` fields, as in `http rid=… sid=… ip=… route=… status=… ms=…`,
//!   `ui_event … ev=…` and `feedback id=…`.
//!
//! Lines are grouped into sessions by `sid`, or by address and user agent without one, and
//! each session gets one time-ordered timeline.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{anyhow, ensure};
use futures::future::join_all;
use serde_json::Value;

use crate::config::LogsSettings;

/// Lines asked for per query.
pub const LINE_LIMIT: usize = 2000;
/// Bytes read of one answer.
const MAX_BODY: usize = 1024 * 1024;
/// How long a search's answer is reused.
const CACHE_TTL: Duration = Duration::from_secs(30);
const CACHE_KEYS: usize = 32;
/// The whole search, follow-up queries included.
const SEARCH_BUDGET: Duration = Duration::from_millis(4500);
/// Addresses whose edge lines are fetched for a session or user search.
const FOLLOW_UP_ADDRESSES: usize = 2;
/// A request slower than this breaks the site's load-time rule.
pub const SLOW_MS: u64 = 400;
/// Characters kept of a line that is not one of the known kinds.
const OTHER_TEXT_CHARS: usize = 300;

/// How far back a search looks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Window {
    Hour,
    SixHours,
    #[default]
    Day,
    Week,
}

impl Window {
    pub const ALL: [Window; 4] = [Self::Hour, Self::SixHours, Self::Day, Self::Week];

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|window| window.as_str() == text)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::SixHours => "6h",
            Self::Day => "24h",
            Self::Week => "7d",
        }
    }

    pub fn seconds(self) -> i64 {
        match self {
            Self::Hour => 3600,
            Self::SixHours => 6 * 3600,
            Self::Day => 24 * 3600,
            Self::Week => 7 * 24 * 3600,
        }
    }
}

/// What a search looks for. Each value is validated before it gets here.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Search {
    Rid(String),
    Sid(String),
    /// The first 16 hex characters of a public key, as `user=` logs it.
    User(String),
    Ip(IpAddr),
}

/// `^[0-9A-Za-z-]{8,64}$`, the request id format.
pub fn valid_rid(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `^[A-Za-z0-9_-]{16,32}$`, the browser session id format.
pub fn valid_sid(id: &str) -> bool {
    (16..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Search {
    pub fn rid(id: &str) -> Option<Self> {
        valid_rid(id).then(|| Self::Rid(id.to_owned()))
    }

    pub fn sid(id: &str) -> Option<Self> {
        valid_sid(id).then(|| Self::Sid(id.to_owned()))
    }

    /// From a hex public key, or its first 16 characters or more.
    pub fn user_hex(hex: &str) -> Option<Self> {
        ((16..=64).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Self::User(hex[..16].to_ascii_lowercase()))
    }

    /// The fixed query for this search.
    pub fn query(&self) -> String {
        match self {
            Self::Rid(rid) => format!(r#"{{job=~"caddy-access|application-journal"}} |= "{rid}""#),
            Self::Sid(sid) => format!(r#"{{job="application-journal"}} |= "sid={sid}""#),
            Self::User(user) => format!(r#"{{job="application-journal"}} |= "user={user}""#),
            Self::Ip(ip) => format!(r#"{{job=~"caddy-access|application-journal"}} |= "{ip}""#),
        }
    }

    /// The searched value, as the page shows it.
    pub fn value(&self) -> String {
        match self {
            Self::Rid(value) | Self::Sid(value) | Self::User(value) => value.clone(),
            Self::Ip(ip) => ip.to_string(),
        }
    }
}

/// Where a line came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The edge proxy's access log.
    Edge,
    /// An app's journal, by its `app` label.
    App(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Access,
    Http,
    UiEvent,
    Feedback,
    Other,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    /// Unix nanoseconds.
    pub ts: i64,
    pub source: Source,
    pub kind: Kind,
    /// The edge's client class label (`person`, `bot`, `lab`, …).
    pub client: Option<String>,
    pub site: Option<String>,
    pub fields: BTreeMap<String, String>,
    /// A line of another kind, cleaned and cut.
    pub text: String,
}

impl LogLine {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty() && *value != "-")
    }

    fn number(&self, key: &str) -> Option<f64> {
        self.get(key)?.parse().ok().filter(|v: &f64| v.is_finite())
    }

    /// The request's duration in milliseconds.
    pub fn ms(&self) -> Option<u64> {
        self.number("ms").map(|ms| ms.max(0.0).round() as u64)
    }

    pub fn status(&self) -> Option<u16> {
        self.get("status")?.parse().ok()
    }

    /// A line another service wrote while handling one of our requests.
    pub fn downstream(&self) -> bool {
        self.get("prid").is_some()
    }
}

/// One line from Loki, given its stream labels.
pub fn parse_line(labels: &BTreeMap<String, String>, ts: i64, line: &str) -> LogLine {
    let edge = labels.get("job").is_some_and(|job| job == "caddy-access");
    let source = if edge {
        Source::Edge
    } else {
        Source::App(
            labels
                .get("app")
                .or_else(|| labels.get("service_name"))
                .cloned()
                .unwrap_or_else(|| "app".into()),
        )
    };
    let mut parsed = LogLine {
        ts,
        source,
        kind: Kind::Other,
        client: labels.get("client").cloned(),
        site: labels.get("site").cloned(),
        fields: BTreeMap::new(),
        text: String::new(),
    };
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Some(fields) = access_fields(trimmed) {
            parsed.kind = Kind::Access;
            parsed.source = Source::Edge;
            parsed.fields = fields;
            return parsed;
        }
    }
    let (kind, fields, text) = text_line(trimmed);
    parsed.kind = kind;
    parsed.fields = fields;
    if kind == Kind::Other {
        parsed.text = text.chars().take(OTHER_TEXT_CHARS).collect();
    }
    parsed
}

/// The fields of an edge access line, under the names app lines use where they overlap.
fn access_fields(line: &str) -> Option<BTreeMap<String, String>> {
    let Value::Object(object) = serde_json::from_str::<Value>(line).ok()? else {
        return None;
    };
    let text = |value: &Value| match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    };
    let mut fields = BTreeMap::new();
    for (from, to) in [
        ("ip", "ip"),
        ("country", "country"),
        ("country_code", "country_code"),
        ("city", "city"),
        ("asn", "asn"),
        ("org", "org"),
        ("method", "method"),
        ("path", "path"),
        ("status", "status"),
        ("agent", "agent"),
        ("referrer", "referrer"),
        ("reason", "reason"),
        ("request_id", "rid"),
        ("rid", "rid"),
    ] {
        if let Some(value) = object.get(from).and_then(text).filter(|v| !v.is_empty()) {
            fields.entry(to.to_owned()).or_insert_with(|| clean(&value));
        }
    }
    // Duration in seconds, as a number or text.
    if let Some(seconds) = object
        .get("duration")
        .and_then(text)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
    {
        fields.insert("ms".into(), ((seconds * 1000.0).round() as i64).to_string());
    }
    fields.contains_key("path").then_some(fields)
}

/// Control characters out, at most 200 characters.
fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

/// `text` without ANSI escape sequences.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Parameters, then one final byte in @..~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The kind, fields and message of an app line.
fn text_line(line: &str) -> (Kind, BTreeMap<String, String>, String) {
    let line = strip_ansi(line);
    let mut rest = line.trim();
    // fern: "[2026-10-07T18:49:22.269Z INFO] target: message"
    if rest.starts_with('[') {
        if let Some(end) = rest.find("] ") {
            rest = rest[end + 2..].trim_start();
        }
    } else if rest.starts_with(|c: char| c.is_ascii_digit()) {
        // tracing: "2026-10-07T18:49:22.269Z  INFO target: message"
        let mut words = rest.splitn(3, char::is_whitespace);
        if let (Some(time), Some(_)) = (words.next(), words.next()) {
            if time.contains('T') {
                rest = rest[time.len()..].trim_start();
                rest = rest
                    .split_once(char::is_whitespace)
                    .map_or(rest, |(_, after)| after.trim_start());
            }
        }
    }
    let (target, message) = match rest.split_once(": ") {
        Some((target, message))
            if !target.is_empty()
                && target.len() <= 120
                && !target.contains(' ')
                && !target.contains('=') =>
        {
            (Some(target), message)
        }
        _ => (None, rest),
    };
    let first = message.split(' ').next().unwrap_or_default();
    let named = |name: &str| target == Some(name) || first == name;
    let fields = fields(message);
    let kind = if named("http") && fields.contains_key("rid") && fields.contains_key("status") {
        Kind::Http
    } else if named("ui_event") && fields.contains_key("ev") {
        Kind::UiEvent
    } else if named("feedback") && fields.contains_key("id") {
        Kind::Feedback
    } else {
        Kind::Other
    };
    (kind, fields, message.to_owned())
}

/// The `key=value` fields of a message. Values may be double-quoted with `\"` and `\\`
/// escapes. The first value of a key wins.
pub fn fields(message: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let bytes: Vec<char> = message.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || matches!(bytes[i - 1], ' ' | '{' | ',' | '(');
        if !boundary || !(bytes[i].is_ascii_alphabetic() || bytes[i] == '_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != '=' || i - start > 32 {
            continue;
        }
        let key: String = bytes[start..i].iter().collect();
        i += 1;
        let mut value = String::new();
        if bytes.get(i) == Some(&'"') {
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    '\\' if i + 1 < bytes.len() => {
                        value.push(bytes[i + 1]);
                        i += 2;
                    }
                    '"' => {
                        i += 1;
                        break;
                    }
                    c => {
                        value.push(c);
                        i += 1;
                    }
                }
            }
        } else {
            while i < bytes.len() && !bytes[i].is_whitespace() {
                value.push(bytes[i]);
                i += 1;
            }
            // A span's closing brace, and what follows it, is not part of its last value.
            let mut depth = 0usize;
            if let Some(end) = value.char_indices().find_map(|(at, c)| match c {
                '{' => {
                    depth += 1;
                    None
                }
                '}' if depth == 0 => Some(at),
                '}' => {
                    depth -= 1;
                    None
                }
                _ => None,
            }) {
                value.truncate(end);
            }
        }
        fields.entry(key).or_insert_with(|| clean(&value));
    }
    fields
}

/// Lines from one `query_range` answer, at most [`LINE_LIMIT`].
pub fn parse_answer(answer: &Value) -> anyhow::Result<Vec<LogLine>> {
    ensure!(
        answer["status"] == "success" && answer["data"]["resultType"] == "streams",
        "The logs query failed"
    );
    let mut lines = Vec::new();
    for stream in answer["data"]["result"].as_array().into_iter().flatten() {
        let labels: BTreeMap<String, String> = stream["stream"]
            .as_object()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        for value in stream["values"].as_array().into_iter().flatten() {
            let (Some(ts), Some(line)) = (
                value[0].as_str().and_then(|ts| ts.parse::<i64>().ok()),
                value[1].as_str(),
            ) else {
                continue;
            };
            lines.push(parse_line(&labels, ts, line));
            if lines.len() >= LINE_LIMIT {
                return Ok(lines);
            }
        }
    }
    Ok(lines)
}

/// Who a session is: its tab's `sid`, or else its address and user agent.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SessionKey {
    Sid(String),
    Visitor { ip: String, agent: String },
}

#[derive(Clone, Debug, Default)]
pub struct Percentiles {
    pub p50: Option<f64>,
    pub p95: Option<f64>,
}

fn percentiles(mut values: Vec<f64>) -> Percentiles {
    if values.is_empty() {
        return Percentiles::default();
    }
    values.sort_by(f64::total_cmp);
    let rank =
        |p: f64| values[((p * values.len() as f64).ceil() as usize).clamp(1, values.len()) - 1];
    Percentiles {
        p50: Some(rank(0.50)),
        p95: Some(rank(0.95)),
    }
}

/// Server time per route: requests, p50, p95 and the slowest, in milliseconds.
#[derive(Clone, Debug)]
pub struct RouteTiming {
    pub source: String,
    pub route: String,
    pub count: usize,
    pub p50: u64,
    pub p95: u64,
    pub max: u64,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub key: SessionKey,
    pub ip: Option<String>,
    pub country: Option<String>,
    pub asn: Option<String>,
    pub client: Option<String>,
    pub agent: Option<String>,
    pub first_ns: i64,
    pub last_ns: i64,
    pub pages: usize,
    pub clicks: usize,
    pub errors: usize,
    /// The slowest request's milliseconds and what it was.
    pub slowest: Option<(u64, String)>,
    pub synthetic: bool,
    /// Its lines, oldest first, as indexes into [`Report::lines`].
    pub timeline: Vec<usize>,
    pub ttfb: Percentiles,
    pub lcp: Percentiles,
    pub inp: Percentiles,
    pub routes: Vec<RouteTiming>,
}

impl Session {
    pub fn slow(&self) -> bool {
        self.slowest.as_ref().is_some_and(|(ms, _)| *ms > SLOW_MS)
    }
}

/// A search's lines, oldest first, and the sessions they make up, newest first.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub lines: Vec<LogLine>,
    pub sessions: Vec<Session>,
    /// A query hit [`LINE_LIMIT`], so older lines are missing.
    pub truncated: bool,
    /// The edge lines of a session or user search could not be read.
    pub edge_missing: bool,
    pub took: Duration,
}

/// Group `lines` into sessions. `focus` keeps only what a session or user search asked for:
/// lines of the matched requests, not everything else from the same address.
pub fn build_report(mut lines: Vec<LogLine>, focus: Option<&Search>) -> Report {
    lines.sort_by_key(|line| line.ts);
    lines.dedup_by(|a, b| a.ts == b.ts && a.fields == b.fields && a.text == b.text);

    // Request ids of the session's own requests, with their sid and the edge's view of them.
    let mut sid_of: HashMap<String, String> = HashMap::new();
    let mut agent_of: HashMap<String, String> = HashMap::new();
    for line in &lines {
        if let (Some(rid), Some(sid)) = (line.get("rid"), line.get("sid")) {
            if !line.downstream() {
                sid_of
                    .entry(rid.to_owned())
                    .or_insert_with(|| sid.to_owned());
            }
        }
        if let (Kind::Access, Some(rid), Some(agent)) =
            (line.kind, line.get("rid"), line.get("agent"))
        {
            agent_of.insert(rid.to_owned(), agent.to_owned());
        }
    }

    if let Some(search) = focus {
        let matched: HashSet<String> = lines
            .iter()
            .filter(|line| match search {
                Search::Sid(sid) => line.get("sid") == Some(sid.as_str()),
                Search::User(user) => line.get("user") == Some(user.as_str()),
                _ => true,
            })
            .filter_map(|line| line.get("rid").map(str::to_owned))
            .collect();
        let sids: HashSet<String> = lines
            .iter()
            .filter(|line| match search {
                Search::User(user) => line.get("user") == Some(user.as_str()),
                Search::Sid(sid) => line.get("sid") == Some(sid.as_str()),
                _ => false,
            })
            .filter_map(|line| line.get("sid").map(str::to_owned))
            .collect();
        if matches!(search, Search::Sid(_) | Search::User(_)) {
            lines.retain(|line| {
                line.get("sid").is_some_and(|sid| sids.contains(sid))
                    || line.get("rid").is_some_and(|rid| {
                        matched.contains(rid) || sid_of.get(rid).is_some_and(|s| sids.contains(s))
                    })
                    || line.get("prid").is_some_and(|prid| matched.contains(prid))
            });
        }
    }

    // The visitor's own lines first; other services' lines join the session of the request
    // that called them.
    let mut groups: BTreeMap<SessionKey, Vec<usize>> = BTreeMap::new();
    let mut key_of: HashMap<String, SessionKey> = HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        if line.downstream() {
            continue;
        }
        let sid = line
            .get("sid")
            .map(str::to_owned)
            .or_else(|| line.get("rid").and_then(|rid| sid_of.get(rid).cloned()));
        let key = match (sid, line.get("ip")) {
            (Some(sid), _) => SessionKey::Sid(sid),
            // A line naming neither a session nor an address belongs to no visitor.
            (None, None) => continue,
            (None, Some(ip)) => SessionKey::Visitor {
                ip: ip.to_owned(),
                agent: line
                    .get("agent")
                    .or_else(|| {
                        line.get("rid")
                            .and_then(|rid| agent_of.get(rid).map(String::as_str))
                    })
                    .unwrap_or_default()
                    .to_owned(),
            },
        };
        if let Some(rid) = line.get("rid") {
            key_of.entry(rid.to_owned()).or_insert_with(|| key.clone());
        }
        groups.entry(key).or_default().push(index);
    }
    for (index, line) in lines.iter().enumerate() {
        if !line.downstream() {
            continue;
        }
        let key = line
            .get("prid")
            .and_then(|prid| key_of.get(prid))
            .or_else(|| line.get("rid").and_then(|rid| key_of.get(rid)));
        if let Some(key) = key {
            groups.entry(key.clone()).or_default().push(index);
        }
    }
    for timeline in groups.values_mut() {
        timeline.sort_unstable();
    }

    let mut sessions: Vec<Session> = groups
        .into_iter()
        .map(|(key, timeline)| summarize(key, timeline, &lines))
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.last_ns));
    Report {
        lines,
        sessions,
        ..Report::default()
    }
}

fn summarize(key: SessionKey, timeline: Vec<usize>, lines: &[LogLine]) -> Session {
    let own: Vec<&LogLine> = timeline.iter().map(|index| &lines[*index]).collect();
    let all = || own.iter().copied();
    let visitor_lines = || all().filter(|line| !line.downstream() && line.kind != Kind::Other);
    let edge = all().find(|line| line.kind == Kind::Access);
    let ip = edge
        .and_then(|line| line.get("ip"))
        .or_else(|| visitor_lines().find_map(|line| line.get("ip")))
        .map(str::to_owned);
    let agent = all()
        .find_map(|line| line.get("agent"))
        .map(str::to_owned)
        .or_else(|| match &key {
            SessionKey::Visitor { agent, .. } if !agent.is_empty() => Some(agent.clone()),
            _ => None,
        });
    let client = all().find_map(|line| line.client.clone());
    let synthetic = agent
        .as_deref()
        .is_some_and(|agent| agent.to_ascii_lowercase().contains("synth"))
        || client.as_deref().is_some_and(|client| client != "person");

    let events = |name: &str| {
        all()
            .filter(|line| line.kind == Kind::UiEvent && line.get("ev") == Some(name))
            .count()
    };
    let page_views = events("page_view");
    let pages = if page_views > 0 {
        page_views
    } else {
        all()
            .filter(|line| {
                line.kind == Kind::Access
                    && line.get("method") == Some("GET")
                    && line.get("path").is_some_and(page_path)
            })
            .count()
    };
    let failed = |line: &LogLine| {
        matches!(line.kind, Kind::Access | Kind::Http) && line.status().is_some_and(|s| s >= 500)
    };
    let errors = events("js_error") + all().filter(|l| failed(l)).count();

    let mut slowest: Option<(u64, String)> = None;
    let mut by_route: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
    for line in all() {
        let Some(ms) = line.ms() else { continue };
        let what = match line.kind {
            Kind::Access => line.get("path").unwrap_or("?").to_owned(),
            Kind::Http => line.get("route").unwrap_or("?").to_owned(),
            _ => continue,
        };
        if slowest.as_ref().is_none_or(|(slowest, _)| ms > *slowest) {
            slowest = Some((ms, what.clone()));
        }
        if line.kind == Kind::Http {
            by_route
                .entry((source_name(&line.source), what))
                .or_default()
                .push(ms);
        }
    }
    let routes = by_route
        .into_iter()
        .map(|((source, route), ms)| {
            let timing = percentiles(ms.iter().map(|ms| *ms as f64).collect());
            RouteTiming {
                source,
                route,
                count: ms.len(),
                p50: timing.p50.unwrap_or_default() as u64,
                p95: timing.p95.unwrap_or_default() as u64,
                max: ms.iter().copied().max().unwrap_or_default(),
            }
        })
        .collect();
    let vital = |event: &str, key: &str| {
        percentiles(
            all()
                .filter(|line| line.kind == Kind::UiEvent && line.get("ev") == Some(event))
                .filter_map(|line| line.number(key))
                .collect(),
        )
    };

    Session {
        ip,
        country: edge.and_then(|line| {
            line.get("country_code")
                .or(line.get("country"))
                .map(str::to_owned)
        }),
        asn: edge.and_then(|line| match (line.get("asn"), line.get("org")) {
            (Some(asn), Some(org)) => Some(format!("AS{asn} {org}")),
            (Some(asn), None) => Some(format!("AS{asn}")),
            (None, org) => org.map(str::to_owned),
        }),
        client,
        agent,
        first_ns: own.first().map(|line| line.ts).unwrap_or_default(),
        last_ns: own.last().map(|line| line.ts).unwrap_or_default(),
        pages,
        clicks: events("click"),
        errors,
        slowest,
        synthetic,
        ttfb: vital("page_view", "ttfb"),
        lcp: vital("vitals", "lcp"),
        inp: vital("vitals", "inp"),
        routes,
        timeline,
        key,
    }
}

/// Whether an address is a page rather than the API or a static file.
fn page_path(path: &str) -> bool {
    !(path.starts_with("/api/")
        || path.starts_with("/assets/")
        || path.starts_with("/ui/")
        || path.starts_with("/static/")
        || path == "/favicon.ico"
        || path == "/robots.txt")
}

pub fn source_name(source: &Source) -> String {
    match source {
        Source::Edge => "edge".into(),
        Source::App(app) => app.clone(),
    }
}

/// Why a search has no report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    NotConfigured,
    TimedOut,
    Failed(String),
}

type CacheKey = (Search, Window);

pub struct VisitorLogs {
    client: Option<reqwest::Client>,
    query_url: String,
    explore_url: Option<String>,
    /// Why configured logs cannot be read.
    pub configuration_error: Option<String>,
    cache: Mutex<HashMap<CacheKey, (Instant, Arc<Report>)>>,
}

impl VisitorLogs {
    pub fn from_settings(settings: &LogsSettings) -> Self {
        let mut logs = Self {
            client: None,
            query_url: String::new(),
            explore_url: settings
                .explore_url
                .as_deref()
                .filter(|url| safe_base(url).is_ok())
                .map(|url| url.trim_end_matches('/').to_owned()),
            configuration_error: None,
            cache: Mutex::new(HashMap::new()),
        };
        let Some(url) = settings.url.as_deref() else {
            return logs;
        };
        match safe_base(url).and_then(|_| {
            Ok(reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(4))
                .build()?)
        }) {
            Ok(client) => {
                logs.client = Some(client);
                logs.query_url = format!("{}{}", url.trim_end_matches('/'), settings.query_path);
            }
            Err(error) => {
                log::warn!("Visitor logs are unavailable: {error}");
                logs.configuration_error = Some(error.to_string());
            }
        }
        logs
    }

    pub fn configured(&self) -> bool {
        self.client.is_some()
    }

    /// An "Open in Explore" link for `rid`, when an Explore base is set.
    pub fn explore_rid(&self, rid: &str, window: Window) -> Option<String> {
        valid_rid(rid).then_some(())?;
        self.explore(&format!(r#"{{job=~".+"}} |= "{rid}""#), window)
    }

    /// An Explore link for a search's own query.
    pub fn explore_search(&self, search: &Search, window: Window) -> Option<String> {
        self.explore(&search.query(), window)
    }

    fn explore(&self, expr: &str, window: Window) -> Option<String> {
        let base = self.explore_url.as_deref()?;
        let panes = serde_json::json!({
            "v": {
                "datasource": "visitors",
                "queries": [{
                    "refId": "A",
                    "expr": expr,
                    "datasource": { "type": "loki", "uid": "visitors" },
                }],
                "range": { "from": format!("now-{}", window.as_str()), "to": "now" },
            }
        });
        let mut url = reqwest::Url::parse(&format!("{base}/explore")).ok()?;
        url.query_pairs_mut()
            .append_pair("schemaVersion", "1")
            .append_pair("panes", &panes.to_string())
            .append_pair("orgId", "1");
        Some(url.to_string())
    }

    /// The report for `search` over `window`, reused for [`CACHE_TTL`].
    pub async fn search(
        &self,
        search: &Search,
        window: Window,
    ) -> Result<Arc<Report>, SearchError> {
        let client = self.client.as_ref().ok_or(SearchError::NotConfigured)?;
        let key = (search.clone(), window);
        if let Some((at, report)) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            if at.elapsed() < CACHE_TTL {
                return Ok(report.clone());
            }
        }
        let started = Instant::now();
        let end = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        let start = end - i128::from(window.seconds()) * 1_000_000_000;
        let range = (start, end);
        let first = tokio::time::timeout(SEARCH_BUDGET, self.query(client, &search.query(), range))
            .await
            .map_err(|_| SearchError::TimedOut)?
            .map_err(|error| {
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(reqwest::Error::is_timeout)
                }) {
                    SearchError::TimedOut
                } else {
                    SearchError::Failed(error.to_string())
                }
            })?;
        let mut truncated = first.len() >= LINE_LIMIT;
        let mut lines = first;
        let mut edge_missing = false;
        // Session and user searches read only app lines; the edge's lines for the same
        // addresses add geo, client class and the user agent.
        if matches!(search, Search::Sid(_) | Search::User(_)) {
            let mut addresses: Vec<IpAddr> = Vec::new();
            for line in lines.iter().rev() {
                if line.downstream() {
                    continue;
                }
                if let Some(ip) = line.get("ip").and_then(|ip| ip.parse::<IpAddr>().ok()) {
                    if !addresses.contains(&ip) {
                        addresses.push(ip);
                    }
                }
                if addresses.len() >= FOLLOW_UP_ADDRESSES {
                    break;
                }
            }
            let remaining = SEARCH_BUDGET.saturating_sub(started.elapsed());
            let follow_ups = join_all(addresses.into_iter().map(|ip| {
                let query = Search::Ip(ip).query();
                async move { self.query(client, &query, range).await }
            }));
            match tokio::time::timeout(remaining, follow_ups).await {
                Ok(results) => {
                    for result in results {
                        match result {
                            Ok(more) => {
                                truncated |= more.len() >= LINE_LIMIT;
                                lines.extend(
                                    more.into_iter().filter(|line| line.kind == Kind::Access),
                                );
                            }
                            Err(_) => edge_missing = true,
                        }
                    }
                }
                Err(_) => edge_missing = true,
            }
        }
        let mut report = build_report(lines, Some(search));
        report.truncated = truncated;
        report.edge_missing = edge_missing;
        report.took = started.elapsed();
        let report = Arc::new(report);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= CACHE_KEYS {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            if cache.len() >= CACHE_KEYS {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), report.clone()));
        Ok(report)
    }

    async fn query(
        &self,
        client: &reqwest::Client,
        query: &str,
        (start, end): (i128, i128),
    ) -> anyhow::Result<Vec<LogLine>> {
        let (start, end, limit) = (start.to_string(), end.to_string(), LINE_LIMIT.to_string());
        let mut response = client
            .get(&self.query_url)
            .query(&[
                ("query", query),
                ("start", start.as_str()),
                ("end", end.as_str()),
                ("limit", limit.as_str()),
                ("direction", "backward"),
            ])
            .send()
            .await?
            .error_for_status()?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                body.len() + chunk.len() <= MAX_BODY,
                "The logs answer is larger than 1 MiB"
            );
            body.extend_from_slice(&chunk);
        }
        let answer: Value = serde_json::from_slice(&body)?;
        parse_answer(&answer)
    }
}

/// An HTTPS base URL (HTTP only on loopback) without credentials, query or fragment.
fn safe_base(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback),
        "logs need HTTPS outside loopback"
    );
    ensure!(
        parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "the logs URL must not contain credentials, a query, or a fragment"
    );
    if parsed.host_str().is_none() {
        return Err(anyhow!("the logs URL has no host"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
