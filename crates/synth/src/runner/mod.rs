pub mod keep_open;
pub mod lanes;

use crate::client::CoordinatorClient;
use crate::config::{KeepOpenConfig, SchedulerConfig};
use crate::db::SynthDb;
use crate::events::{Event, Events};
use crate::scenarios::{
    self, ScenarioConfig, ScenarioResult, ScenarioStatus, StepResult, StepStatus,
};
use anyhow::Result;
use dashmap::DashMap;
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use uuid::Uuid;

/// The scenarios synth runs, by the names runs are started with.
pub const SCENARIOS: &[&str] = &[
    "full_lifecycle",
    "escrow_refund",
    "abandoned_unpaid",
    "paid_abandonment",
    "duplicate_submission",
    "late_submission",
    scenarios::queued::QUEUED_SPLIT,
    scenarios::queued::QUEUED_ONE_POOL,
    scenarios::queued::QUEUED_TOO_FEW,
    scenarios::queued::QUEUED_LEFTOVER_REFUND,
    scenarios::stress::STRESS_FULL_POOL,
];

/// Whether a run of `scenario` can be recorded: one of [`SCENARIOS`], or a competition an
/// operator asked for, which no schedule runs.
fn recordable(scenario: &str) -> bool {
    SCENARIOS.contains(&scenario) || scenario == scenarios::manual::MANUAL_COMPETITION
}

/// Cover every case/window pair instead of coupling two cycles of equal length.
fn scheduled_selection<'a>(scenarios: &[&'a str], windows: &[u64], cycle: usize) -> (&'a str, u64) {
    (
        scenarios[cycle % scenarios.len()],
        windows[(cycle / scenarios.len()) % windows.len()],
    )
}

/// A run in progress, and the step it is on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveRun {
    pub run_id: String,
    /// The competition the run plays, by the id synth chose for it when the run was planned.
    pub competition_id: Uuid,
    pub scenario: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    pub current_step: Option<String>,
}

/// Shared state for the runner
#[derive(Clone)]
pub struct Runner {
    client: CoordinatorClient,
    db: SynthDb,
    events: Events,
    last_result: Arc<Mutex<Option<ScenarioResult>>>,
    /// Every run in progress, by its competition: runs overlap, each moving its own competition
    /// through its states.
    live: Live,
    /// Chooses the stations of lanes that pick them for the weather.
    picker: Option<Arc<crate::picker::Picker>>,
    /// What the keep-open check last found, for the dashboard.
    open: keep_open::SharedOpenStatus,
}

/// Runs in progress, by competition id.
type Live = Arc<DashMap<Uuid, LiveRun>>;

tokio::task_local! {
    /// The run a scenario is recording into, for the steps it records.
    static RECORDER: Recorder;
}

/// Saves and announces a run's steps as the scenario records them.
#[derive(Clone)]
struct Recorder {
    run_id: String,
    competition_id: Uuid,
    events: Events,
    live: Live,
    steps: mpsc::UnboundedSender<Record>,
}

/// What a scenario records about a step, saved in the order recorded.
enum Record {
    /// What the step has done so far; `saved` is told once it is in the database.
    Progress {
        step: String,
        details: serde_json::Value,
        saved: oneshot::Sender<std::result::Result<(), String>>,
    },
    Finished(StepResult),
}

/// Announce that the running scenario has begun `step`. Does nothing outside a run.
pub(crate) fn step_started(step: &str) {
    let _ = RECORDER.try_with(|recorder| {
        if let Some(mut live) = recorder.live.get_mut(&recorder.competition_id) {
            live.current_step = Some(step.to_string());
        }
        recorder.events.send(Event::StepStarted {
            run_id: recorder.run_id.clone(),
            step: step.to_string(),
        });
    });
}

/// Save and announce a step the running scenario finished. Does nothing outside a run.
pub(crate) fn step_finished(step: &StepResult) {
    let _ = RECORDER.try_with(|recorder| {
        let _ = recorder.steps.send(Record::Finished(step.clone()));
    });
}

