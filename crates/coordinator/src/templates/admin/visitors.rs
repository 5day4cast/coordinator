//! The operator's Visitors page: a search form that renders at once, and the results, which
//! load as a fragment. Everything from the logs is shown as escaped text.

use maud::{html, Markup};
use time::{macros::format_description, OffsetDateTime};

use crate::infra::visitor_logs::{
    source_name, Kind, LogLine, Percentiles, Report, Search, SearchError, Session, SessionKey,
    VisitorLogs, Window, SLOW_MS,
};

/// Sessions given a timeline on the page.
const SHOWN_SESSIONS: usize = 30;
/// Lines shown of one session's timeline.
const SHOWN_LINES: usize = 400;

/// What the search form holds.
#[derive(Clone, Debug, Default)]
pub struct VisitorQuery {
    pub ip: String,
    pub rid: String,
    pub sid: String,
    pub user: String,
    pub window: Window,
    pub synthetic: bool,
}

impl VisitorQuery {
    pub fn is_empty(&self) -> bool {
        [&self.ip, &self.rid, &self.sid, &self.user]
            .iter()
            .all(|value| value.trim().is_empty())
    }

    /// The query string that repeats this search, with `synthetic` as given.
    pub fn query_string(&self, synthetic: bool) -> String {
        let mut url = reqwest::Url::parse("http://localhost/").expect("static URL");
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in [
                ("ip", &self.ip),
                ("rid", &self.rid),
                ("sid", &self.sid),
                ("user", &self.user),
            ] {
                if !value.trim().is_empty() {
                    pairs.append_pair(key, value.trim());
                }
            }
            pairs.append_pair("window", self.window.as_str());
            if synthetic {
                pairs.append_pair("synth", "1");
            }
        }
        url.query().unwrap_or_default().to_owned()
    }
}

fn utc(ns: i64, with_date: bool) -> String {
    let Ok(at) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns)) else {
        return String::from("?");
    };
    if with_date {
        at.format(format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second]"
        ))
    } else {
        at.format(format_description!(
            "[hour]:[minute]:[second].[subsecond digits:3]"
        ))
    }
    .unwrap_or_default()
}

/// The page around the results: the form, and a slot that loads them.
pub fn visitors_page(logs: &VisitorLogs, query: &VisitorQuery) -> Markup {
    html! { main.admin-workspace {
        p.eyebrow { "Players" } h1 { "Visitors" }
        p { "Find a visitor by address, request id, session id, or account, and follow what they did across the site and its services. Lab and synthetic traffic is hidden unless you ask for it." }
        @if !logs.configured() {
            p.notice {
                "Visitor logs are not configured. Set " code { "COORDINATOR_LOGS_URL" }
                " (or " code { "[admin_settings.logs] url" } ") to the monitoring base URL."
                @if let Some(error) = &logs.configuration_error { " The configured URL was refused: " (error) "." }
            }
        }
        form.discovery-filters method="get" action="/admin/visitors" {
            label { "Address" input name="ip" value=(query.ip) maxlength="64" placeholder="203.0.113.7"; }
            label { "Request id" input name="rid" value=(query.rid) maxlength="64"; }
            label { "Session id" input name="sid" value=(query.sid) maxlength="32"; }
            label { "Pubkey, npub or username" input name="user" value=(query.user) maxlength="128"; }
            label { "Window"
                select name="window" {
                    @for window in Window::ALL {
                        option value=(window.as_str()) selected[window == query.window] { (window.as_str()) }
                    }
                }
            }
            label { input type="checkbox" name="synth" value="1" checked[query.synthetic]; " Show lab and synthetic traffic" }
            button type="submit" { "Search" }
        }
        @if logs.configured() && !query.is_empty() {
            section id="visitor-results" hx-get=(format!("/admin/visitors/results?{}", query.query_string(query.synthetic)))
                hx-trigger="load" hx-swap="outerHTML" aria-busy="true" {
                p.note { "Reading visitor logs…" }
            }
        }
    } }
}

