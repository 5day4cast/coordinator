use std::sync::Mutex as StdMutex;

use axum::{extract::Query, routing::get, Json, Router};

use super::*;

const RID: &str = "0199c1a2-7b3e-7c11-9a00-5f1e2d3c4b5a";
const RID2: &str = "0199c1a2-7b3e-7c11-9a00-5f1e2d3c4b5b";
const SID: &str = "Xq3vT9mPa1Lw0Zb8Yc7Rkd";
const SID2: &str = "Mn4bV8cXz2Qa6Ws1Ed5Rtf";
const IP: &str = "198.51.100.23";
/// 2026-10-07T18:49:22Z in nanoseconds.
const T0: i64 = 1_791_398_962_000_000_000;
const MS: i64 = 1_000_000;

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn edge_labels(client: &str) -> BTreeMap<String, String> {
    labels(&[
        ("job", "caddy-access"),
        ("site", "5day4cast.com"),
        ("client", client),
    ])
}

fn app_labels(app: &str) -> BTreeMap<String, String> {
    labels(&[
        ("job", "application-journal"),
        ("app", app),
        ("slot", "green"),
    ])
}

/// An access line as the log shipper writes it (see its caddy-access pipeline).
fn access(rid: &str, path: &str, status: u16, seconds: f64, agent: &str) -> String {
    serde_json::json!({
        "ip": IP, "country": "Germany", "country_code": "DE", "region": "Berlin",
        "city": "Berlin", "lat": 52.5, "lon": 13.4, "asn": 3320,
        "org": "Deutsche Telekom AG", "proto": "HTTP/2.0", "method": "GET", "path": path,
        "status": status, "duration": seconds, "size": 5120, "referrer": "",
        "agent": agent, "reason": "browser", "request_id": rid,
    })
    .to_string()
}

const BROWSER: &str = "Mozilla/5.0 (X11; Linux x86_64) Firefox/131.0";

/// The coordinator's request line, with fern's prefix and colours.
fn http(rid: &str, sid: &str, route: &str, status: u16, ms: u64) -> String {
    format!(
        "[2026-10-07T18:49:22.269806379Z \u{1b}[34mINFO\u{1b}[0m] http: http rid={rid} prid=- sid={sid} ip={IP} method=GET route={route} status={status} ms={ms} user=-"
    )
}