/// Save what the running scenario's `step` has done so far, and wait until it is saved: for a
/// step about to do something a restart must not lose track of, such as paying. The step shows
/// as running until it finishes. Does nothing outside a run.
pub(crate) async fn step_progress(step: &str, details: serde_json::Value) -> Result<()> {
    let Ok(saved) = RECORDER.try_with(|recorder| {
        let (saved, is_saved) = oneshot::channel();
        recorder
            .steps
            .send(Record::Progress {
                step: step.to_string(),
                details,
                saved,
            })
            .map_err(|_| anyhow::anyhow!("Run recorder stopped before progress could be saved"))?;
        Ok::<_, anyhow::Error>(is_saved)
    }) else {
        return Ok(());
    };
    saved?
        .await
        .map_err(|_| anyhow::anyhow!("Run recorder stopped before acknowledging progress"))?
        .map_err(anyhow::Error::msg)
}

/// Save each step in the order recorded, announcing each once it is saved.
async fn save_steps(
    db: SynthDb,
    events: Events,
    run_id: String,
    mut records: mpsc::UnboundedReceiver<Record>,
) {
    // Steps saved before they finished, by name, and their rows.
    let mut open: HashMap<String, String> = HashMap::new();
    while let Some(record) = records.recv().await {
        match record {
            Record::Progress {
                step,
                details,
                saved,
            } => {
                let row = match open.get(&step) {
                    Some(row) => Ok(row.clone()),
                    None => db.create_step(&run_id, &step).await,
                };
                let written = match row {
                    Ok(row) => {
                        let written = db.update_step_details(&row, &details.to_string()).await;
                        open.insert(step.clone(), row);
                        written
                    }
                    Err(e) => Err(e),
                };
                if let Err(e) = &written {
                    error!("Cannot save step {step} of run {run_id} so far: {e:#}");
                }
                let _ = saved.send(written.map_err(|error| error.to_string()));
                events.send(Event::StepStarted {
                    run_id: run_id.clone(),
                    step,
                });
            }
            Record::Finished(step) => {
                let details = step.details.as_ref().map(|d| d.to_string());
                let row = match open.remove(&step.name) {
                    Some(row) => Ok(row),
                    None => db.create_step(&run_id, &step.name).await,
                };
                let saved = match row {
                    Ok(row) => {
                        db.complete_step(
                            &row,
                            step.duration_ms,
                            step.error.as_deref(),
                            details.as_deref(),
                        )
                        .await
                    }
                    Err(e) => Err(e),
                };
                if let Err(e) = saved {
                    error!("Cannot save step {} of run {run_id}: {e:#}", step.name);
                }
                events.send(Event::StepFinished {
                    run_id: run_id.clone(),
                    step: step.name.clone(),
                    passed: step.status == StepStatus::Passed,
                });
            }
        }
    }
}

impl Runner {
    pub fn new(client: CoordinatorClient, db: SynthDb, events: Events) -> Self {
        Self {
            client,
            db,
            events,
            last_result: Arc::new(Mutex::new(None)),
            live: Arc::new(DashMap::new()),
            picker: None,
            open: Arc::default(),
        }
    }

    /// Let lanes with a `picker` choose their stations for the weather.
    pub fn with_picker(mut self, picker: crate::picker::Picker) -> Self {
        self.picker = Some(Arc::new(picker));
        self
    }

