pub mod keep_open;
pub mod lanes;
pub mod resume;

use crate::client::CoordinatorClient;
use crate::config::{KeepOpenConfig, SchedulerConfig};
use crate::db::{SynthDb, TestRun};
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
use tokio::sync::{mpsc, oneshot, Mutex};
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

/// Tries at reaching a resumed run's competition, and the wait between them: five minutes for
/// a coordinator restarting with synth.
const RESUME_TRIES: u32 = 30;
const RESUME_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

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
    schedules: Arc<HashMap<String, Vec<String>>>,
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
    /// What the run did before a restart, if it is a resumed run.
    prior: Option<Arc<resume::Prior>>,
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

/// The latest row of step `name` saved before a restart, when the running scenario is a resumed
/// run. None outside a run, for a new run, or for a step the run had not reached.
pub(crate) fn prior_step(name: &str) -> Option<crate::db::TestStep> {
    RECORDER
        .try_with(|recorder| {
            recorder
                .prior
                .as_ref()
                .and_then(|prior| prior.step(name).cloned())
        })
        .ok()
        .flatten()
}

/// The competition a resumed run made before the restart, which it carries on with instead of
/// making another. None outside a run, or for a new run.
pub(crate) fn resumed_competition() -> Option<Uuid> {
    RECORDER
        .try_with(|recorder| recorder.prior.as_ref().map(|_| recorder.competition_id))
        .ok()
        .flatten()
}

/// When a resumed run first started, as an instant now, so its players keep the arrival times
/// they were planned at. None outside a run, or for a new run.
pub(crate) fn resumed_start() -> Option<std::time::Instant> {
    RECORDER
        .try_with(|recorder| {
            recorder.prior.as_ref().map(|prior| {
                let since = (OffsetDateTime::now_utc() - prior.started_at)
                    .max(time::Duration::ZERO)
                    .unsigned_abs();
                let now = std::time::Instant::now();
                now.checked_sub(since).unwrap_or(now)
            })
        })
        .ok()
        .flatten()
}