fn fixture() -> Vec<LogLine> {
    let edge = edge_labels("person");
    let coordinator = app_labels("coordinator");
    let oracle = app_labels("oracle");
    let keymeld = app_labels("keymeld");
    vec![
        parse_line(&edge, T0, &access(RID, "/competitions", 200, 0.0123, BROWSER)),
        parse_line(&coordinator, T0 + MS, &http(RID, "-", "/competitions", 200, 11)),
        parse_line(
            &coordinator,
            T0 + 300 * MS,
            &format!(
                "[2026-10-07T18:49:22.569Z \u{1b}[34mINFO\u{1b}[0m] ui_event: ui_event site=5day4cast rid={RID} sid={SID} ip={IP} ev=page_view page=/competitions ref=- ttfb=180 dcl=320 load=410"
            ),
        ),
        parse_line(
            &coordinator,
            T0 + 2000 * MS,
            &format!(
                "[2026-10-07T18:49:24Z \u{1b}[34mINFO\u{1b}[0m] ui_event: ui_event site=5day4cast rid={RID} sid={SID} ip={IP} ev=click page=/competitions el=button id=- track=enter text=\"Enter now\""
            ),
        ),
        parse_line(&edge, T0 + 2100 * MS, &access(RID2, "/competitions/abc/entry-form", 200, 0.512, BROWSER)),
        parse_line(&coordinator, T0 + 2101 * MS, &http(RID2, SID, "/competitions/{competition_id}/entry-form", 200, 505)),
        parse_line(
            &oracle,
            T0 + 2200 * MS,
            &format!(
                "[2026-10-07T18:49:24.2Z INFO] http: http rid=0199c1a2-0000-7000-8000-00000000aaaa prid={RID2} sid=- ip=10.0.0.5 method=GET route=/stations status=200 ms=40 user=-"
            ),
        ),
        parse_line(
            &keymeld,
            T0 + 2300 * MS,
            &format!(
                "2026-10-07T18:49:24.300000Z  \u{1b}[32m INFO\u{1b}[0m \u{1b}[1mrequest\u{1b}[0m\u{1b}[1m{{\u{1b}[0m\u{1b}[3mrid\u{1b}[0m\u{1b}[2m=\u{1b}[0m{RID2}\u{1b}[1m}}\u{1b}[0m\u{1b}[2m:\u{1b}[0m keymeld_gateway::api: session created prid={RID2}"
            ),
        ),
        parse_line(
            &coordinator,
            T0 + 5000 * MS,
            &format!(
                "[2026-10-07T18:49:27Z \u{1b}[34mINFO\u{1b}[0m] feedback: feedback id=0199c1a3-1111-7000-8000-000000000001 rid={RID2} sid={SID} ip={IP} page=/competitions"
            ),
        ),
        parse_line(
            &coordinator,
            T0 + 6000 * MS,
            &format!(
                "[2026-10-07T18:49:28Z \u{1b}[34mINFO\u{1b}[0m] ui_event: ui_event site=5day4cast rid={RID} sid={SID} ip={IP} ev=vitals page=/competitions lcp=900 cls=0.01 inp=120"
            ),
        ),
        // A second tab behind the same address.
        parse_line(&coordinator, T0 + 7000 * MS, &http("0199c1a2-0000-7000-8000-00000000bbbb", SID2, "/", 200, 9)),
        // An old-style line is kept as text.
        parse_line(
            &coordinator,
            T0 + 8000 * MS,
            "[2026-10-07T18:49:30Z \u{1b}[34mINFO\u{1b}[0m] http_request: new request, GET /api/v1/competitions",
        ),
    ]
}

#[test]
fn edge_access_lines_are_read_from_json() {
    let line = parse_line(
        &edge_labels("person"),
        T0,
        &access(RID, "/entries", 502, 0.4321, BROWSER),
    );
    assert_eq!(line.kind, Kind::Access);
    assert_eq!(line.source, Source::Edge);
    assert_eq!(line.get("rid"), Some(RID));
    assert_eq!(line.get("ip"), Some(IP));
    assert_eq!(line.get("country_code"), Some("DE"));
    assert_eq!(line.get("asn"), Some("3320"));
    assert_eq!(line.status(), Some(502));
    assert_eq!(line.ms(), Some(432));
    assert_eq!(line.client.as_deref(), Some("person"));
    // Status and duration as text are read too.
    let text = parse_line(
        &edge_labels("bot"),
        T0,
        r#"{"ip":"2001:db8::1","path":"/","status":"404","duration":"0.002","rid":"abcdefgh"}"#,
    );
    assert_eq!(text.status(), Some(404));
    assert_eq!(text.ms(), Some(2));
    assert_eq!(text.get("ip"), Some("2001:db8::1"));
    assert_eq!(text.get("rid"), Some("abcdefgh"));
}

