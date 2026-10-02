//! Create a competition from the dashboard: the form, and the handler that records it as a
//! `manual_competition` run, so the competition is made the way the scenarios make theirs and its
//! money is followed like theirs.
//!
//! The form is checked against the coordinator's own rules before anything is recorded: the
//! stations it takes, the seats a competition may have, the windows the oracle attests, and
//! whether entries are paused at the network fees now. What the coordinator still refuses is shown
//! in its own words. `synth run manual-competition` posts the same form.

use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::post,
    Json, Router,
};
use axum_extra::extract::Form;
use coordinator_core::keymeld::pools::MAX_POOL_PLAYERS;
use maud::{html, Markup};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::broadcast::error::RecvError;

use super::format;
use super::routes::{from_htmx, short_id, Dashboard};
use crate::events::Event;
use crate::scenarios::manual::{self, MANUAL_COMPETITION};
use crate::scenarios::{ArrivalPattern, ScenarioConfig};

/// Where the form posts.
pub(crate) const PATH: &str = "/api/competitions";

/// How long the handler waits for the coordinator to take or refuse the competition.
const CREATE_WAIT: Duration = Duration::from_secs(60);

/// The entry windows the form offers, in seconds, and the one it starts on.
const ENTRY_WINDOWS: [u64; 7] = [900, 1800, 3600, 7200, 21_600, 43_200, 86_400];
const DEFAULT_ENTRY_WINDOW: u64 = 3600;

const HALF_DAY: u64 = 43_200;
const DAY: u64 = 86_400;
const WEEK: u64 = 7 * DAY;

pub(super) fn router(state: Dashboard) -> Router {
    Router::new().route(PATH, post(create)).with_state(state)
}

/// What the form sends. Every field is text, so a value that does not parse is said plainly
/// rather than refused by the extractor.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct CreateForm {
    /// Picked from the configured stations; the field repeats once for each.
    #[serde(default)]
    pub stations: Vec<String>,
    /// More stations, separated by commas or spaces.
    #[serde(default)]
    pub other_stations: String,
    #[serde(default)]
    pub entry_fee: String,
    #[serde(default)]
    pub entry_window_secs: String,
    #[serde(default)]
    pub window_secs: String,
    #[serde(default)]
    pub seats: String,
    /// "listed" for the oracle's public list; unlisted otherwise.
    #[serde(default)]
    pub listed: String,
    #[serde(default)]
    pub players: String,
}

/// Whether the oracle attests a window of `seconds`: a full day or more, up to a week, or a
/// 12-hour half of a UTC day.
fn attested(seconds: u64) -> bool {
    (DAY..=WEEK).contains(&seconds) || seconds == HALF_DAY
}