/// Save each step in the order recorded, announcing each once it is saved. A resumed run's
/// steps still running at the restart finish in their own rows, and steps it had finished are
/// not saved again when they end the same way.
async fn save_steps(
    db: SynthDb,
    events: Events,
    run_id: String,
    prior: Option<Arc<resume::Prior>>,
    mut records: mpsc::UnboundedReceiver<Record>,
) {
    // Steps saved before they finished, by name, and their rows.
    let mut open: HashMap<String, String> = prior
        .as_ref()
        .map(|prior| prior.open_rows())
        .unwrap_or_default();
    let mut replay = prior
        .as_ref()
        .map(|prior| prior.replay())
        .unwrap_or_default();
    while let Some(record) = records.recv().await {
        match record {
            Record::Progress { step, saved, .. } if replay.finished(&step) => {
                // It finished before the restart, and what it saved then stands.
                let _ = saved.send(Ok(()));
            }
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
            Record::Finished(step) if replay.repeats(&step.name, step.error.is_some()) => {
                events.send(Event::StepFinished {
                    run_id: run_id.clone(),
                    step: step.name.clone(),
                    passed: step.status == StepStatus::Passed,
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
            schedules: Arc::default(),
        }
    }

    pub fn with_schedule(mut self, scheduler: &SchedulerConfig) -> Self {
        let mut schedules: HashMap<String, Vec<String>> = HashMap::new();
        if scheduler.enabled {
            if scheduler.lanes.is_empty() {
                for scenario in scheduler.scenario_names() {
                    schedules
                        .entry(scenario.into())
                        .or_default()
                        .push("Scheduled".into());
                }
            } else {
                for lane in &scheduler.lanes {
                    for scenario in &lane.scenarios {
                        schedules
                            .entry(scenario.clone())
                            .or_default()
                            .push(lane.name.clone());
                    }
                }
            }
        }
        self.schedules = Arc::new(schedules);
        self
    }

    pub fn schedule_for(&self, scenario: &str) -> String {
        self.schedules
            .get(scenario)
            .map(|lanes| lanes.join(", "))
            .unwrap_or_else(|| "Manual only".into())
    }

    #[cfg(test)]
    pub(crate) fn for_tests(client: CoordinatorClient, db: SynthDb, events: Events) -> Self {
        let picker = crate::picker::fixtures::picker(db.clone());
        Self::new(client, db, events).with_picker(picker)
    }

    /// Use this Oracle to require eligible stations for every new run.
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
        anyhow::ensure!(
            self.db.scenario_enabled(scenario).await?,
            "Scenario {scenario} is paused; no new run was started"
        );
        let picker = self.picker.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Oracle eligibility is not configured; no competition was created")
        })?;
        let mut config = config.clone();
        if let (Some(mix), None) = (&config.player_mix, config.min_players) {
            config.min_players = mix.floor_at(self.network_fee_rate().await);
        }
        let mut config = config.resolve_plan(scenario)?;
        let selected = match config.competition_id {
            Some(id) => crate::picker::store::pick_for(&self.db, id).await.is_some(),
            None => false,
        };
        if scenario != scenarios::manual::MANUAL_COMPETITION && !selected {
            picker.choose_default(&mut config).await?;
        }
        picker.validate_stations(&config).await?;
        let config_json = serde_json::to_string(&config)?;
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
        let (config, competition_id) = resume::recorded_plan(&run)?;
        self.execute(run_id, scenario, config, competition_id, None)
            .await
    }

    /// Carry on every run the last shutdown left running from its last finished step, and mark
    /// those that cannot carry on as interrupted, saying why. Call once, before any new run
    /// starts. Returns how many runs were taken up again; each goes on in its own task.
    pub async fn resume_unfinished(&self) -> Result<usize> {
        let mut resumed = 0;
        for run in self.db.unfinished_runs().await? {
            let steps = self.db.get_steps(&run.id).await?;
            match resume::resume_point(&run, &steps) {
                Ok(point) => {
                    let runner = self.clone();
                    tokio::spawn(async move { runner.resume(run, point, steps).await });
                    resumed += 1;
                }
                Err(why) => {
                    warn!("Run {} ({}) cannot be resumed: {why}", run.id, run.scenario);
                    self.db.interrupt_run(&run.id, &why).await?;
                }
            }
        }
        Ok(resumed)
    }

    /// Carry on `run` from `point`, once the coordinator answers for its competition.
    async fn resume(
        &self,
        run: TestRun,
        point: resume::ResumePoint,
        steps: Vec<crate::db::TestStep>,
    ) {
        if let Err(error) = self.reach_competition(&point.competition_id).await {
            let why = if point.created {
                format!("the coordinator did not answer for its competition: {error:#}")
            } else {
                format!("it restarted before its competition was created: {error:#}")
            };
            warn!("Run {} ({}) cannot be resumed: {why}", run.id, run.scenario);
            if let Err(error) = self.db.interrupt_run(&run.id, &why).await {
                error!("Cannot mark run {} interrupted: {error:#}", run.id);
            }
            return;
        }
        let started_at = OffsetDateTime::parse(
            &run.started_at,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap_or_else(|_| OffsetDateTime::now_utc());
        info!(
            "Resuming run {} ({}) from its last finished step",
            run.id, run.scenario
        );
        crate::server::metrics::record_resumed_run();
        let prior = Arc::new(resume::Prior::new(started_at, steps));
        if let Err(error) = self
            .execute(
                run.id.clone(),
                &run.scenario,
                point.config,
                point.competition_id,
                Some(prior),
            )
            .await
        {
            error!("Resumed run {} stopped: {error:#}", run.id);
        }
    }

    /// Ask for a resumed run's competition until the coordinator answers, as it may be
    /// restarting with synth. Fails at once if the coordinator has no such competition, and
    /// after [`RESUME_TRIES`] tries without an answer.
    async fn reach_competition(&self, competition_id: &Uuid) -> Result<()> {
        let mut tries = 1;
        loop {
            match self.client.get_competition(competition_id).await {
                Ok(_) => return Ok(()),
                Err(error) if tries >= RESUME_TRIES || format!("{error:#}").contains("(404") => {
                    return Err(error)
                }
                Err(_) => {
                    tries += 1;
                    tokio::time::sleep(RESUME_RETRY).await;
                }
            }
        }
    }

    /// Run a recorded plan, or carry on one a restart cut short from what it did before.
    async fn execute(
        &self,
        run_id: String,
        scenario: &str,
        config: ScenarioConfig,
        competition_id: Uuid,
        prior: Option<Arc<resume::Prior>>,
    ) -> Result<ScenarioResult> {
        info!("Starting scenario '{scenario}' (run: {run_id}, competition: {competition_id})");
        self.live.insert(
            competition_id,
            LiveRun {
                run_id: run_id.clone(),
                competition_id,
                scenario: scenario.to_string(),
                started_at: prior
                    .as_ref()
                    .map_or_else(OffsetDateTime::now_utc, |prior| prior.started_at),
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
            prior.clone(),
            to_save,
        ));
        let recorder = Recorder {
            run_id: run_id.clone(),
            competition_id,
            events: self.events.clone(),
            live: self.live.clone(),
            steps,
            prior,
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
    /// for the fees, which rose since the count was drawn, draw again, large enough. Returns why
    /// if the run made no competition, which a lane tries again later.
    async fn run_or_redraw(&self, scenario: &str, mut config: ScenarioConfig) -> Option<String> {
        match self.db.scenario_enabled(scenario).await {
            Ok(false) => {
                info!("Skipping paused scenario {scenario}");
                return None;
            }
            Err(error) => {
                error!("Cannot read scenario controls; no run started: {error:#}");
                return None;
            }
            Ok(true) => {}
        }
        match self.attempt(scenario, config.clone()).await {
            Ok(result) if result.refused_as_small() => {
                config.min_players = config
                    .player_mix
                    .as_ref()
                    .map(|mix| mix.min_players_high_fees);
                config.seed = config.seed.map(|seed| seed.wrapping_add(0x5245_4452_4157));
                config.competition_id = None;
                self.attempt(scenario, config).await.err()
            }
            Ok(_) => None,
            Err(why) => Some(why),
        }
    }

    /// Record and run one run of `scenario`. Err, saying why, if it made no competition. A run
    /// that could not even be recorded, as when the oracle could not confirm its stations, is
    /// recorded here as a failed run, so history and metrics show it.
    async fn attempt(
        &self,
        scenario: &str,
        config: ScenarioConfig,
    ) -> std::result::Result<ScenarioResult, String> {
        let run_id = match self.record_run(scenario, &config).await {
            Ok(run_id) => run_id,
            Err(error) => {
                let why = format!("{error:#}");
                error!("Scheduled run of {scenario} failed before it started: {why}");
                if !why.contains("is paused") {
                    self.record_failed_start(scenario, &config, "plan_run", &why)
                        .await;
                }
                return Err(why);
            }
        };
        match self.run_recorded(run_id, scenario).await {
            Ok(result) => match result.creation_error() {
                Some(why) => Err(why),
                None => Ok(result),
            },
            Err(error) => {
                error!("Scheduled run failed: {error:#}");
                Err(format!("{error:#}"))
            }
        }
    }

    /// Record a run of `scenario` that failed at `step` before it was under way, in the history
    /// and the metrics.
    async fn record_failed_start(
        &self,
        scenario: &str,
        config: &ScenarioConfig,
        step: &str,
        why: &str,
    ) {
        crate::server::metrics::record_scenario(scenario, false, 0, &[(step.to_string(), 0)]);
        let config = serde_json::to_string(config).ok();
        if let Err(error) = self
            .db
            .record_failed_start(scenario, config.as_deref(), step, why)
            .await
        {
            error!("Cannot record the run of {scenario} that failed to start: {error:#}");
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
            let start = Arc::new(keep_open::LaneStart::default());
            tokio::spawn(
                keep_open::KeepOpen::new(keep_open.clone(), base.entry_fee as u64, start.clone())
                    .run(self.client.clone(), self.db.clone(), self.open.clone()),
            );
            (
                keep_open.lane.clone(),
                start,
                std::time::Duration::from_secs(keep_open.min_minutes_left * 60),
            )
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
            .enumerate()
            .map(|(index, lane)| {
                let (runner, base) = (self.clone(), base.clone());
                let start = early
                    .as_ref()
                    .filter(|(name, _, _)| *name == lane.name)
                    .map(|(_, start, grace)| (start.clone(), *grace));
                // Lanes started together asked the oracle for more than it takes at once.
                let offset = lanes::start_offset(index);
                tokio::spawn(async move { runner.run_lane(lane, base, start, offset).await })
            })
            .collect();
        futures::future::join_all(lanes).await;
        Ok(())
    }

    async fn run_lane(
        &self,
        lane: lanes::LaneConfig,
        base: ScenarioConfig,
        early: Option<(Arc<keep_open::LaneStart>, std::time::Duration)>,
        offset: std::time::Duration,
    ) {
        let start = early.as_ref().map(|(start, _)| start.clone());
        lane_loop(&lane, &base, early, offset, |scenario, config| {
            let runner = self.clone();
            let (picking, defaults, start) = (lane.clone(), base.clone(), start.clone());
            tokio::spawn(async move {
                runner
                    .start_with_retries(&picking, &defaults, &scenario, config, start)
                    .await
            });
        })
        .await
    }

    /// Start a lane's run. If it made no competition, it is in the history as failed; try again
    /// a few times, minutes apart, unless the lane started another run meanwhile. A keep-open
    /// lane's next check also starts one, if nothing else is open.
    async fn start_with_retries(
        &self,
        lane: &lanes::LaneConfig,
        base: &ScenarioConfig,
        scenario: &str,
        config: ScenarioConfig,
        start: Option<Arc<keep_open::LaneStart>>,
    ) {
        let mut failed = 0;
        loop {
            let Some(why) = self
                .start_lane_run(lane, base, scenario, config.clone())
                .await
            else {
                return;
            };
            failed += 1;
            let failed_at = tokio::time::Instant::now();
            if let Some(start) = &start {
                start.failed();
            }
            let Some(at) = lanes::retry_start(
                failed,
                OffsetDateTime::now_utc(),
                config.observation_start,
                config.entry_window_secs,
            ) else {
                warn!(
                    "Lane {}: {scenario} made no competition in {failed} tries; it waits for its \
                     next run: {why}",
                    lane.name
                );
                return;
            };
            warn!(
                "Lane {}: {scenario} made no competition; trying again at {at}: {why}",
                lane.name
            );
            sleep_until(at).await;
            if start
                .as_ref()
                .is_some_and(|start| start.started_since(failed_at))
            {
                info!(
                    "Lane {} started another run meanwhile; {scenario} is not tried again",
                    lane.name
                );
                return;
            }
            crate::server::metrics::record_lane_retry(&lane.name);
        }
    }

    /// Pick a lane's stations for its run and run it. Returns why if it made no competition;
    /// None if it did, or its scenario is paused.
    async fn start_lane_run(
        &self,
        lane: &lanes::LaneConfig,
        base: &ScenarioConfig,
        scenario: &str,
        mut config: ScenarioConfig,
    ) -> Option<String> {
        match self.db.scenario_enabled(scenario).await {
            Ok(true) => {}
            Ok(false) => {
                info!("Skipping paused scenario {scenario}");
                return None;
            }
            Err(error) => {
                error!("Cannot read scenario controls; no run started: {error:#}");
                return None;
            }
        }
        if let Some(picker) = &self.picker {
            if let Err(error) = picker.choose(lane, base, &mut config).await {
                let why = format!("{error:#}");
                warn!("Lane {} could not pick its stations: {why}", lane.name);
                self.record_failed_start(scenario, &config, "pick_stations", &why)
                    .await;
                return Some(why);
            }
        }
        self.run_or_redraw(scenario, config).await
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
            let _ = self.run_or_redraw(scenario, next).await;
            cycle = cycle.wrapping_add(1);
            tokio::time::sleep_until(
                started + std::time::Duration::from_secs(scheduler.interval_secs),
            )
            .await;
        }
    }
}

/// Start `lane`'s runs through `start` on its cadence, for ever, none sooner than `offset` from
/// now. When `early` is notified, the lane's next run starts at once instead of when it was due:
/// its scenario and window rotation carry on from it, and its next run follows a whole cadence
/// after it.
async fn lane_loop(
    lane: &lanes::LaneConfig,
    base: &ScenarioConfig,
    early: Option<(Arc<keep_open::LaneStart>, std::time::Duration)>,
    offset: std::time::Duration,
    mut start: impl FnMut(String, ScenarioConfig),
) {
    let mut after = OffsetDateTime::now_utc();
    let not_before = after + offset;
    let mut cycle = 0usize;
    loop {
        let (mut at, mut close) = lane.next_start(base, after);
        // An aligned lane keeps its close; only its start waits.
        at = at.max(not_before);
        match &early {
            Some((early, grace)) => tokio::select! {
                _ = sleep_until(at) => {}
                _ = early.notify.notified() => {
                    // A check may have requested this before the scheduled run started.
                    if early.recent(*grace) {
                        continue;
                    }
                    sleep_until(not_before).await;
                    (at, close) = lane.next_start(base, OffsetDateTime::now_utc());
                    info!("Lane {} starts its next run early, to keep a competition open", lane.name);
                }
            },
            None => sleep_until(at).await,
        }
        if let Some((early, _)) = &early {
            early.record();
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
    async fn paused_runs_and_missing_oracle_stop_before_execution() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let runner = Runner::new(
            CoordinatorClient::new("http://127.0.0.1:1", None),
            db.clone(),
            Events::new(),
        );
        db.set_scenario_enabled("full_lifecycle", false)
            .await
            .unwrap();
        let config = ScenarioConfig::default();
        assert!(runner
            .record_run("full_lifecycle", &config)
            .await
            .unwrap_err()
            .to_string()
            .contains("paused"));
        db.set_scenario_enabled("full_lifecycle", true)
            .await
            .unwrap();
        assert!(runner
            .record_run("full_lifecycle", &config)
            .await
            .unwrap_err()
            .to_string()
            .contains("eligibility is not configured"));
        assert!(db.list_runs(10).await.unwrap().is_empty());
    }

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
        let saving = tokio::spawn(save_steps(
            db,
            events.clone(),
            run_id.clone(),
            None,
            records,
        ));
        let recorder = Recorder {
            run_id,
            competition_id: Uuid::now_v7(),
            events,
            live: Arc::new(DashMap::new()),
            steps,
            prior: None,
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
        let runner = Runner::for_tests(
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
        let runner = Runner::for_tests(
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
            unlisted: None,
            stress: None,
            picker: None,
        };
        let base = ScenarioConfig::default();
        lane.validate(&base).unwrap();
        let early = Arc::new(keep_open::LaneStart::default());
        let (started, mut starts) = mpsc::unbounded_channel();
        let begun = tokio::time::Instant::now();
        early.notify.notify_one();
        let running = tokio::spawn({
            let early = early.clone();
            async move {
                lane_loop(
                    &lane,
                    &base,
                    Some((early, std::time::Duration::from_secs(300))),
                    std::time::Duration::ZERO,
                    |scenario, config| {
                        started
                            .send((
                                begun.elapsed().as_secs(),
                                scenario,
                                config.observation_window_secs,
                                config.backfill,
                            ))
                            .unwrap();
                    },
                )
                .await
            }
        });
        let (at, scenario, window, backfill) = starts.recv().await.unwrap();
        assert_eq!(
            (at, scenario.as_str(), window),
            (0, "queued_one_pool", 86_400)
        );
        assert_eq!(backfill.map(|backfill| backfill.margin), Some(1));
        early.notify.notify_one();
        tokio::task::yield_now().await;
        assert!(
            starts.try_recv().is_err(),
            "the startup check must not duplicate the run"
        );

        tokio::time::sleep(std::time::Duration::from_secs(1000)).await;
        early.notify.notify_one();
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

    /// A run that cannot start is in the history and the metrics as a failed run, saying why,
    /// and a lane hears why, to try again later.
    #[tokio::test]
    async fn a_run_that_cannot_start_is_recorded_as_failed() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        // Without an oracle to confirm its stations, no run can be planned.
        let runner = Runner::new(
            CoordinatorClient::new("http://127.0.0.1:1", None),
            db.clone(),
            Events::new(),
        );
        let failed_before = crate::server::metrics::SCENARIO_RUNS
            .with_label_values(&["late_submission", "failed"])
            .get();
        let why = runner
            .run_or_redraw("late_submission", ScenarioConfig::default())
            .await
            .expect("no competition was made");
        assert!(why.contains("eligibility is not configured"), "{why}");
        let runs = db.list_runs(10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error_message.as_deref(), Some(why.as_str()));
        let steps = db.get_steps(&runs[0].id).await.unwrap();
        assert_eq!(steps[0].step_name, "plan_run");
        assert!(
            crate::server::metrics::SCENARIO_RUNS
                .with_label_values(&["late_submission", "failed"])
                .get()
                >= failed_before + 1.0
        );
        // A paused scenario tries nothing, so there is nothing to record or try again.
        db.set_scenario_enabled("late_submission", false)
            .await
            .unwrap();
        assert!(runner
            .run_or_redraw("late_submission", ScenarioConfig::default())
            .await
            .is_none());
        assert_eq!(db.list_runs(10).await.unwrap().len(), 1);
    }

    /// Runs the last shutdown left running that cannot carry on are marked interrupted, saying
    /// why: a stress run, and one whose plan was never saved.
    #[tokio::test]
    async fn runs_that_cannot_resume_say_why() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let runner = Runner::new(
            CoordinatorClient::new("http://127.0.0.1:1", None),
            db.clone(),
            Events::new(),
        );
        let stress = db.create_run("stress_full_pool", None).await.unwrap();
        let unplanned = db.create_run("escrow_refund", None).await.unwrap();
        db.create_step(&unplanned, "prepare_user_behavior")
            .await
            .unwrap();
        assert_eq!(runner.resume_unfinished().await.unwrap(), 0);
        let stress = db.get_run(&stress).await.unwrap().unwrap();
        assert_eq!(stress.status, "interrupted");
        assert!(stress
            .error_message
            .unwrap()
            .ends_with("a stress_full_pool run is not resumed after a restart"));
        let unplanned_run = db.get_run(&unplanned).await.unwrap().unwrap();
        assert_eq!(unplanned_run.status, "interrupted");
        assert!(unplanned_run
            .error_message
            .unwrap()
            .contains("its plan cannot be read"));
        assert_eq!(
            db.get_steps(&unplanned).await.unwrap()[0].status,
            "interrupted"
        );
    }

    /// A resumed run carries on with the competition it made. A step still running at the
    /// restart finishes in its own row, and a step it had finished is not saved again when its
    /// check passes again.
    #[tokio::test]
    async fn a_resumed_run_finishes_open_steps_in_place_and_keeps_finished_ones() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let run_id = db.create_run("escrow_refund", None).await.unwrap();
        let competition_id = Uuid::now_v7();
        let created = serde_json::json!({ "competition_id": competition_id }).to_string();
        db.add_step(&run_id, "create_competition", 5, None, Some(&created))
            .await
            .unwrap();
        let waiting = db.create_step(&run_id, "wait_cancelled").await.unwrap();
        let prior = Arc::new(resume::Prior::new(
            OffsetDateTime::now_utc() - time::Duration::minutes(1),
            db.get_steps(&run_id).await.unwrap(),
        ));
        let events = Events::new();
        let (steps, to_save) = mpsc::unbounded_channel();
        let saving = tokio::spawn(save_steps(
            db.clone(),
            events.clone(),
            run_id.clone(),
            Some(prior.clone()),
            to_save,
        ));
        let recorder = Recorder {
            run_id: run_id.clone(),
            competition_id,
            events,
            live: Arc::new(DashMap::new()),
            steps,
            prior: Some(prior),
        };
        let step = |name: &str| StepResult {
            name: name.into(),
            status: StepStatus::Passed,
            duration_ms: 1,
            details: None,
            error: None,
        };
        RECORDER
            .scope(recorder, async {
                assert_eq!(resumed_competition(), Some(competition_id));
                let anchor = resumed_start().unwrap();
                assert!(anchor.elapsed() >= std::time::Duration::from_secs(55));
                assert_eq!(
                    prior_step("wait_cancelled").map(|row| row.status),
                    Some("running".to_string())
                );
                assert!(prior_step("refund_alice").is_none());
                let mut created = step("create_competition");
                created.details = Some(serde_json::json!({ "competition_id": competition_id }));
                step_finished(&created);
                step_finished(&step("wait_cancelled"));
                step_finished(&step("refund_alice"));
            })
            .await;
        saving.await.unwrap();
        let saved = db.get_steps(&run_id).await.unwrap();
        assert_eq!(
            saved
                .iter()
                .map(|step| (step.step_name.as_str(), step.status.as_str()))
                .collect::<Vec<_>>(),
            [
                ("create_competition", "passed"),
                ("wait_cancelled", "passed"),
                ("refund_alice", "passed"),
            ]
        );
        assert_eq!(
            saved[1].id, waiting,
            "finished in the row it was running in"
        );
        // Outside a run, nothing is resumed.
        assert!(resumed_competition().is_none() && prior_step("wait_cancelled").is_none());
    }

    /// Lanes start apart, so their first runs do not ask the oracle at once.
    #[tokio::test(start_paused = true)]
    async fn a_lane_waits_its_offset_before_its_first_run() {
        let lane = lanes::LaneConfig {
            name: "queued".into(),
            interval_secs: 3600,
            align: lanes::Align::Interval,
            scenarios: vec![scenarios::queued::QUEUED_ONE_POOL.into()],
            observation_windows_secs: Some(vec![86_400]),
            entry_window_secs: Some(7200),
            stations: None,
            stations_per_run: None,
            fill: scenarios::Fill::Immediate,
            early_players: 1,
            backfill_before_close_secs: 1800,
            backfill_margin: 1,
            unlisted: None,
            stress: None,
            picker: None,
        };
        let base = ScenarioConfig::default();
        let (started, mut starts) = mpsc::unbounded_channel();
        let begun = tokio::time::Instant::now();
        let running = tokio::spawn(async move {
            lane_loop(&lane, &base, None, lanes::start_offset(2), |_, _| {
                started.send(begun.elapsed().as_secs()).unwrap()
            })
            .await
        });
        let first = starts.recv().await.unwrap();
        assert!((89..=90).contains(&first), "first run at {first} s");
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
            None,
            to_save,
        ));
        let recorder = Recorder {
            run_id: run_id.clone(),
            competition_id,
            events: events.clone(),
            live: live.clone(),
            steps,
            prior: None,
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