#[test]
fn app_lines_are_read_through_prefixes_and_colours() {
    let lines = fixture();
    let http = &lines[1];
    assert_eq!(http.kind, Kind::Http);
    assert_eq!(http.source, Source::App("coordinator".into()));
    assert_eq!(http.get("rid"), Some(RID));
    assert_eq!(http.get("sid"), None, "a `-` is no value");
    assert_eq!(http.get("route"), Some("/competitions"));
    assert_eq!(http.ms(), Some(11));

    let click = &lines[3];
    assert_eq!(click.kind, Kind::UiEvent);
    assert_eq!(click.get("text"), Some("Enter now"));
    assert_eq!(click.get("track"), Some("enter"));

    let entry = &lines[5];
    assert_eq!(
        entry.get("route"),
        Some("/competitions/{competition_id}/entry-form")
    );

    let oracle = &lines[6];
    assert_eq!(oracle.kind, Kind::Http);
    assert!(oracle.downstream());
    assert_eq!(oracle.get("prid"), Some(RID2));

    // A tracing span's field, between colour codes and braces.
    let keymeld = &lines[7];
    assert_eq!(keymeld.kind, Kind::Other);
    assert_eq!(keymeld.get("rid"), Some(RID2));
    assert!(keymeld.text.contains("session created"));
    assert!(!keymeld.text.contains('\u{1b}'));

    let feedback = &lines[8];
    assert_eq!(feedback.kind, Kind::Feedback);
    assert_eq!(
        feedback.get("id"),
        Some("0199c1a3-1111-7000-8000-000000000001")
    );

    let old = &lines[11];
    assert_eq!(old.kind, Kind::Other);
    assert_eq!(old.text, "new request, GET /api/v1/competitions");
}