/// Why there are no results, with a way to look in Grafana instead.
pub fn visitors_error(error: &SearchError, explore: Option<&str>) -> Markup {
    html! {
        section id="visitor-results" {
            p.notice {
                @match error {
                    SearchError::NotConfigured => "Visitor logs are not configured.",
                    SearchError::TimedOut => "The logs did not answer within 4 seconds.",
                    SearchError::Failed(_) => "The logs could not be read.",
                }
                @if let SearchError::Failed(reason) = error {
                    " " span.note { (reason.chars().take(200).collect::<String>()) }
                }
            }
            @if let Some(explore) = explore {
                p { a href=(explore) rel="noreferrer" target="_blank" { "Open this search in Grafana Explore" } }
            }
        }
    }
}

/// A search the form could not run.
pub fn visitors_invalid(reason: &str) -> Markup {
    html! { section id="visitor-results" { p.notice { (reason) } } }
}

fn session_label(key: &SessionKey) -> Markup {
    match key {
        SessionKey::Sid(sid) => html! { code { (sid) } },
        SessionKey::Visitor { ip, .. } => html! { "no session · " code { (ip) } },
    }
}

fn ms(value: Option<f64>) -> String {
    value.map_or_else(|| "–".into(), |value| format!("{value:.0}"))
}

fn vital(name: &str, values: &Percentiles) -> Markup {
    html! { tr { th { (name) } td { (ms(values.p50)) } td { (ms(values.p95)) } } }
}

/// What a line says, in a few words.
fn line_summary(line: &LogLine) -> Markup {
    let field = |key: &str| line.get(key).unwrap_or_default().to_owned();
    match line.kind {
        Kind::Access => html! {
            (field("method")) " " code { (field("path")) }
            @if let Some(country) = line.get("country_code").or(line.get("country")) { " · " (country) }
            @if let Some(org) = line.get("org") { " · " (org) }
        },
        Kind::Http => html! {
            (field("method")) " " code { (field("route")) }
            @if let Some(prid) = line.get("prid") { " · called by " code { (prid.chars().take(13).collect::<String>()) "…" } }
            @if let Some(user) = line.get("user") { " · user " code { (user) } }
        },
        Kind::UiEvent => {
            let keys: &[&str] = match line.get("ev").unwrap_or_default() {
                "page_view" => &["page", "ref", "ttfb", "load"],
                "vitals" => &["page", "lcp", "inp", "cls"],
                "click" => &["page", "el", "id", "track", "text"],
                "submit" => &["page", "form"],
                "htmx" => &["verb", "path", "status", "ms"],
                "js_error" => &["msg", "src", "line"],
                "mark" => &["name"],
                _ => &["page"],
            };
            html! {
                strong { (field("ev")) }
                @for key in keys {
                    @if let Some(value) = line.get(key) { " " (key) "=" code { (value) } }
                }
            }
        }
        Kind::Feedback => html! {
            "feedback sent · "
            @if let Some(id) = line.get("id").and_then(|id| uuid::Uuid::parse_str(id).ok()) {
                a href=(format!("/admin/feedback/{id}")) { "read it" }
            }
        },
        Kind::Other => html! { span.note { (line.text) } },
    }
}

fn timeline(report: &Report, session: &Session, logs: &VisitorLogs, window: Window) -> Markup {
    html! {
        div.scroll { table.ops-table {
            thead { tr { th { "Time (UTC)" } th { "Source" } th { "What" } th { "Status" } th { "ms" } th { "Request" } } }
            tbody {
                @for index in session.timeline.iter().take(SHOWN_LINES) {
                    @let line = &report.lines[*index];
                    tr {
                        td { (utc(line.ts, false)) }
                        td { (source_name(&line.source)) }
                        td { (line_summary(line)) }
                        td { @if let Some(status) = line.status() { @if status >= 500 { strong { (status) } } @else { (status) } } }
                        td { @if let Some(ms) = line.ms() { @if ms > SLOW_MS { strong title="Over 400 ms" { (ms) " ⚠" } } @else { (ms) } } }
                        td {
                            @if let Some(rid) = line.get("rid") {
                                @match logs.explore_rid(rid, window) {
                                    Some(link) => a href=(link) rel="noreferrer" target="_blank" title=(rid) { code { (rid.chars().take(13).collect::<String>()) "…" } },
                                    None => code title=(rid) { (rid.chars().take(13).collect::<String>()) "…" },
                                }
                            }
                        }
                    }
                }
            }
        } }
        @if session.timeline.len() > SHOWN_LINES {
            p.note { "Showing the first " (SHOWN_LINES) " of " (session.timeline.len()) " lines." }
        }
    }
}

