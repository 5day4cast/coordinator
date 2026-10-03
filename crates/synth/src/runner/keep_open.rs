//! Keep a competition open for visitors: one anyone can enter, with time left to do it.
//!
//! Every check lists the coordinator's competitions and counts those that are listed, still take
//! entries, have room, and close their entries at least `min_minutes_left` from now. When none
//! do, the keep-open lane's next run starts at once, unless that lane started a run in the last
//! `min_minutes_left`: its competition can take that long to show up. While the coordinator
//! would refuse tickets anyway, as it does while the Arkade network recovers, no run is started;
//! the next check tries again.

use std::sync::{Arc, Mutex};

use log::{info, warn};
use time::{Duration, OffsetDateTime};

use crate::client::competitions::CompetitionResponse;
use crate::client::CoordinatorClient;
use crate::config::KeepOpenConfig;

/// The competitions open for visitors at a check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Open {
    pub competitions: usize,
    /// The longest time left among them before entries close, in minutes; 0 when none.
    pub minutes_left: i64,
}

/// Count the competitions in `listed` that anyone can enter and that close their entries at
/// least `min_left` after `now`.
pub fn open_competitions(
    listed: &[CompetitionResponse],
    now: OffsetDateTime,
    min_left: Duration,
) -> Open {
    let left: Vec<Duration> = listed
        .iter()
        .filter(|competition| competition.listed() && competition.open_to_enter(now))
        .filter_map(|competition| competition.entries_close())
        .map(|close| close - now)
        .filter(|left| *left >= min_left)
        .collect();
    Open {
        competitions: left.len(),
        minutes_left: left.iter().max().map_or(0, |left| left.whole_minutes()),
    }
}

/// What the dashboard shows about the competitions open for visitors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct OpenStatus {
    pub open: Open,
    /// A run was started because none was open, and its competition is not open yet.
    pub starting: bool,
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
}

impl KeepOpen {
    pub fn new(config: KeepOpenConfig, entry_fee: u64, lane_start: Arc<LaneStart>) -> Self {
        Self {
            config,
            entry_fee,
            last_start: None,
            lane_start,
            paused: false,
        }
    }

    fn min_left(&self) -> Duration {
        Duration::minutes(self.config.min_minutes_left as i64)
    }

    /// Check once: what is open, and whether to start a run now.
    pub async fn check(
        &mut self,
        client: &CoordinatorClient,
    ) -> anyhow::Result<(OpenStatus, bool)> {
        let now = OffsetDateTime::now_utc();
        let open = open_competitions(&client.list_competitions().await?, now, self.min_left());
        crate::server::metrics::record_open(open.competitions, open.minutes_left);
        let grace = std::time::Duration::from_secs(self.config.min_minutes_left * 60);
        // A start that created no competition is not one to wait for.
        if self.lane_start.take_failed() {
            self.last_start = None;
        }
        let recently =
            self.last_start.is_some_and(|at| at.elapsed() < grace) || self.lane_start.recent(grace);
        if open.competitions > 0 || recently {
            return Ok((
                OpenStatus {
                    open,
                    starting: open.competitions == 0,
                },
                false,
            ));
        }
        if let Some(reason) = client.entries_paused(self.entry_fee).await? {
            if !self.paused {
                info!("No competition is open for visitors, but the coordinator refuses tickets now; trying again at the next check: {reason}");
            }
            self.paused = true;
            return Ok((
                OpenStatus {
                    open,
                    starting: false,
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
        info!(
            "No competition is open for visitors; lane {} starts its next run now",
            self.config.lane
        );
        Ok((
            OpenStatus {
                open,
                starting: true,
            },
            true,
        ))
    }

    /// Check every `check_interval_secs`, waking the lane through `lane_start` when a run is needed,
    /// and keeping `status` current for the dashboard.
    pub async fn run(mut self, client: CoordinatorClient, status: SharedOpenStatus) {
        let mut checks = tokio::time::interval(std::time::Duration::from_secs(
            self.config.check_interval_secs,
        ));
        checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            checks.tick().await;
            match self.check(&client).await {
                Ok((open, starting)) => {
                    *status
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(open);
                    if starting {
                        self.lane_start.notify.notify_one();
                    }
                }
                Err(e) => warn!("Cannot check for a competition open for visitors: {e:#}"),
            }
        }
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