#[test]
fn quoted_values_and_odd_keys_are_handled() {
    let parsed = fields(r#"ev=js_error msg="a \"quoted\" = thing" src=app.js line=12 url?x=1 k=v"#);
    assert_eq!(parsed["msg"], r#"a "quoted" = thing"#);
    assert_eq!(parsed["src"], "app.js");
    assert_eq!(parsed["line"], "12");
    assert_eq!(parsed["k"], "v");
    assert!(!parsed.contains_key("x"));
    // The first value of a key wins; the logger's trailing rid repeats it.
    assert_eq!(fields("rid=one msg=x rid=two")["rid"], "one");
    // Addresses keep their colons; a span's closing brace is cut.
    assert_eq!(fields("ip=2001:db8:: x=1")["ip"], "2001:db8::");
    assert_eq!(fields("span{rid=abc}: msg")["rid"], "abc");
    assert_eq!(fields(r#"text="unterminated"#)["text"], "unterminated");
    assert_eq!(strip_ansi("\u{1b}[1;34mINFO\u{1b}[0m done"), "INFO done");
}

#[test]
fn sessions_are_grouped_by_sid_and_joined_by_request_id() {
    let report = build_report(fixture(), None);
    let keys: Vec<_> = report.sessions.iter().map(|s| s.key.clone()).collect();
    assert!(keys.contains(&SessionKey::Sid(SID.into())));
    assert!(keys.contains(&SessionKey::Sid(SID2.into())));
    let session = report
        .sessions
        .iter()
        .find(|s| s.key == SessionKey::Sid(SID.into()))
        .unwrap();
    // The first request had no sid on its http line; its page_view event names the session.
    let kinds: Vec<_> = session
        .timeline
        .iter()
        .map(|i| report.lines[*i].kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            Kind::Access,
            Kind::Http,
            Kind::UiEvent,
            Kind::UiEvent,
            Kind::Access,
            Kind::Http,
            Kind::Http,
            Kind::Other,
            Kind::Feedback,
            Kind::UiEvent,
        ]
    );
    assert_eq!(session.ip.as_deref(), Some(IP));
    assert_eq!(session.country.as_deref(), Some("DE"));
    assert_eq!(session.asn.as_deref(), Some("AS3320 Deutsche Telekom AG"));
    assert_eq!(session.client.as_deref(), Some("person"));
    assert_eq!(session.pages, 1);
    assert_eq!(session.clicks, 1);
    assert_eq!(session.errors, 0);
    assert_eq!(
        session.slowest,
        Some((512, "/competitions/abc/entry-form".into()))
    );
    assert!(session.slow());
    assert!(!session.synthetic);
    assert_eq!(session.ttfb.p50, Some(180.0));
    assert_eq!(session.lcp.p95, Some(900.0));
    assert_eq!(session.inp.p50, Some(120.0));
    let oracle = session
        .routes
        .iter()
        .find(|route| route.source == "oracle")
        .unwrap();
    assert_eq!(
        (oracle.route.as_str(), oracle.count, oracle.max),
        ("/stations", 1, 40)
    );
    assert_eq!(session.first_ns, T0);
    assert_eq!(session.last_ns, T0 + 6000 * MS);
    // The newest session comes first.
    assert_eq!(report.sessions[0].key, SessionKey::Sid(SID2.into()));
}

#[test]
fn a_session_search_keeps_only_that_session() {
    let report = build_report(fixture(), Some(&Search::Sid(SID2.into())));
    assert_eq!(report.sessions.len(), 1);
    assert_eq!(report.sessions[0].key, SessionKey::Sid(SID2.into()));
    assert_eq!(report.lines.len(), 1);
}

#[test]
fn visitors_without_a_session_are_told_apart_by_agent_and_synthetic_ones_are_marked() {
    let edge = edge_labels("person");
    let lab = edge_labels("lab");
    let lines = vec![
        parse_line(&edge, T0, &access("aaaaaaaa-1", "/", 200, 0.01, BROWSER)),
        parse_line(
            &edge,
            T0 + MS,
            &access("aaaaaaaa-2", "/", 500, 0.01, "curl/8.0"),
        ),
        parse_line(
            &edge,
            T0 + 2 * MS,
            &access("aaaaaaaa-3", "/", 200, 0.01, "5day4cast-synth/0.5"),
        ),
        parse_line(
            &lab,
            T0 + 3 * MS,
            &access(
                "aaaaaaaa-4",
                "/help",
                200,
                0.01,
                BROWSER.replace("131", "130").as_str(),
            ),
        ),
    ];
    let report = build_report(lines, None);
    assert_eq!(report.sessions.len(), 4);
    let by_agent = |agent: &str| {
        report
            .sessions
            .iter()
            .find(|s| s.agent.as_deref() == Some(agent))
            .unwrap()
    };
    assert!(!by_agent(BROWSER).synthetic);
    assert_eq!(by_agent("curl/8.0").errors, 1);
    assert!(by_agent("5day4cast-synth/0.5").synthetic);
    assert_eq!(report.sessions.iter().filter(|s| s.synthetic).count(), 2);
}

#[test]
fn searches_use_only_the_fixed_queries() {
    assert_eq!(
        Search::rid(RID).unwrap().query(),
        format!(r#"{{job=~"caddy-access|application-journal"}} |= "{RID}""#)
    );
    assert_eq!(
        Search::sid(SID).unwrap().query(),
        format!(r#"{{job="application-journal"}} |= "sid={SID}""#)
    );
    assert_eq!(
        Search::user_hex("ABCDEF0123456789ffff").unwrap().query(),
        r#"{job="application-journal"} |= "user=abcdef0123456789""#
    );
    assert_eq!(
        Search::Ip("2001:db8::1".parse().unwrap()).query(),
        r#"{job=~"caddy-access|application-journal"} |= "2001:db8::1""#
    );
    for bad in [
        r#"x" or "y"#,
        "short",
        "a".repeat(65).as_str(),
        "a b c d e f g h",
    ] {
        assert!(Search::rid(bad).is_none(), "{bad}");
    }
    for bad in [r#"abcdefghijklmnop""#, "abc", "abcdefghijklmnop|q"] {
        assert!(Search::sid(bad).is_none(), "{bad}");
    }
    assert!(Search::user_hex("abcdef").is_none());
    assert!(Search::user_hex("zzzzzzzzzzzzzzzz").is_none());
    assert_eq!(Window::parse("24h"), Some(Window::Day));
    assert_eq!(Window::parse("30d"), None);
    assert_eq!(Window::default(), Window::Day);
}

#[test]
fn explore_links_name_the_visitors_datasource() {
    let logs = VisitorLogs::from_settings(&LogsSettings {
        explore_url: Some("https://grafana.example.com/".into()),
        ..LogsSettings::default()
    });
    assert!(!logs.configured());
    let link = logs.explore_rid(RID, Window::Day).unwrap();
    let url = reqwest::Url::parse(&link).unwrap();
    assert_eq!(url.path(), "/explore");
    let panes: Value =
        serde_json::from_str(&url.query_pairs().find(|(k, _)| k == "panes").unwrap().1).unwrap();
    assert_eq!(panes["v"]["datasource"], "visitors");
    assert_eq!(
        panes["v"]["queries"][0]["expr"],
        format!(r#"{{job=~".+"}} |= "{RID}""#)
    );
    assert_eq!(panes["v"]["range"]["from"], "now-24h");
    assert_eq!(logs.explore_rid("bad rid", Window::Day), None);
    let unset = VisitorLogs::from_settings(&LogsSettings::default());
    assert_eq!(unset.explore_rid(RID, Window::Day), None);
}

#[test]
fn unsafe_logs_urls_are_refused() {
    for url in [
        "http://monitoring.example.com",
        "https://user:pw@monitoring.example.com",
        "https://monitoring.example.com/?q=1",
        "not a url",
    ] {
        let logs = VisitorLogs::from_settings(&LogsSettings {
            url: Some(url.into()),
            ..LogsSettings::default()
        });
        assert!(!logs.configured(), "{url}");
        assert!(logs.configuration_error.is_some(), "{url}");
    }
}

/// A `query_range` answer holding `lines` as (labels, ts, line), one stream per label set as
/// Loki answers.
fn answer(lines: &[(BTreeMap<String, String>, i64, String)]) -> Value {
    let mut streams: BTreeMap<String, (Value, Vec<Value>)> = BTreeMap::new();
    for (labels, ts, line) in lines {
        let key = serde_json::to_string(labels).unwrap();
        streams
            .entry(key)
            .or_insert_with(|| (serde_json::json!(labels), Vec::new()))
            .1
            .push(serde_json::json!([ts.to_string(), line]));
    }
    let result: Vec<Value> = streams
        .into_values()
        .map(|(labels, values)| serde_json::json!({ "stream": labels, "values": values }))
        .collect();
    serde_json::json!({ "status": "success", "data": { "resultType": "streams", "result": result } })
}

#[test]
fn answers_are_parsed_and_capped() {
    let lines: Vec<_> = (0..LINE_LIMIT + 5)
        .map(|i| {
            (
                app_labels("coordinator"),
                T0 + i as i64,
                http(RID, SID, "/", 200, 3),
            )
        })
        .collect();
    assert_eq!(parse_answer(&answer(&lines)).unwrap().len(), LINE_LIMIT);
    assert!(parse_answer(&serde_json::json!({ "status": "error" })).is_err());
}

/// Parsing and grouping a full answer of 2000 lines is a small part of the page's time;
/// Loki's own answer is most of it.
#[test]
fn a_full_answer_is_parsed_and_grouped_quickly() {
    let mut lines = Vec::new();
    for i in 0..LINE_LIMIT as i64 {
        let rid = format!("0199c1a2-0000-7000-8000-{i:012}");
        let sid = format!("session{:015}", i % 40);
        lines.push(match i % 4 {
            0 => (edge_labels("person"), T0 + i * MS, access(&rid, "/competitions", 200, 0.02, BROWSER)),
            1 => (app_labels("coordinator"), T0 + i * MS, http(&rid, &sid, "/competitions", 200, 20)),
            2 => (app_labels("coordinator"), T0 + i * MS, format!("[2026-10-07T18:49:22Z INFO] ui_event: ui_event site=5day4cast rid={rid} sid={sid} ip={IP} ev=click page=/ el=a id=- track=- text=Home")),
            _ => (app_labels("oracle"), T0 + i * MS, format!("[2026-10-07T18:49:22Z INFO] http: http rid=0199c1a2-1111-7000-8000-{i:012} prid={rid} sid=- ip=10.0.0.5 method=GET route=/x status=200 ms=4 user=-")),
        });
    }
    let body = serde_json::to_vec(&answer(&lines)).unwrap();
    let started = Instant::now();
    let parsed = parse_answer(&serde_json::from_slice(&body).unwrap()).unwrap();
    let report = build_report(parsed, None);
    let took = started.elapsed();
    println!(
        "parsed and grouped {} lines ({} bytes) into {} sessions in {took:?}",
        report.lines.len(),
        body.len(),
        report.sessions.len()
    );
    assert!(body.len() < MAX_BODY, "a full answer fits the body cap");
    assert_eq!(report.lines.len(), LINE_LIMIT);
    assert!(took < Duration::from_millis(400), "{took:?}");
}

#[derive(Clone, Default)]
struct Stub {
    asked: Arc<StdMutex<Vec<HashMap<String, String>>>>,
}

#[tokio::test]
async fn a_session_search_reads_the_session_then_its_addresses_edge_lines() {
    let stub = Stub::default();
    let asked = stub.asked.clone();
    let app = Router::new().route(
        "/api/datasources/proxy/uid/visitors/loki/api/v1/query_range",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let asked = asked.clone();
            async move {
                let query = params["query"].clone();
                asked.lock().unwrap().push(params);
                let lines: Vec<_> = fixture_raw()
                    .into_iter()
                    .filter(|(labels, _, line)| {
                        let wanted = query.rsplit("|= \"").next().unwrap().trim_end_matches('"');
                        let job_ok = labels["job"] == "application-journal"
                            || query.contains("caddy-access");
                        job_ok && line.contains(wanted)
                    })
                    .collect();
                Json(answer(&lines))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let logs = VisitorLogs::from_settings(&LogsSettings {
        url: Some(url),
        ..LogsSettings::default()
    });
    assert!(logs.configured());
    let search = Search::sid(SID2).unwrap();
    let report = logs.search(&search, Window::Hour).await.unwrap();
    assert_eq!(report.sessions.len(), 1);
    let session = &report.sessions[0];
    assert_eq!(session.key, SessionKey::Sid(SID2.into()));
    {
        let asked = stub.asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "the session, then its address");
        assert_eq!(asked[0]["query"], search.query());
        assert_eq!(asked[0]["limit"], "2000");
        assert_eq!(asked[0]["direction"], "backward");
        let start: i128 = asked[0]["start"].parse().unwrap();
        let end: i128 = asked[0]["end"].parse().unwrap();
        assert_eq!(end - start, 3600 * 1_000_000_000);
        assert_eq!(asked[1]["query"], Search::Ip(IP.parse().unwrap()).query());
    }
    // Answered again from the cache.
    logs.search(&search, Window::Hour).await.unwrap();
    assert_eq!(stub.asked.lock().unwrap().len(), 2);
    let unconfigured = VisitorLogs::from_settings(&LogsSettings::default());
    assert_eq!(
        unconfigured.search(&search, Window::Hour).await.err(),
        Some(SearchError::NotConfigured)
    );
}

/// The fixture's raw lines, for the stub to filter as Loki would.
fn fixture_raw() -> Vec<(BTreeMap<String, String>, i64, String)> {
    vec![
        (
            edge_labels("person"),
            T0,
            access(RID, "/competitions", 200, 0.0123, BROWSER),
        ),
        (
            app_labels("coordinator"),
            T0 + MS,
            http(RID, SID, "/competitions", 200, 11),
        ),
        (
            app_labels("coordinator"),
            T0 + 7000 * MS,
            http("0199c1a2-0000-7000-8000-00000000bbbb", SID2, "/", 200, 9),
        ),
        (
            edge_labels("person"),
            T0 + 6999 * MS,
            access(
                "0199c1a2-0000-7000-8000-00000000bbbb",
                "/",
                200,
                0.009,
                BROWSER,
            ),
        ),
    ]
}