fn performance(session: &Session) -> Markup {
    html! {
        div.metric-grid {
            div {
                h3 { "Page timings (ms)" }
                table.ops-table {
                    thead { tr { th {} th { "p50" } th { "p95" } } }
                    tbody { (vital("TTFB", &session.ttfb)) (vital("LCP", &session.lcp)) (vital("INP", &session.inp)) }
                }
            }
            div {
                h3 { "Server time per route (ms)" }
                @if session.routes.is_empty() { p.note { "No request lines." } }
                @else {
                    table.ops-table {
                        thead { tr { th { "Service" } th { "Route" } th { "Requests" } th { "p50" } th { "p95" } th { "Max" } } }
                        tbody {
                            @for route in &session.routes {
                                tr {
                                    td { (route.source) } td { code { (route.route) } } td { (route.count) }
                                    td { (route.p50) } td { (route.p95) }
                                    td { @if route.max > SLOW_MS { strong { (route.max) " ⚠" } } @else { (route.max) } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The results of a search.
pub fn visitors_results(
    report: &Report,
    logs: &VisitorLogs,
    search: &Search,
    query: &VisitorQuery,
) -> Markup {
    let shown: Vec<&Session> = report
        .sessions
        .iter()
        .filter(|session| query.synthetic || !session.synthetic)
        .collect();
    let hidden = report.sessions.len() - shown.len();
    html! {
        section id="visitor-results" {
            p.note {
                (report.lines.len()) " lines in " (report.sessions.len()) " sessions for " code { (search.value()) }
                " over " (query.window.as_str()) ", read in " (report.took.as_millis()) " ms."
                @if let Some(explore) = logs.explore_search(search, query.window) {
                    " " a href=(explore) rel="noreferrer" target="_blank" { "Open in Grafana Explore" }
                }
            }
            @if report.truncated {
                p.notice { "The logs held more lines than one search reads (2000); the oldest are missing. Narrow the window." }
            }
            @if report.edge_missing {
                p.notice { "The edge's lines could not be read, so location and client class may be missing." }
            }
            @if hidden > 0 {
                p.note {
                    (hidden) " lab or synthetic sessions hidden. "
                    a href=(format!("/admin/visitors?{}", query.query_string(true))) { "Show them" }
                }
            }
            @if shown.is_empty() {
                p { "No visitor sessions found." }
            } @else {
                h2 { "Sessions" }
                div.scroll { table.ops-table {
                    thead { tr {
                        th { "Session" } th { "Address" } th { "Where" } th { "Client" }
                        th { "First seen (UTC)" } th { "Last seen" } th { "Pages" } th { "Clicks" } th { "Errors" } th { "Slowest" }
                    } }
                    tbody {
                        @for (index, session) in shown.iter().enumerate() {
                            tr {
                                td { @if index < SHOWN_SESSIONS { a href=(format!("#session-{index}")) { (session_label(&session.key)) } } @else { (session_label(&session.key)) } }
                                td { @if let Some(ip) = &session.ip { code { (ip) } } }
                                td { (session.country.as_deref().unwrap_or("")) @if let Some(asn) = &session.asn { br; span.note { (asn) } } }
                                td {
                                    (session.client.as_deref().unwrap_or("–"))
                                    @if let Some(agent) = &session.agent { br; span.note title=(agent) { (agent.chars().take(40).collect::<String>()) } }
                                }
                                td { (utc(session.first_ns, true)) }
                                td { (utc(session.last_ns, true)) }
                                td { (session.pages) }
                                td { (session.clicks) }
                                td { @if session.errors > 0 { strong { (session.errors) } } @else { "0" } }
                                td {
                                    @if let Some((ms, what)) = &session.slowest {
                                        @if session.slow() { strong title="Over 400 ms" { (ms) " ms ⚠" } } @else { (ms) " ms" }
                                        br; code { (what) }
                                    }
                                }
                            }
                        }
                    }
                } }
                @for (index, session) in shown.iter().take(SHOWN_SESSIONS).enumerate() {
                    details id=(format!("session-{index}")) open[index == 0] {
                        summary { "Session " (session_label(&session.key)) " · " (session.timeline.len()) " lines" }
                        (performance(session))
                        h3 { "Timeline" }
                        (timeline(report, session, logs, query.window))
                    }
                }
                @if shown.len() > SHOWN_SESSIONS {
                    p.note { "Timelines are shown for the newest " (SHOWN_SESSIONS) " sessions." }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{config::LogsSettings, infra::visitor_logs::build_report};

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn report() -> Report {
        let app = labels(&[("job", "application-journal"), ("app", "coordinator")]);
        let edge = labels(&[("job", "caddy-access"), ("client", "person")]);
        let bot = labels(&[("job", "caddy-access"), ("client", "bot")]);
        let lines = vec![
            crate::infra::visitor_logs::parse_line(&edge, 1_791_398_962_000_000_000, r#"{"ip":"203.0.113.7","path":"/<script>","method":"GET","status":200,"duration":0.6,"agent":"Mozilla","rid":"0199c1a2-0000-7000-8000-000000000001"}"#),
            crate::infra::visitor_logs::parse_line(&app, 1_791_398_962_100_000_000, "[t INFO] ui_event: ui_event site=5day4cast rid=0199c1a2-0000-7000-8000-000000000001 sid=Xq3vT9mPa1Lw0Zb8Yc7Rkd ip=203.0.113.7 ev=click page=/ text=\"<b>Go</b>\""),
            crate::infra::visitor_logs::parse_line(&bot, 1_791_398_963_000_000_000, r#"{"ip":"203.0.113.7","path":"/wp-login.php","method":"GET","status":404,"duration":0.001,"agent":"scanner"}"#),
        ];
        build_report(lines, None)
    }

    fn logs() -> VisitorLogs {
        VisitorLogs::from_settings(&LogsSettings {
            url: Some("https://monitoring.example.com".into()),
            explore_url: Some("https://grafana.example.com".into()),
            ..LogsSettings::default()
        })
    }

    #[test]
    fn results_escape_log_text_flag_slow_requests_and_hide_synthetic_sessions() {
        let report = report();
        let query = VisitorQuery {
            ip: "203.0.113.7".into(),
            ..VisitorQuery::default()
        };
        let search = Search::Ip("203.0.113.7".parse().unwrap());
        let html = visitors_results(&report, &logs(), &search, &query).into_string();
        assert!(!html.contains("<script>") && !html.contains("<b>Go</b>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("600 ms ⚠"));
        assert!(html.contains("1 lab or synthetic sessions hidden"));
        assert!(!html.contains("wp-login.php"));
        assert!(html.contains("https://grafana.example.com/explore?"));
        let all = visitors_results(
            &report,
            &logs(),
            &search,
            &VisitorQuery {
                synthetic: true,
                ..query
            },
        )
        .into_string();
        assert!(all.contains("wp-login.php"));
    }

    #[test]
    fn the_shell_loads_results_only_for_a_search_on_configured_logs() {
        let query = VisitorQuery {
            sid: "Xq3vT9mPa1Lw0Zb8Yc7Rkd".into(),
            window: Window::Week,
            ..VisitorQuery::default()
        };
        let html = visitors_page(&logs(), &query).into_string();
        assert!(
            html.contains(
                r#"hx-get="/admin/visitors/results?sid=Xq3vT9mPa1Lw0Zb8Yc7Rkd&amp;window=7d""#
            ),
            "{html}"
        );
        assert!(html.contains(r#"hx-trigger="load""#));
        assert!(!visitors_page(&logs(), &VisitorQuery::default())
            .into_string()
            .contains("hx-get"));
        let unset = VisitorLogs::from_settings(&LogsSettings::default());
        let html = visitors_page(&unset, &query).into_string();
        assert!(html.contains("Visitor logs are not configured"));
        assert!(!html.contains("hx-get"));
    }

    #[test]
    fn errors_are_short_and_offer_explore() {
        let html = visitors_error(
            &SearchError::TimedOut,
            Some("https://grafana.example.com/explore?x"),
        )
        .into_string();
        assert!(html.contains("within 4 seconds"));
        assert!(html.contains("Open this search in Grafana Explore"));
        let html = visitors_error(&SearchError::Failed("<oops>".into()), None).into_string();
        assert!(html.contains("&lt;oops&gt;"));
    }
}
