//! Keep a competition open for visitors: one anyone can enter, with time left to do it.
//!
//! Every check lists the coordinator's competitions and counts those that are listed, still take
//! entries, have room, and close their entries at least `min_minutes_left` from now. The
//! keep-open lane's next run starts `start_ahead_minutes` before the last of them stops
//! counting, so the next competition is up before that one goes, unless that lane started a run
//! in the last `min_minutes_left`: its competition can take that long to show up. While the
//! coordinator would refuse tickets anyway, as it does while the Arkade network recovers, no run
//! is started; the next check tries again.
//!
//! The check also loads the entry form of each open competition as a visitor's browser does
//! (see [`crate::client::visitor`]): one listed as open whose form has lines without a forecast
//! is one nobody but synth can enter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use log::{error, info, warn};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::client::competitions::CompetitionResponse;
use crate::client::CoordinatorClient;
use crate::config::KeepOpenConfig;
use crate::db::SynthDb;

/// The name a failed check of the entry forms is recorded under in the run history.
pub const VISITOR_FORM_CHECK: &str = "visitor_entry_form";

/// How often the open competitions' entry forms are loaded while all of them can be entered. One
/// that is new, or that cannot be entered, is loaded at the next check.
const FORMS_EVERY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Checks in a row that find a form unenterable before it is recorded as a failed check: its
/// forecasts may only be late.
const BLOCKED_CHECKS: u32 = 2;

/// The competitions open for visitors at a check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Open {
    pub competitions: usize,
    /// The longest time left among them before entries close, in minutes; 0 when none.
    pub minutes_left: i64,
}

/// The competitions in `listed` that anyone can enter and that close their entries at least
/// `min_left` after `now`, each with the time it has left.
fn open_to_visitors(
    listed: &[CompetitionResponse],
    now: OffsetDateTime,
    min_left: Duration,
) -> Vec<(&CompetitionResponse, Duration)> {
    listed
        .iter()
        .filter(|competition| competition.listed() && competition.open_to_enter(now))
        .filter_map(|competition| Some((competition, competition.entries_close()? - now)))
        .filter(|(_, left)| *left >= min_left)
        .collect()
}

/// Count the competitions in `listed` that anyone can enter and that close their entries at
/// least `min_left` after `now`.
pub fn open_competitions(
    listed: &[CompetitionResponse],
    now: OffsetDateTime,
    min_left: Duration,
) -> Open {
    let open = open_to_visitors(listed, now, min_left);
    Open {
        competitions: open.len(),
        minutes_left: open
            .iter()
            .map(|(_, left)| *left)
            .max()
            .map_or(0, |left| left.whole_minutes()),
    }
}

/// What visitors meet on the entry forms of the competitions open for them, as last loaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Forms {
    /// The open competitions whose forms were loaded.
    pub checked: usize,
    /// Of them, those where every pick can be made.
    pub enterable: usize,
    /// Forecast lines without a forecast, over all of them.
    pub missing_forecasts: usize,
}

impl Forms {
    /// A visitor can enter every open competition.
    pub fn all_enterable(&self) -> bool {
        self.enterable == self.checked
    }
}

/// What the dashboard shows about the competitions open for visitors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct OpenStatus {
    pub open: Open,
    /// A run was started to keep one open, and its competition is not open yet.
    pub starting: bool,
    /// What their entry forms show a visitor; None until they have been loaded.
    pub forms: Option<Forms>,
}

/// An entry form found unenterable: for how many checks in a row, and whether that is in the
/// run history yet.
#[derive(Debug, Clone, Copy, Default)]
struct Blocked {
    checks: u32,
    recorded: bool,
}

/// The latest keep-open check, shared with the dashboard. None before the first, or without a
/// keep-open lane.
pub type SharedOpenStatus = Arc<Mutex<Option<OpenStatus>>>;

/// Starts requested by keep-open and the last run started by its lane.
#[derive(Default)]
pub struct LaneStart {
    pub notify: tokio::sync::Notify,
    last: Mutex<Option<tokio::time::Instant>>,
    /// The lane's last run created no competition, which keep-open has not yet heard.
    start_failed: std::sync::atomic::AtomicBool,
}

impl LaneStart {
    pub fn record(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(tokio::time::Instant::now());
    }