    /// What the keep-open check last found: None before its first check, or without one.
    pub fn open_status(&self) -> Option<keep_open::OpenStatus> {
        *self
            .open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run a scenario by name, saving each step as it finishes.
    pub async fn run_scenario(
        &self,
        scenario: &str,
        config: ScenarioConfig,
    ) -> Result<ScenarioResult> {
        let run_id = self.record_run(scenario, &config).await?;
        self.run_recorded(run_id, scenario).await
    }

    /// Check `scenario` is one synth runs, and record a run of it, returning the run's id. Run
    /// it with [`Runner::run_recorded`].
    pub async fn record_run(&self, scenario: &str, config: &ScenarioConfig) -> Result<String> {
        if !recordable(scenario) {
            error!("Unknown scenario: {}", scenario);
            return Err(anyhow::anyhow!(
                "Unknown scenario: {scenario}; expected one of {}",
                SCENARIOS.join(", ")
            ));
        }
        let mut config = config.clone();
        if let (Some(mix), None) = (&config.player_mix, config.min_players) {
            config.min_players = mix.floor_at(self.network_fee_rate().await);
        }
        let config_json = serde_json::to_string(&config.resolve_plan(scenario)?)?;
        self.db.create_run(scenario, Some(&config_json)).await
    }

    /// The coordinator's network fee rate, or None if it cannot be read, which is taken as high.
    async fn network_fee_rate(&self) -> Option<f64> {
        self.client
            .network_fee_rate()
            .await
            .inspect_err(|e| {
                warn!("Cannot read the network fee; drawing no small competitions: {e:#}")
            })
            .ok()
    }

    /// Run the scenario of a run [`Runner::record_run`] recorded.
    pub async fn run_recorded(&self, run_id: String, scenario: &str) -> Result<ScenarioResult> {
        // Execute precisely the plan already on disk, never a newly sampled caller config.
        let run = self
            .db
            .get_run(&run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Recorded run not found"))?;
        anyhow::ensure!(
            run.scenario == scenario,
            "Recorded scenario does not match requested execution"
        );
        let config: ScenarioConfig = serde_json::from_str(
            run.config_json
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Recorded run has no resolved plan"))?,
        )?;
        anyhow::ensure!(
            config.seed.is_some() && config.entry_plan.len() == config.users,
            "Recorded run has no resolved entry plan"
        );
        anyhow::ensure!(
            config.planned_scenario.as_deref() == Some(scenario),
            "Recorded plan belongs to another scenario"
        );
        let competition_id = config
            .competition_id
            .ok_or_else(|| anyhow::anyhow!("Recorded plan has no competition id"))?;
        info!("Starting scenario '{scenario}' (run: {run_id}, competition: {competition_id})");
        self.live.insert(
            competition_id,
            LiveRun {
                run_id: run_id.clone(),
                competition_id,
                scenario: scenario.to_string(),
                started_at: OffsetDateTime::now_utc(),
                current_step: None,
            },
        );
        self.events.send(Event::RunStarted {
            run_id: run_id.clone(),
            scenario: scenario.to_string(),
        });

        let (steps, to_save) = mpsc::unbounded_channel();
        let saving = tokio::spawn(save_steps(
            self.db.clone(),
            self.events.clone(),
            run_id.clone(),
            to_save,
        ));
        let recorder = Recorder {
            run_id: run_id.clone(),
            competition_id,
            events: self.events.clone(),
            live: self.live.clone(),
            steps,
        };
        let result = RECORDER
            .scope(recorder, async {
                match scenario {
                    "full_lifecycle" => {
                        scenarios::run_full_lifecycle(&self.client, &self.db, &config).await
                    }
                    "escrow_refund" => {
                        scenarios::run_escrow_refund(&self.client, &self.db, &config).await
                    }
                    "abandoned_unpaid" => {
                        scenarios::run_abandoned_unpaid(&self.client, &self.db, &config).await
                    }
                    "paid_abandonment" => {
                        scenarios::run_paid_abandonment(&self.client, &self.db, &config).await
                    }
                    "duplicate_submission" => {
                        scenarios::run_duplicate_submission(&self.client, &self.db, &config).await
                    }
                    "late_submission" => {
                        scenarios::run_late_submission(&self.client, &self.db, &config).await
                    }
                    scenarios::queued::QUEUED_SPLIT => {
                        scenarios::run_queued_split(&self.client, &self.db, &config).await
                    }
                    scenarios::queued::QUEUED_ONE_POOL => {
                        scenarios::run_queued_one_pool(&self.client, &self.db, &config).await
                    }
                    scenarios::queued::QUEUED_TOO_FEW => {
                        scenarios::run_queued_too_few(&self.client, &self.db, &config).await
                    }
                    scenarios::queued::QUEUED_LEFTOVER_REFUND => {
                        scenarios::run_queued_leftover_refund(&self.client, &self.db, &config).await
                    }
                    scenarios::stress::STRESS_FULL_POOL => {
                        scenarios::run_stress_full_pool(&self.client, &self.db, &config).await
                    }
                    scenarios::manual::MANUAL_COMPETITION => {
                        scenarios::run_manual_competition(&self.client, &self.db, &config).await
                    }
                    _ => unreachable!("record_run validates scenario names"),
                }
            })
            .await;
        // The recorder, and with it the last sender, is gone; wait for its steps to be saved.
        if let Err(e) = saving.await {
            error!("Saving run {run_id}'s steps stopped: {e}");
        }

        self.db
            .complete_run(&run_id, result.error.as_deref())
            .await?;
        crate::server::metrics::record_scenario(
            scenario,
            result.status == ScenarioStatus::Passed,
            result.total_duration_ms,
            &result
                .steps
                .iter()
                .map(|step| (step.name.clone(), step.duration_ms))
                .collect::<Vec<_>>(),
        );
        self.live.remove(&competition_id);
        *self.last_result.lock().await = Some(result.clone());
        self.events.send(Event::RunFinished {
            run_id: run_id.clone(),
            passed: result.status == ScenarioStatus::Passed,
        });

        if result.status == ScenarioStatus::Passed {
            info!(
                "Scenario '{}' passed in {}ms",
                scenario, result.total_duration_ms
            );
        } else {
            error!(
                "Scenario '{}' failed: {}",
                scenario,
                result.error.as_deref().unwrap_or("unknown")
            );
        }

        Ok(result)
    }

    /// Get the last scenario result
    pub async fn last_result(&self) -> Option<ScenarioResult> {
        self.last_result.lock().await.clone()
    }

    /// Every run in progress, oldest first.
    pub fn live_runs(&self) -> Vec<LiveRun> {
        let mut runs: Vec<LiveRun> = self.live.iter().map(|run| run.value().clone()).collect();
        runs.sort_by_key(|run| run.started_at);
        runs
    }

    /// The run `run_id`, if it is in progress.
    pub fn live_run(&self, run_id: &str) -> Option<LiveRun> {
        self.live
            .iter()
            .find(|run| run.run_id == run_id)
            .map(|run| run.value().clone())
    }

    pub fn events(&self) -> &Events {
        &self.events
    }

    pub fn client(&self) -> &CoordinatorClient {
        &self.client
    }

    /// Get database reference
    pub fn db(&self) -> &SynthDb {
        &self.db
    }

    /// Run `scenario`, logging a failure. If the coordinator refused its competition as too small
    /// for the fees, which rose since the count was drawn, draw again, large enough.
    async fn run_or_redraw(&self, scenario: &str, mut config: ScenarioConfig) {
        match self.run_scenario(scenario, config.clone()).await {
            Ok(result) if result.refused_as_small() => {
                config.min_players = config
                    .player_mix
                    .as_ref()
                    .map(|mix| mix.min_players_high_fees);
                config.seed = config.seed.map(|seed| seed.wrapping_add(0x5245_4452_4157));
                config.competition_id = None;
                if let Err(e) = self.run_scenario(scenario, config).await {
                    error!("Scheduled run failed: {:?}", e);
                }
            }
            Ok(_) => {}
            Err(e) => error!("Scheduled run failed: {:?}", e),
        }
    }

    /// Run every lane side by side, each starting its runs on its own cadence without waiting
    /// for its earlier ones, and with `keep_open`, its lane's next run early whenever no
    /// competition is open for visitors. Returns only if a lane is invalid.
    pub async fn run_lanes(
        &self,
        lanes: &[lanes::LaneConfig],
        base: ScenarioConfig,
        keep_open: Option<&KeepOpenConfig>,
    ) -> Result<()> {
        for lane in lanes {
            lane.validate(&base)?;
        }
        if let Some(keep_open) = keep_open {
            keep_open.validate(lanes)?;
        }
        let early = keep_open.map(|keep_open| {
            let start = Arc::new(Notify::new());
            tokio::spawn(
                keep_open::KeepOpen::new(keep_open.clone(), base.entry_fee as u64).run(
                    self.client.clone(),
                    start.clone(),
                    self.open.clone(),
                ),
            );
            (keep_open.lane.clone(), start)
        });
        info!(
            "Starting {} lanes: {}",
            lanes.len(),
            lanes
                .iter()
                .map(|lane| format!("{} ({:?})", lane.name, lane.scenarios))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let lanes: Vec<_> = lanes
            .iter()
            .cloned()
            .map(|lane| {
                let (runner, base) = (self.clone(), base.clone());
                let start = early
                    .as_ref()
                    .filter(|(name, _)| *name == lane.name)
                    .map(|(_, start)| start.clone());
                tokio::spawn(async move { runner.run_lane(lane, base, start).await })
            })
            .collect();
        futures::future::join_all(lanes).await;
        Ok(())
    }

    async fn run_lane(
        &self,
        lane: lanes::LaneConfig,
        base: ScenarioConfig,
        early: Option<Arc<Notify>>,
    ) {
        lane_loop(&lane, &base, early, |scenario, config| {
            let runner = self.clone();
            let (picking, defaults) = (lane.clone(), base.clone());
            tokio::spawn(async move {
                let mut config = config;
                if let Some(picker) = &runner.picker {
                    picker.choose(&picking, &defaults, &mut config).await;
                }
                runner.run_or_redraw(&scenario, config).await
            });
        })
        .await
    }

    /// Start the scheduled runner loop
    pub async fn run_scheduled(
        &self,
        scheduler: &SchedulerConfig,
        config: ScenarioConfig,
        windows: Vec<u64>,
    ) -> Result<()> {
        scheduler.validate(&windows)?;
        info!(
            "Starting scheduled runner: {:?} every {}s, observation windows {:?}s",
            scheduler.scenario_names(),
            scheduler.interval_secs,
            windows
        );
        let scenarios = scheduler.scenario_names();
        let mut cycle = 0usize;

        loop {
            // Runs start every interval, however long each one's entry window keeps it going.
            let started = tokio::time::Instant::now();
            let mut next = config.clone();
            next.observation_window_choices.clear();
            let (scenario, window) = scheduled_selection(&scenarios, &windows, cycle);
            next.observation_window_secs = window;
            next.seed = config.seed.map(|seed| seed.wrapping_add(cycle as u64));
            self.run_or_redraw(scenario, next).await;
            cycle = cycle.wrapping_add(1);
            tokio::time::sleep_until(
                started + std::time::Duration::from_secs(scheduler.interval_secs),
            )
            .await;
        }
    }
}

/// Start `lane`'s runs through `start` on its cadence, for ever. When `early` is notified, the
/// lane's next run starts at once instead of when it was due: its scenario and window rotation
/// carry on from it, and its next run follows a whole cadence after it.
async fn lane_loop(
    lane: &lanes::LaneConfig,
    base: &ScenarioConfig,
    early: Option<Arc<Notify>>,
    mut start: impl FnMut(String, ScenarioConfig),
) {
    let mut after = OffsetDateTime::now_utc();
    let mut cycle = 0usize;
    loop {
        let (mut at, mut close) = lane.next_start(base, after);
        match &early {
            Some(early) => tokio::select! {
                _ = sleep_until(at) => {}
                _ = early.notified() => {
                    (at, close) = lane.next_start(base, OffsetDateTime::now_utc());
                    info!("Lane {} starts its next run early, to keep a competition open", lane.name);
                }
            },
            None => sleep_until(at).await,
        }
        let (scenario, config) = lane.run_config(base, cycle, close);
        info!("Lane {} starts {scenario} (cycle {cycle})", lane.name);
        start(scenario, config);
        after = match close {
            // The next half after this one.
            Some(close) => close + time::Duration::seconds(1) - (close - at),
            None => at + time::Duration::seconds(lane.interval_secs as i64),
        };
        cycle = cycle.wrapping_add(1);
    }
}

/// Sleep until `at`, at once if it has passed.
async fn sleep_until(at: OffsetDateTime) {
    let wait = (at - OffsetDateTime::now_utc()).max(time::Duration::ZERO);
    tokio::time::sleep(wait.unsigned_abs()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_progress_write_stops_work_before_payment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.db");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        let run_id = db.create_run("full_lifecycle", None).await.unwrap();
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sqlx::query("DROP TABLE test_steps")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let events = Events::new();
        let (steps, records) = mpsc::unbounded_channel();
        let saving = tokio::spawn(save_steps(db, events.clone(), run_id.clone(), records));
        let recorder = Recorder {
            run_id,
            competition_id: Uuid::now_v7(),
            events,
            live: Arc::new(DashMap::new()),
            steps,
        };
        let mut paid = false;
        let result = RECORDER
            .scope(recorder, async {
                step_progress(
                    "entry_before_payment",
                    serde_json::json!({"ticket_id":"fixture"}),
                )
                .await?;
                paid = true;
                Ok::<_, anyhow::Error>(())
            })
            .await;
        assert!(result.is_err());
        assert!(!paid, "a missing durable trace must not allow payment");
        saving.await.unwrap();
    }

    #[test]
    fn each_scheduled_case_receives_every_window_and_legacy_keeps_rotation() {
        let cases = ["full_lifecycle", "late_submission"];
        let windows = [7200, 10800, 14400, 600];
        let selected: Vec<_> = (0..8)
            .map(|index| scheduled_selection(&cases, &windows, index))
            .collect();
        for case in cases {
            assert_eq!(
                selected
                    .iter()
                    .filter_map(|(name, window)| (*name == case).then_some(*window))
                    .collect::<Vec<_>>(),
                windows
            );
        }
        assert_eq!(
            scheduled_selection(&cases, &windows, 8),
            ("full_lifecycle", 7200)
        );
        assert_eq!(
            (0..5)
                .map(|index| scheduled_selection(&["full_lifecycle"], &windows, index).1)
                .collect::<Vec<_>>(),
            vec![7200, 10800, 14400, 600, 7200]
        );
    }

    #[tokio::test]
    async fn resolved_seed_and_entry_plan_are_persisted_before_execution() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let runner = Runner::new(
            CoordinatorClient::new("http://127.0.0.1:1", None),
            db.clone(),
            Events::new(),
        );
        let config = ScenarioConfig::default();
        let id = runner.record_run("late_submission", &config).await.unwrap();
        let saved = db.get_run(&id).await.unwrap().unwrap();
        let plan: ScenarioConfig =
            serde_json::from_str(saved.config_json.as_deref().unwrap()).unwrap();
        assert!(plan.seed.is_some());
        assert_eq!(plan.entry_plan.len(), config.users);
        assert_eq!(plan.planned_scenario.as_deref(), Some("late_submission"));
        assert!(saved.competition_id.is_none());
        let mut invalid = config;
        invalid.entry_timing.arrival.max_secs = u64::MAX;
        assert!(runner.record_run("full_lifecycle", &invalid).await.is_err());
        assert_eq!(db.list_runs(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scheduled_competitions_rotate_windows_and_reject_invalid_cycles_before_recording() {
        use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
        use time::format_description::well_known::Rfc3339;

        // Reject creation at a local fake coordinator; no users or payments are needed.
        async fn create(
            State(posted): State<mpsc::UnboundedSender<serde_json::Value>>,
            Json(body): Json<serde_json::Value>,
        ) -> StatusCode {
            posted.send(body).unwrap();
            StatusCode::INTERNAL_SERVER_ERROR
        }
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let (posted, mut requests) = mpsc::unbounded_channel();
        let app = Router::new()
            .route("/api/v1/competitions", post(create))
            .with_state(posted);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runner = Runner::new(
            CoordinatorClient::new(&url, None),
            db.clone(),
            Events::new(),
        );
        let scheduler = SchedulerConfig {
            enabled: true,
            interval_secs: 1,
            scenario: "full_lifecycle".into(),
            scenarios: None,
            lanes: Vec::new(),
            keep_open: None,
        };
        assert!(runner
            .run_scheduled(&scheduler, ScenarioConfig::default(), vec![7200, 0])
            .await
            .is_err());
        assert!(db.list_runs(10).await.unwrap().is_empty());
        assert!(requests.try_recv().is_err());

        let running = tokio::spawn(async move {
            runner
                .run_scheduled(
                    &scheduler,
                    ScenarioConfig::default(),
                    vec![7200, 10800, 14400, 600],
                )
                .await
        });
        let observed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut windows = Vec::new();
            for _ in 0..5 {
                let body = requests.recv().await.unwrap();
                let start = OffsetDateTime::parse(
                    body["start_observation_date"].as_str().unwrap(),
                    &Rfc3339,
                )
                .unwrap();
                let end =
                    OffsetDateTime::parse(body["end_observation_date"].as_str().unwrap(), &Rfc3339)
                        .unwrap();
                windows.push((end - start).whole_seconds());
            }
            windows
        })
        .await;
        running.abort();
        server.abort();
        assert_eq!(observed.unwrap(), vec![7200, 10800, 14400, 600, 7200]);
    }

    /// A keep-open start runs the lane's next run at once, and the lane's next scheduled run
    /// follows a whole interval after it, not when it was first due.
    #[tokio::test(start_paused = true)]
    async fn a_lane_started_early_restarts_its_timer_from_that_run() {
        let lane = lanes::LaneConfig {
            name: "open".into(),
            interval_secs: 3600,
            align: lanes::Align::Interval,
            scenarios: vec![scenarios::queued::QUEUED_ONE_POOL.into()],
            observation_windows_secs: Some(vec![86_400, 172_800]),
            entry_window_secs: Some(7200),
            stations: None,
            stations_per_run: None,
            fill: scenarios::Fill::Backfill,
            early_players: 1,
            backfill_before_close_secs: 1800,
            backfill_margin: 1,
        };
        let base = ScenarioConfig::default();
        lane.validate(&base).unwrap();
        let early = Arc::new(Notify::new());
        let (started, mut starts) = mpsc::unbounded_channel();
        let begun = tokio::time::Instant::now();
        let running = tokio::spawn({
            let early = early.clone();
            async move {
                lane_loop(&lane, &base, Some(early), |scenario, config| {
                    started
                        .send((
                            begun.elapsed().as_secs(),
                            scenario,
                            config.observation_window_secs,
                            config.backfill,
                        ))
                        .unwrap();
                })
                .await
            }
        });
        let (at, scenario, window, backfill) = starts.recv().await.unwrap();
        assert_eq!(
            (at, scenario.as_str(), window),
            (0, "queued_one_pool", 86_400)
        );
        assert_eq!(backfill.map(|backfill| backfill.margin), Some(1));

        tokio::time::sleep(std::time::Duration::from_secs(1000)).await;
        early.notify_one();
        let (at, _, window, _) = starts.recv().await.unwrap();
        assert_eq!(
            (at, window),
            (1000, 172_800),
            "at once, and the rotation goes on"
        );
        // Not at 3600, when it was first due: a whole interval after the early run.
        let (at, _, window, _) = starts.recv().await.unwrap();
        assert!((4595..=4600).contains(&at), "next run at {at} s");
        assert_eq!(window, 86_400);
        running.abort();
    }

    /// A page watching a run sees each step as soon as it is recorded, not when the run ends.
    #[tokio::test]
    async fn a_step_is_saved_and_announced_while_its_run_goes_on() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let events = Events::new();
        let mut watching = events.subscribe();
        let run_id = db.create_run("full_lifecycle", None).await.unwrap();
        let competition_id = Uuid::now_v7();
        let live: Live = Arc::new(DashMap::new());
        live.insert(
            competition_id,
            LiveRun {
                run_id: run_id.clone(),
                competition_id,
                scenario: "full_lifecycle".into(),
                started_at: OffsetDateTime::now_utc(),
                current_step: None,
            },
        );
        let (steps, to_save) = mpsc::unbounded_channel();
        let saving = tokio::spawn(save_steps(
            db.clone(),
            events.clone(),
            run_id.clone(),
            to_save,
        ));
        let recorder = Recorder {
            run_id: run_id.clone(),
            competition_id,
            events: events.clone(),
            live: live.clone(),
            steps,
        };

        RECORDER
            .scope(recorder, async {
                step_started("create_competition");
                assert_eq!(
                    live.get(&competition_id).unwrap().current_step.as_deref(),
                    Some("create_competition")
                );
                step_finished(&StepResult {
                    name: "create_competition".into(),
                    status: StepStatus::Passed,
                    duration_ms: 5,
                    details: Some(serde_json::json!({ "competition_id": "c" })),
                    error: None,
                });
                loop {
                    if let Event::StepFinished { step, passed, .. } = watching.recv().await.unwrap()
                    {
                        assert_eq!((step.as_str(), passed), ("create_competition", true));
                        break;
                    }
                }
                // The run is still going, and the step is already there to show.
                let saved = db.get_steps(&run_id).await.unwrap();
                assert_eq!(saved.len(), 1);
                assert!(saved[0]
                    .details_json
                    .as_deref()
                    .unwrap()
                    .contains("competition_id"));

                // A step saves what it has done before paying, and finishes in the same row.
                step_progress("user_alice_enter", serde_json::json!({ "ticket_id": "t" }))
                    .await
                    .unwrap();
                let saved = db.get_steps(&run_id).await.unwrap();
                assert_eq!(saved.len(), 2, "saved before the step finishes");
                assert_eq!(saved[1].status, "running");
                assert!(saved[1].details_json.as_deref().unwrap().contains("\"t\""));
                step_finished(&StepResult {
                    name: "user_alice_enter".into(),
                    status: StepStatus::Passed,
                    duration_ms: 5,
                    details: Some(serde_json::json!({ "ticket_id": "t", "paid": true })),
                    error: None,
                });
                loop {
                    if let Event::StepFinished { step, .. } = watching.recv().await.unwrap() {
                        if step == "user_alice_enter" {
                            break;
                        }
                    }
                }
                let saved = db.get_steps(&run_id).await.unwrap();
                assert_eq!(saved.len(), 2, "finished in the row it was saved in");
                assert_eq!(saved[1].status, "passed");
                assert!(saved[1].details_json.as_deref().unwrap().contains("paid"));
            })
            .await;
        saving.await.unwrap();
    }
}