fn window_label(seconds: u64) -> String {
    match seconds {
        s if s % DAY == 0 => format!("{} day{}", s / DAY, if s == DAY { "" } else { "s" }),
        s if s % 3600 == 0 => format!("{} hours", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

fn number<T: std::str::FromStr>(value: &str, field: &str, default: T) -> Result<T, String> {
    match value.trim() {
        "" => Ok(default),
        value => value
            .parse()
            .map_err(|_| format!("{field} must be a whole number, not {value:?}")),
    }
}

/// The run a form asks for, from `base`, the configured defaults, checked as the coordinator
/// checks a competition. `windows` are the configured observation windows, the only ones offered.
pub(crate) fn manual_config(
    form: &CreateForm,
    base: &ScenarioConfig,
    windows: &[u64],
    now: OffsetDateTime,
) -> Result<ScenarioConfig, String> {
    let mut stations: Vec<String> = Vec::new();
    for station in form.stations.iter().map(String::as_str).chain(
        form.other_stations
            .split(|c: char| c == ',' || c.is_whitespace()),
    ) {
        let station = station.trim().to_ascii_uppercase();
        if station.is_empty() || stations.contains(&station) {
            continue;
        }
        if station.len() > 16
            || !station
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(format!(
                "{station:?} is not a station ID: 1–16 letters, digits, hyphens or underscores"
            ));
        }
        stations.push(station);
    }
    if !(1..=50).contains(&stations.len()) {
        return Err("pick between 1 and 50 stations".into());
    }
    let entry_fee = number(&form.entry_fee, "the entry fee", base.entry_fee)?;
    if entry_fee == 0 {
        return Err("the entry fee must be at least 1 sat".into());
    }
    let entry_window = number(
        &form.entry_window_secs,
        "the entry window",
        DEFAULT_ENTRY_WINDOW,
    )?;
    if !(61..=DAY).contains(&entry_window) {
        return Err(
            "the entry window must be between 61 seconds and 24 hours: invoices close 60 seconds \
             before observations start"
                .into(),
        );
    }
    let first_attested = windows.iter().copied().find(|window| attested(*window));
    let window = match form.window_secs.trim() {
        "" => first_attested.ok_or(
            "none of the configured observation windows is one the oracle attests: a full day \
             (24 hours or more), or a day (12:00–24:00 UTC) or night (00:00–12:00 UTC) half",
        )?,
        _ => number(&form.window_secs, "the observation window", 0)?,
    };
    if !windows.contains(&window) {
        return Err(format!(
            "the observation window must be one of the configured ones: {}",
            windows
                .iter()
                .map(|window| window_label(*window))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !attested(window) {
        return Err(format!(
            "the oracle does not attest a {} window: the window must be a full day (24 hours or \
             more), or a day (12:00–24:00 UTC) or night (00:00–12:00 UTC) half",
            window_label(window)
        ));
    }
    let seats = number(&form.seats, "the seats", MAX_POOL_PLAYERS)?;
    if !(2..=MAX_POOL_PLAYERS).contains(&seats) {
        return Err(format!(
            "a competition has between 2 and {MAX_POOL_PLAYERS} seats"
        ));
    }
    let players = number(&form.players, "the synth players", 0)?;
    if players > seats {
        return Err(format!(
            "{players} synth players do not fit in {seats} seats"
        ));
    }

    let mut config = base.clone();
    config.stations = stations;
    config.entry_fee = entry_fee;
    config.entry_window_secs = entry_window;
    config.observation_window_secs = window;
    config.observation_window_choices.clear();
    // A half-day window starts on the half: entries stay open until the first one after the
    // entry window.
    config.observation_start = (window == HALF_DAY).then(|| {
        crate::runner::lanes::next_half(now + time::Duration::seconds(entry_window as i64))
    });
    if let Some(start) = config.observation_start {
        config.entry_window_secs = (start - now).whole_seconds().max(0) as u64;
    }
    config.seats = Some(seats);
    config.listed = form.listed.trim() == "listed";
    config.users = players;
    config.player_mix = None;
    config.min_players = None;
    config.competition_id = None;
    config.queue_players = None;
    config.max_pool_players = None;
    config.stress = None;
    // Synth's players come over the whole window, as people do.
    config.entry_timing.arrival_pattern = ArrivalPattern::Spread;
    Ok(config)
}

/// The coordinator's own words in a failed request's error, when it gave them as JSON.
fn coordinator_message(error: &str) -> String {
    error
        .find('{')
        .and_then(|start| serde_json::from_str::<serde_json::Value>(&error[start..]).ok())
        .and_then(|body| body.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| error.to_string())
}

/// The form, for the dashboard's header, where the live updates leave what was typed alone.
pub(super) fn form(state: &Dashboard) -> Markup {
    let base = &state.scenario_config;
    let windows = &state.observation_windows_secs;
    let default_window = windows.iter().copied().find(|window| attested(*window));
    html! {
        details.create {
            summary { "Create a competition" }
            form.create hx-post=(PATH) hx-target="#create-result"
                hx-confirm="Create this competition? Synth's players, if any, pay real entries from the payer's node." {
                label { "Stations"
                    select name="stations" multiple size=(base.stations.len().clamp(2, 6)) {
                        @for station in &base.stations { option value=(station) selected { (station) } }
                    }
                }
                label { "Other stations"
                    input type="text" name="other_stations" placeholder="KSEA, KBOS" autocomplete="off";
                }
                label { "Entry fee (sats)"
                    input type="number" name="entry_fee" min="1" value=(base.entry_fee) required;
                }
                label { "Entry window"
                    select name="entry_window_secs" {
                        @for seconds in ENTRY_WINDOWS {
                            option value=(seconds) selected[seconds == DEFAULT_ENTRY_WINDOW] { (window_label(seconds)) }
                        }
                    }
                }
                label { "Observation window"
                    select name="window_secs" {
                        @for window in windows {
                            option value=(window) selected[Some(*window) == default_window] disabled[!attested(*window)] {
                                (window_label(*window))
                                @if *window == HALF_DAY { " (a UTC half)" }
                                @if !attested(*window) { " (the oracle does not attest it)" }
                            }
                        }
                    }
                }
                label { "Seats"
                    input type="number" name="seats" min="2" max=(MAX_POOL_PLAYERS) value=(MAX_POOL_PLAYERS) required;
                }
                fieldset {
                    legend { "The oracle's public list" }
                    label { input type="radio" name="listed" value="unlisted" checked; " Unlisted" }
                    label { input type="radio" name="listed" value="listed"; " Listed" }
                }
                label { "Fill with synth players"
                    input type="number" name="players" min="0" max=(MAX_POOL_PLAYERS) value="0";
                }
                p.note {
                    "With 0 the competition is created and left open for people. Synth's players "
                    "come over the whole entry window, each paying " (format::sats(base.entry_fee as u64))
                    " sats and the ticket's fees."
                }
                button type="submit" { "Create competition" }
                p #create-result aria-live="polite" {}
            }
        }
    }
}

/// What the handler answers: a fragment for the form, or JSON for the CLI.
fn answer(htmx: bool, status: StatusCode, json: serde_json::Value, markup: Markup) -> Response {
    if htmx {
        // htmx swaps only successful responses in, so a refusal is said with a 200.
        return Html(markup.into_string()).into_response();
    }
    (status, Json(json)).into_response()
}

fn refused(htmx: bool, error: &str, run_id: Option<&str>) -> Response {
    answer(
        htmx,
        StatusCode::BAD_REQUEST,
        serde_json::json!({ "error": error, "run_id": run_id }),
        html! { span.error { (error) } },
    )
}

async fn create(
    State(state): State<Dashboard>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Response {
    let htmx = from_htmx(&headers);
    let now = OffsetDateTime::now_utc();
    let config = match manual_config(
        &form,
        &state.scenario_config,
        &state.observation_windows_secs,
        now,
    ) {
        Ok(config) => config,
        Err(error) => return refused(htmx, &error, None),
    };
    let runner = &state.runner;
    // The coordinator issues no ticket while entries are paused; do not send players into that.
    let paused = match runner.client().network_fee_quote().await {
        Ok(quote) => quote.paused(config.entry_fee as u64),
        Err(_) if config.users == 0 => None,
        Err(error) => {
            return refused(
                htmx,
                &format!("cannot tell whether entries are paused: {error:#}"),
                None,
            )
        }
    };
    if let (Some(paused), true) = (paused, config.users > 0) {
        return refused(
            htmx,
            &format!("{paused}; create it without synth players, or try again later"),
            None,
        );
    }
    let run_id = match runner.record_run(MANUAL_COMPETITION, &config).await {
        Ok(run_id) => run_id,
        Err(error) => return refused(htmx, &format!("{error:#}"), None),
    };
    let competition_id = runner
        .db()
        .get_run(&run_id)
        .await
        .ok()
        .flatten()
        .and_then(|run| run.config_json)
        .and_then(|config| serde_json::from_str::<ScenarioConfig>(&config).ok())
        .and_then(|config| config.competition_id);

    // Listening before the run starts, so its creation is not missed.
    let mut events = runner.events().subscribe();
    let running = runner.clone();
    let started = run_id.clone();
    tokio::spawn(async move {
        if let Err(e) = running.run_recorded(started, MANUAL_COMPETITION).await {
            log::error!("Manual competition run failed: {e:#}");
        }
    });
    let created = tokio::time::timeout(CREATE_WAIT, async {
        loop {
            let steps = runner.db().get_steps(&run_id).await.unwrap_or_default();
            if let Some(created) = manual::created(&steps) {
                return created;
            }
            match events.recv().await {
                Ok(Event::RunFinished {
                    run_id: finished, ..
                }) if finished == run_id => {
                    let steps = runner.db().get_steps(&run_id).await.unwrap_or_default();
                    if let Some(created) = manual::created(&steps) {
                        return created;
                    }
                    let run = runner.db().get_run(&run_id).await.ok().flatten();
                    return Err(run
                        .and_then(|run| run.error_message)
                        .unwrap_or_else(|| "the run ended before creating it".into()));
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return Err("synth is shutting down".into()),
            }
        }
    })
    .await;
    let run_link = format!("/runs/{run_id}");
    match created {
        Ok(Err(error)) => refused(
            htmx,
            &format!(
                "The coordinator refused it: {}",
                coordinator_message(&error)
            ),
            Some(&run_id),
        ),
        Err(_) => answer(
            htmx,
            StatusCode::ACCEPTED,
            serde_json::json!({
                "status": "pending",
                "scenario": MANUAL_COMPETITION,
                "run_id": run_id,
                "competition_id": competition_id,
            }),
            html! {
                "Recorded run " a href=(run_link) { (short_id(&run_id)) }
                "; the coordinator has not answered yet. Its page says when it does."
            },
        ),
        Ok(Ok(())) => {
            let competition = competition_id.map(|id| id.to_string()).unwrap_or_default();
            let link = format!(
                "{}/competitions/{competition}/leaderboard",
                runner.client().base_url()
            );
            let closes = now + time::Duration::seconds(config.entry_window_secs as i64);
            answer(
                htmx,
                StatusCode::OK,
                serde_json::json!({
                    "status": "created",
                    "scenario": MANUAL_COMPETITION,
                    "run_id": run_id,
                    "competition_id": competition,
                    "link": link,
                    "players": config.users,
                    "paused": paused,
                }),
                html! {
                    "Created competition " a href=(link) rel="noreferrer" { code { (competition) } }
                    " (run " a href=(run_link) { (short_id(&run_id)) } "). "
                    @if config.users > 0 {
                        (config.users) " synth players enter it before entries close "
                        (format::when(closes, now)) "."
                    } @else {
                        "It is open for people until " (format::when(closes, now)) "."
                    }
                    @if let Some(paused) = paused { " " span.error { (paused) "." } }
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::EntryBehavior;
    use axum::{
        body::Body,
        extract::State as Shared,
        http::{header, Request},
        routing::get,
    };
    use std::sync::{Arc, Mutex};
    use time::macros::datetime;
    use tower::ServiceExt;

    const WINDOWS: [u64; 4] = [7200, HALF_DAY, DAY, 2 * DAY];

    fn form(fields: &[(&str, &str)]) -> CreateForm {
        let body = fields
            .iter()
            .map(|(key, value)| format!("{key}={}", value.replace(' ', "+")))
            .collect::<Vec<_>>()
            .join("&");
        serde_html_form_like(&body)
    }

    /// The form as the extractor reads it, repeated stations included.
    fn serde_html_form_like(body: &str) -> CreateForm {
        let request = Request::post("/")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            use axum::extract::FromRequest;
            Form::<CreateForm>::from_request(request, &())
                .await
                .unwrap()
                .0
        })
    }

    #[test]
    fn the_form_is_checked_as_the_coordinator_checks_a_competition() {
        let base = ScenarioConfig::default();
        let now = datetime!(2026-10-01 10:30 UTC);
        let config = manual_config(
            &form(&[
                ("stations", "KDEN"),
                ("stations", "KJFK"),
                ("other_stations", "ksea, KDEN kbos"),
                ("entry_fee", "5000"),
                ("entry_window_secs", "3600"),
                ("window_secs", "86400"),
                ("seats", "10"),
                ("listed", "listed"),
                ("players", "4"),
            ]),
            &base,
            &WINDOWS,
            now,
        )
        .unwrap();
        assert_eq!(config.stations, ["KDEN", "KJFK", "KSEA", "KBOS"]);
        assert_eq!((config.entry_fee, config.entry_window_secs), (5000, 3600));
        assert_eq!(config.observation_window_secs, DAY);
        assert_eq!(
            (config.seats, config.listed, config.users),
            (Some(10), true, 4)
        );
        assert_eq!(config.entry_timing.arrival_pattern, ArrivalPattern::Spread);

        // Unset, the defaults: unlisted, no players, a full pool, the first attested window.
        let defaults = manual_config(&form(&[("stations", "KDEN")]), &base, &WINDOWS, now).unwrap();
        assert_eq!(
            (defaults.users, defaults.seats, defaults.listed),
            (0, Some(MAX_POOL_PLAYERS), false)
        );
        assert_eq!(defaults.entry_fee, base.entry_fee);
        assert_eq!(defaults.entry_window_secs, DEFAULT_ENTRY_WINDOW);
        assert_eq!(defaults.observation_window_secs, HALF_DAY);
        // A half-day window starts on the half after the entry window: noon, from 10:30.
        assert_eq!(
            defaults.observation_start,
            Some(datetime!(2026-10-01 12:00 UTC))
        );
        assert_eq!(defaults.entry_window_secs, 5400);

        for (fields, error) in [
            (vec![], "between 1 and 50 stations"),
            (vec![("other_stations", "K DEN!")], "is not a station ID"),
            (
                vec![("stations", "KDEN"), ("entry_fee", "0")],
                "at least 1 sat",
            ),
            (
                vec![("stations", "KDEN"), ("entry_fee", "lots")],
                "whole number",
            ),
            (
                vec![("stations", "KDEN"), ("entry_window_secs", "60")],
                "between 61 seconds and 24 hours",
            ),
            (
                vec![("stations", "KDEN"), ("window_secs", "7200")],
                "does not attest a 2 hours window",
            ),
            (
                vec![("stations", "KDEN"), ("window_secs", "259200")],
                "one of the configured ones",
            ),
            (
                vec![("stations", "KDEN"), ("seats", "26")],
                "between 2 and 25 seats",
            ),
            (
                vec![("stations", "KDEN"), ("seats", "3"), ("players", "4")],
                "do not fit in 3 seats",
            ),
        ] {
            let refused = manual_config(&form(&fields), &base, &WINDOWS, now).unwrap_err();
            assert!(refused.contains(error), "{fields:?}: {refused}");
        }
        // Configured windows the oracle does not attest leave nothing to choose by default.
        let refused =
            manual_config(&form(&[("stations", "KDEN")]), &base, &[7200, 600], now).unwrap_err();
        assert!(refused.contains("none of the configured observation windows"));
    }

    #[test]
    fn the_coordinators_words_are_taken_from_its_json() {
        assert_eq!(
            coordinator_message(
                r#"Create competition failed (400 Bad Request): {"error":"total_allowed_entries must be between 2 and 25"}"#
            ),
            "total_allowed_entries must be between 2 and 25"
        );
        assert_eq!(coordinator_message("timed out"), "timed out");
    }

    #[test]
    fn a_manual_run_spreads_its_players_and_may_have_none() {
        let mut config = manual_config(
            &form(&[
                ("stations", "KDEN"),
                ("players", "6"),
                ("window_secs", "86400"),
            ]),
            &ScenarioConfig::default(),
            &WINDOWS,
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        let plan = config.resolve_plan(MANUAL_COMPETITION).unwrap();
        assert_eq!(plan.entry_plan.len(), 6);
        assert!(plan
            .entry_plan
            .iter()
            .all(|entry| entry.behavior == EntryBehavior::Complete));
        let latest = plan
            .entry_timing
            .latest_spread_arrival(plan.entry_window_secs)
            .unwrap();
        assert!(plan
            .entry_plan
            .iter()
            .all(|entry| entry.arrival_secs <= latest));
        config.users = 0;
        assert!(config
            .resolve_plan(MANUAL_COMPETITION)
            .unwrap()
            .entry_plan
            .is_empty());
        // Other scenarios still need players.
        assert!(config.resolve_plan("full_lifecycle").is_err());
    }

    /// Requests a fake coordinator saw, by path, with the bodies of competitions it was asked
    /// to create.
    #[derive(Clone, Default)]
    struct Seen {
        paths: Arc<Mutex<Vec<String>>>,
        created: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    async fn coordinator(refuse: bool) -> (String, Seen, tokio::task::JoinHandle<()>) {
        async fn fee() -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "enabled": true, "network_fee_sats": 10, "sat_per_vb": 1.0, "conf_target": 6,
                "pool_players": 25, "multiplier_percent": 100, "pause_above_entry_bps": 1000,
                "arkade_unavailable": false,
            }))
        }
        let seen = Seen::default();
        let created = move |Shared(seen): Shared<Seen>, Json(body): Json<serde_json::Value>| async move {
            let id = body["id"].clone();
            seen.created.lock().unwrap().push(body);
            if refuse {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(
                        serde_json::json!({ "error": "A competition needs at least 5 players while Bitcoin network fees are above 2 sat/vB (now 9 sat/vB)" }),
                    ),
                );
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": id, "created_at": "2026-10-01T00:00:00Z", "event_submission": {}
                })),
            )
        };
        let app = Router::new()
            .route("/api/v1/competitions", post(created))
            .route("/api/v1/network-fee", get(fee))
            .fallback(
                |Shared(seen): Shared<Seen>, request: Request<Body>| async move {
                    seen.paths
                        .lock()
                        .unwrap()
                        .push(request.uri().path().to_string());
                    StatusCode::NOT_FOUND
                },
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen, server)
    }

    async fn dashboard_at(url: &str, directory: &tempfile::TempDir) -> Dashboard {
        let db = crate::db::SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let mut dashboard = Dashboard::for_tests(db.clone());
        dashboard.runner = crate::runner::Runner::new(
            crate::client::CoordinatorClient::new(url, None),
            db,
            crate::events::Events::new(),
        );
        dashboard.observation_windows_secs = WINDOWS.to_vec();
        dashboard
    }

    async fn post_form(dashboard: Dashboard, body: &str) -> (StatusCode, serde_json::Value) {
        let response = router(dashboard)
            .oneshot(
                Request::post(PATH)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// With no players asked for, the competition is created, recorded as a run of its own kind
    /// that the money trail follows, and nobody enters it.
    #[tokio::test]
    async fn a_competition_with_no_players_is_created_and_recorded_with_no_entries() {
        let (url, seen, server) = coordinator(false).await;
        let directory = tempfile::tempdir().unwrap();
        let dashboard = dashboard_at(&url, &directory).await;
        let db = dashboard.runner.db().clone();
        let (status, answer) = post_form(
            dashboard,
            "stations=KDEN&stations=KJFK&entry_fee=2000&entry_window_secs=3600&window_secs=86400&seats=8&listed=listed&players=0",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["status"], "created");
        let run_id = answer["run_id"].as_str().unwrap();
        let competition_id = answer["competition_id"].as_str().unwrap();
        assert!(answer["link"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/competitions/{competition_id}/leaderboard")));

        // The run ends once the competition is made.
        let run = loop {
            let run = db.get_run(run_id).await.unwrap().unwrap();
            if run.status != "running" {
                break run;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            (run.scenario.as_str(), run.status.as_str()),
            (MANUAL_COMPETITION, "passed")
        );
        assert_eq!(run.competition_id.as_deref(), Some(competition_id));
        let plan: ScenarioConfig =
            serde_json::from_str(run.config_json.as_deref().unwrap()).unwrap();
        assert_eq!((plan.users, plan.seats, plan.listed), (0, Some(8), true));
        assert!(plan.entry_plan.is_empty());
        let steps = db.get_steps(run_id).await.unwrap();
        assert_eq!(
            steps
                .iter()
                .map(|step| step.step_name.as_str())
                .collect::<Vec<_>>(),
            ["create_competition"]
        );
        assert!(crate::trail::tracker::entries_of(&steps).is_empty());
        let created = seen.created.lock().unwrap().clone();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["id"].as_str(), Some(competition_id));
        assert_eq!(created[0]["total_allowed_entries"], 8);
        assert_eq!(created[0]["entry_fee"], 2000);
        assert_eq!(created[0]["unlisted"], false);
        assert_eq!(created[0]["locations"], serde_json::json!(["KDEN", "KJFK"]));
        // No ticket, entry or anything else was asked for.
        assert!(seen.paths.lock().unwrap().is_empty(), "{:?}", seen.paths);
        server.abort();
    }

    /// The coordinator's refusal is shown in its words, and a bad form records nothing.
    #[tokio::test]
    async fn refusals_are_shown_and_a_bad_form_records_no_run() {
        let (url, _, server) = coordinator(true).await;
        let directory = tempfile::tempdir().unwrap();
        let dashboard = dashboard_at(&url, &directory).await;
        let db = dashboard.runner.db().clone();

        let (status, answer) = post_form(dashboard.clone(), "stations=KDEN&seats=40").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(answer["error"]
            .as_str()
            .unwrap()
            .contains("between 2 and 25 seats"));
        assert!(db.list_runs(10).await.unwrap().is_empty());

        let (status, answer) =
            post_form(dashboard, "stations=KDEN&window_secs=86400&seats=2").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            answer["error"],
            "The coordinator refused it: A competition needs at least 5 players while Bitcoin \
             network fees are above 2 sat/vB (now 9 sat/vB)"
        );
        let runs = db.list_runs(10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(answer["run_id"].as_str(), Some(runs[0].id.as_str()));
        server.abort();
    }
}