    pub fn recent(&self, grace: std::time::Duration) -> bool {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|at| at.elapsed() < grace)
    }

    /// Whether the lane started a run after `at`.
    pub fn started_since(&self, at: tokio::time::Instant) -> bool {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|last| last > at)
    }

    /// The lane's run created no competition: it no longer counts as a recent start, so the next
    /// check starts another if nothing else is open.
    pub fn failed(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.start_failed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the lane's run failed since this was last asked.
    fn take_failed(&self) -> bool {
        self.start_failed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

/// Decides, check by check, when the keep-open lane's next run starts early.
pub struct KeepOpen {
    config: KeepOpenConfig,
    /// What a ticket of the lane's runs costs before fees, to tell if fees pause entries.
    entry_fee: u64,
    last_start: Option<tokio::time::Instant>,
    lane_start: Arc<LaneStart>,
    /// The pause is logged when it starts, not at every check.
    paused: bool,
    /// The competitions the last check found open, and their stations.
    open_now: Vec<(Uuid, Vec<String>)>,
    /// What their entry forms last showed, and when they were last loaded.
    forms: Option<Forms>,
    forms_loaded: Option<tokio::time::Instant>,
    /// The open competitions whose forms were loaded, and those found unenterable.
    form_seen: HashMap<Uuid, Option<Blocked>>,
}

impl KeepOpen {
    pub fn new(config: KeepOpenConfig, entry_fee: u64, lane_start: Arc<LaneStart>) -> Self {
        Self {
            config,
            entry_fee,
            last_start: None,
            lane_start,
            paused: false,
            open_now: Vec::new(),
            forms: None,
            forms_loaded: None,
            form_seen: HashMap::new(),
        }
    }

    fn min_left(&self) -> Duration {
        Duration::minutes(self.config.min_minutes_left as i64)
    }

    /// How long the last open competition must still count as open for the next run to wait.
    fn start_ahead(&self) -> Duration {
        Duration::minutes(self.config.start_ahead_minutes as i64)
    }

    /// Check once: what is open, and whether to start a run now.
    pub async fn check(
        &mut self,
        client: &CoordinatorClient,
    ) -> anyhow::Result<(OpenStatus, bool)> {
        let now = OffsetDateTime::now_utc();
        let listed = client.list_competitions().await?;
        let open = open_competitions(&listed, now, self.min_left());
        // Those with long enough left that the next run need not start yet.
        let lasting = open_competitions(&listed, now, self.min_left() + self.start_ahead());
        self.open_now = open_to_visitors(&listed, now, self.min_left())
            .into_iter()
            .map(|(competition, _)| (competition.id, competition.stations()))
            .collect();
        crate::server::metrics::record_open(open.competitions, open.minutes_left);
        let grace = std::time::Duration::from_secs(self.config.min_minutes_left * 60);
        // A start that created no competition is not one to wait for.
        if self.lane_start.take_failed() {
            self.last_start = None;
        }
        let recently =
            self.last_start.is_some_and(|at| at.elapsed() < grace) || self.lane_start.recent(grace);
        let forms = self.forms;
        if lasting.competitions > 0 || recently {
            return Ok((
                OpenStatus {
                    open,
                    starting: lasting.competitions == 0,
                    forms,
                },
                false,
            ));
        }
        if let Some(reason) = client.entries_paused(self.entry_fee).await? {
            if !self.paused {
                info!("No competition stays open for visitors, but the coordinator refuses tickets now; trying again at the next check: {reason}");
            }
            self.paused = true;
            return Ok((
                OpenStatus {
                    open,
                    starting: false,
                    forms,
                },
                false,
            ));
        }
        if self.paused {
            info!("The coordinator takes tickets again");
        }
        self.paused = false;
        self.last_start = Some(tokio::time::Instant::now());
        crate::server::metrics::record_keep_open_start();
        if open.competitions == 0 {
            info!(
                "No competition is open for visitors; lane {} starts its next run now",
                self.config.lane
            );
        } else {
            info!(
                "The last competition open for visitors stops counting in under {} minutes; lane \
                 {} starts its next run now",
                self.config.start_ahead_minutes, self.config.lane
            );
        }
        Ok((
            OpenStatus {
                open,
                starting: true,
                forms,
            },
            true,
        ))
    }

    /// Whether the open competitions' entry forms are due a look: one is new, one could not be
    /// entered at the last look, or the last look is [`FORMS_EVERY`] old.
    fn forms_due(&self) -> bool {
        self.open_now.len() != self.form_seen.len()
            || self
                .forms_loaded
                .is_none_or(|at| at.elapsed() >= FORMS_EVERY)
            || self
                .open_now
                .iter()
                .any(|(id, _)| self.form_seen.get(id).is_none_or(Option::is_some))
    }

    /// Load the entry form of each open competition as a visitor's browser does, set the gauges
    /// from what they show, and record a form found unenterable [`BLOCKED_CHECKS`] times in a row
    /// as a failed check in the run history, once.
    pub async fn check_forms(&mut self, client: &CoordinatorClient, db: &SynthDb) -> Option<Forms> {
        if self.open_now.is_empty() {
            self.form_seen.clear();
            self.forms = None;
            crate::server::metrics::record_entry_forms(None);
            return None;
        }
        let mut forms = Forms::default();
        let mut seen = HashMap::new();
        for (id, stations) in &self.open_now {
            let form = match client.entry_form(id).await {
                Ok(form) => form,
                Err(error) => {
                    warn!("Cannot load competition {id}'s entry form: {error:#}");
                    crate::client::visitor::EntryForm::Unavailable(
                        "the coordinator could not be reached".into(),
                    )
                }
            };
            let check = form.check(stations);
            forms.checked += 1;
            forms.missing_forecasts += check.missing;
            let Some(why) = check.blocked else {
                forms.enterable += 1;
                seen.insert(*id, None);
                continue;
            };
            let mut blocked = self
                .form_seen
                .get(id)
                .copied()
                .flatten()
                .unwrap_or_default();
            blocked.checks += 1;
            if blocked.checks >= BLOCKED_CHECKS && !blocked.recorded {
                blocked.recorded = true;
                record_blocked_form(db, id, &why).await;
            }
            seen.insert(*id, Some(blocked));
        }
        self.form_seen = seen;
        self.forms_loaded = Some(tokio::time::Instant::now());
        // With no form loaded, the coordinator did not answer: what was last seen stands.
        if forms.checked > 0 {
            self.forms = Some(forms);
            crate::server::metrics::record_entry_forms(Some(forms));
        }
        self.forms
    }

    /// Check every `check_interval_secs`, waking the lane through `lane_start` when a run is needed,
    /// loading the open competitions' entry forms when they are due, and keeping `status` current
    /// for the dashboard.
    pub async fn run(mut self, client: CoordinatorClient, db: SynthDb, status: SharedOpenStatus) {
        let mut checks = tokio::time::interval(std::time::Duration::from_secs(
            self.config.check_interval_secs,
        ));
        checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let publish = |open: OpenStatus| {
            *status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(open);
        };
        loop {
            checks.tick().await;
            match self.check(&client).await {
                Ok((open, starting)) => {
                    publish(open);
                    if starting {
                        self.lane_start.notify.notify_one();
                    }
                    if self.forms_due() {
                        let forms = self.check_forms(&client, &db).await;
                        publish(OpenStatus { forms, ..open });
                    }
                }
                Err(e) => {
                    warn!("Cannot check for a competition open for visitors: {e:#}");
                    self.forms = None;
                    self.forms_loaded = None;
                    crate::server::metrics::record_entry_forms(None);
                    *status
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                }
            }
        }
    }
}

/// Record that visitors cannot enter competition `id` as a failed check in the run history and
/// the metrics, saying `why`.
async fn record_blocked_form(db: &SynthDb, id: &Uuid, why: &str) {
    let why = format!("Visitors cannot enter competition {id}: {why}");
    error!("{why}");
    crate::server::metrics::record_scenario(
        VISITOR_FORM_CHECK,
        false,
        0,
        &[("entry_form".to_string(), 0)],
    );
    if let Err(error) = db
        .record_failed_start(VISITOR_FORM_CHECK, None, "entry_form", &why)
        .await
    {
        error!("Cannot record the failed entry form check: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn competition(json: serde_json::Value) -> CompetitionResponse {
        let mut base = serde_json::json!({
            "id": uuid::Uuid::now_v7(),
            "created_at": "2026-10-01T00:00:00Z",
        });
        base.as_object_mut()
            .unwrap()
            .extend(json.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    /// While the coordinator refuses tickets, no run starts; the next check tries again, and
    /// once a run started, none starts again until its competition has had time to show up.
    #[tokio::test]
    async fn a_start_refused_while_entries_are_paused_is_tried_at_the_next_check() {
        use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        async fn fee(State(asked): State<Arc<AtomicUsize>>) -> (StatusCode, String) {
            match asked.fetch_add(1, Ordering::SeqCst) {
                0 => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    crate::client::competitions::ENTRIES_PAUSED_FOR_ARKADE.into(),
                ),
                _ => (
                    StatusCode::OK,
                    serde_json::json!({
                        "enabled": true,
                        "network_fee_sats": 30,
                        "sat_per_vb": 1.0,
                        "pause_above_entry_bps": 5000,
                        "arkade_unavailable": false,
                    })
                    .to_string(),
                ),
            }
        }
        let asked = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/api/v1/competitions",
                get(|| async { Json(Vec::<serde_json::Value>::new()) }),
            )
            .route("/api/v1/network-fee", get(fee))
            .with_state(asked.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = CoordinatorClient::new(&url, None);
        let mut keep = KeepOpen::new(
            KeepOpenConfig {
                lane: "open".into(),
                min_minutes_left: 30,
                check_interval_secs: 60,
                start_ahead_minutes: 5,
            },
            1000,
            Arc::new(LaneStart::default()),
        );

        let (status, start) = keep.check(&client).await.unwrap();
        assert!(!start, "entries are paused");
        assert_eq!(status, OpenStatus::default());
        assert!(keep.paused);
        let (status, start) = keep.check(&client).await.unwrap();
        assert!(start, "tried again at the next check");
        assert!(status.starting && !keep.paused);
        // Started: the next check waits for its competition rather than starting another.
        let (status, start) = keep.check(&client).await.unwrap();
        assert!(!start && status.starting);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        // A regular lane start gets the same grace period, without a keep-open request.
        keep.last_start = None;
        keep.lane_start.record();
        let (status, start) = keep.check(&client).await.unwrap();
        assert!(!start && status.starting);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        // A start that created no competition does not hold the next one back.
        keep.lane_start.failed();
        let (status, start) = keep.check(&client).await.unwrap();
        assert!(start && status.starting, "tried again at the next check");
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        server.abort();
    }

    fn closing(at: OffsetDateTime) -> String {
        at.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    fn config(start_ahead_minutes: u64) -> KeepOpenConfig {
        KeepOpenConfig {
            lane: "open".into(),
            min_minutes_left: 30,
            check_interval_secs: 60,
            start_ahead_minutes,
        }
    }

    /// The next run starts while the last open competition still counts as open, so there is no
    /// moment without one.
    #[tokio::test]
    async fn the_next_run_starts_before_the_last_open_competition_stops_counting() {
        use axum::{routing::get, Json, Router};

        let close = OffsetDateTime::now_utc() + Duration::minutes(33);
        let listed = serde_json::json!([{
            "id": uuid::Uuid::now_v7(),
            "created_at": "2026-10-01T00:00:00Z",
            "kind": "queued",
            "event_submission": {
                "start_observation_date": closing(close),
                "locations": ["KSTS"],
            },
            "pool_rules": { "min_players": 2, "max_players": 25 },
            "entries": 3,
            "max_entries": 100,
            "total_entries": 3,
        }]);
        let app = Router::new()
            .route(
                "/api/v1/competitions",
                get(move || {
                    let listed = listed.clone();
                    async move { Json(listed) }
                }),
            )
            .route(
                "/api/v1/network-fee",
                get(|| async { Json(serde_json::json!({ "enabled": false })) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = CoordinatorClient::new(&url, None);

        // With thirty-three minutes left it is open, and without a head start nothing is due.
        let mut waits = KeepOpen::new(config(0), 1000, Arc::new(LaneStart::default()));
        let (status, start) = waits.check(&client).await.unwrap();
        assert!(!start && !status.starting);
        assert_eq!(status.open.competitions, 1);
        // Five minutes ahead, it stops counting too soon to wait for.
        let mut ahead = KeepOpen::new(config(5), 1000, Arc::new(LaneStart::default()));
        let (status, start) = ahead.check(&client).await.unwrap();
        assert!(start && status.starting);
        assert_eq!(
            status.open.competitions, 1,
            "still open while the next one starts"
        );
        assert_eq!(ahead.open_now.len(), 1);
        assert_eq!(ahead.open_now[0].1, ["KSTS"]);
        // Its competition is given time to show up before another run starts.
        let (status, start) = ahead.check(&client).await.unwrap();
        assert!(!start && status.starting);
        server.abort();
    }

    /// A competition listed as open whose form lets nobody pick is not one a visitor can enter:
    /// the second check in a row to find it so records it in the run history, once.
    #[tokio::test]
    async fn an_open_competition_nobody_can_enter_is_recorded_as_a_failed_check() {
        use axum::{routing::get, Router};
        use std::sync::atomic::{AtomicBool, Ordering};

        let fixed = Arc::new(AtomicBool::new(false));
        let forecasts = fixed.clone();
        let app = Router::new().route(
            "/competitions/{id}/entry-forecasts",
            get(move || {
                let fixed = forecasts.clone();
                async move {
                    let (forecast, disabled) = if fixed.load(Ordering::SeqCst) {
                        ("<strong class=\"pick-forecast\">98</strong>", "")
                    } else {
                        (
                            "<span class=\"pick-forecast is-missing\">no forecast yet</span>",
                            " disabled",
                        )
                    };
                    format!(
                        "<div id=\"entryForecasts\"><fieldset data-station=\"KSTS\">\
                         <div class=\"pick-row\">{forecast}<input type=\"radio\" \
                         name=\"KSTS_temp_high\" value=\"over\"{disabled}></div></fieldset></div>"
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = CoordinatorClient::new(&url, None);
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let mut keep = KeepOpen::new(config(5), 1000, Arc::new(LaneStart::default()));

        assert!(keep.forms_due(), "never loaded");
        assert_eq!(keep.check_forms(&client, &db).await, None, "none is open");
        keep.open_now = vec![(Uuid::now_v7(), vec!["KSTS".to_string()])];
        let blocked = Forms {
            checked: 1,
            enterable: 0,
            missing_forecasts: 1,
        };
        assert_eq!(keep.check_forms(&client, &db).await, Some(blocked));
        assert!(
            db.list_runs(10).await.unwrap().is_empty(),
            "its forecasts may only be late"
        );
        assert!(keep.forms_due(), "loaded again at the next check");
        assert_eq!(keep.check_forms(&client, &db).await, Some(blocked));
        assert_eq!(keep.check_forms(&client, &db).await, Some(blocked));
        let runs = db.list_runs(10).await.unwrap();
        assert_eq!(runs.len(), 1, "recorded once");
        assert_eq!(
            (runs[0].scenario.as_str(), runs[0].status.as_str()),
            (VISITOR_FORM_CHECK, "failed")
        );
        let why = runs[0].error_message.clone().unwrap();
        assert!(why.contains("1 of 1 forecast lines"), "{why}");
        assert!(why.contains("KSTS_temp_high"), "{why}");

        fixed.store(true, Ordering::SeqCst);
        let ready = Forms {
            checked: 1,
            enterable: 1,
            missing_forecasts: 0,
        };
        assert_eq!(keep.check_forms(&client, &db).await, Some(ready));
        assert!(ready.all_enterable() && !blocked.all_enterable());
        assert!(!keep.forms_due(), "then only every few minutes");
        server.abort();
    }

    #[test]
    fn only_listed_competitions_anyone_can_still_enter_in_time_are_open() {
        let now = OffsetDateTime::now_utc();
        let queue = |close: OffsetDateTime, extra: serde_json::Value| {
            let mut json = serde_json::json!({
                "kind": "queued",
                "event_submission": {
                    "start_observation_date": closing(close),
                    "total_allowed_entries": 25,
                    "unlisted": true,
                },
                "pool_rules": { "min_players": 2, "max_players": 25 },
                "entries": 3,
                "max_entries": 100,
                "total_entries": 3,
            });
            json.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            competition(json)
        };
        let single = |close: OffsetDateTime, unlisted: bool, entries: u64| {
            competition(serde_json::json!({
                "event_submission": {
                    "start_observation_date": closing(close),
                    "total_allowed_entries": 5,
                    "unlisted": unlisted,
                },
                "total_entries": entries,
            }))
        };
        let min_left = Duration::minutes(30);
        let enough = queue(now + Duration::hours(5), serde_json::json!({}));
        let little = queue(now + Duration::minutes(20), serde_json::json!({}));
        let full = queue(
            now + Duration::hours(6),
            serde_json::json!({ "entries": 100, "total_entries": 100 }),
        );
        let full_single = single(now + Duration::hours(6), false, 5);
        let live = queue(
            now - Duration::hours(1),
            serde_json::json!({ "pools_formed_at": closing(now - Duration::hours(1)) }),
        );
        let live_single = competition(serde_json::json!({
            "event_submission": {
                "start_observation_date": closing(now + Duration::hours(6)),
                "total_allowed_entries": 5,
            },
            "total_entries": 5,
            "escrow_funds_confirmed_at": closing(now),
        }));
        let unlisted = single(now + Duration::hours(8), true, 1);
        let listed = single(now + Duration::hours(2), false, 1);

        let open = |list: &[&CompetitionResponse]| {
            let list: Vec<CompetitionResponse> = list.iter().map(|c| (*c).clone()).collect();
            open_competitions(&list, now, min_left)
        };
        assert_eq!(
            open(&[
                &enough,
                &little,
                &full,
                &full_single,
                &live,
                &live_single,
                &unlisted
            ]),
            Open {
                competitions: 1,
                minutes_left: 300,
            },
            "a listed queue with room and five hours left; the rest are not open"
        );
        let both = open(&[&enough, &listed]);
        assert_eq!(both.competitions, 2);
        assert_eq!(both.minutes_left, 300, "the longest time left");
        assert_eq!(
            open(&[&little, &full, &live, &unlisted]),
            Open::default(),
            "none open: zero minutes"
        );
    }
}
