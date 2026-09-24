use crate::client::CoordinatorClient;
use crate::db::SynthDb;
use crate::events::{Event, Events};
use crate::scenarios::{
    self, ScenarioConfig, ScenarioResult, ScenarioStatus, StepResult, StepStatus,
};
use anyhow::Result;
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

/// The run in progress, and the step it is on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveRun {
    pub run_id: String,
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
    live: Arc<std::sync::Mutex<Option<LiveRun>>>,
}

tokio::task_local! {
    /// The run a scenario is recording into, for the steps it records.
    static RECORDER: Recorder;
}

/// Saves and announces a run's steps as the scenario records them.
#[derive(Clone)]
struct Recorder {
    run_id: String,
    events: Events,
    live: Arc<std::sync::Mutex<Option<LiveRun>>>,
    steps: mpsc::UnboundedSender<StepResult>,
}

/// Announce that the running scenario has begun `step`. Does nothing outside a run.
pub(crate) fn step_started(step: &str) {
    let _ = RECORDER.try_with(|recorder| {
        if let Some(live) = recorder.live.lock().expect("live run lock").as_mut() {
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
        let _ = recorder.steps.send(step.clone());
    });
}

/// Save each step in the order recorded, announcing each once it is saved.
async fn save_steps(
    db: SynthDb,
    events: Events,
    run_id: String,
    mut steps: mpsc::UnboundedReceiver<StepResult>,
) {
    while let Some(step) = steps.recv().await {
        let details = step.details.as_ref().map(|d| d.to_string());
        let saved = async {
            let step_id = db.create_step(&run_id, &step.name).await?;
            db.complete_step(
                &step_id,
                step.duration_ms,
                step.error.as_deref(),
                details.as_deref(),
            )
            .await
        }
        .await;
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

impl Runner {
    pub fn new(client: CoordinatorClient, db: SynthDb, events: Events) -> Self {
        Self {
            client,
            db,
            events,
            last_result: Arc::new(Mutex::new(None)),
            live: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Run a scenario by name, saving each step as it finishes.
    pub async fn run_scenario(
        &self,
        scenario: &str,
        config: ScenarioConfig,
    ) -> Result<ScenarioResult> {
        if !matches!(scenario, "full_lifecycle" | "escrow_refund") {
            error!("Unknown scenario: {}", scenario);
            return Err(anyhow::anyhow!("Unknown scenario: {}", scenario));
        }
        let config_json = serde_json::to_string(&config)?;
        let run_id = self.db.create_run(scenario, Some(&config_json)).await?;

        info!("Starting scenario '{}' (run: {})", scenario, run_id);
        *self.live.lock().expect("live run lock") = Some(LiveRun {
            run_id: run_id.clone(),
            scenario: scenario.to_string(),
            started_at: OffsetDateTime::now_utc(),
            current_step: None,
        });
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
                    _ => scenarios::run_escrow_refund(&self.client, &self.db, &config).await,
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
        *self.live.lock().expect("live run lock") = None;
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

    /// The run in progress, if any.
    pub fn live(&self) -> Option<LiveRun> {
        self.live.lock().expect("live run lock").clone()
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

    /// Start the scheduled runner loop
    pub async fn run_scheduled(&self, interval_secs: u64, scenario: &str, config: ScenarioConfig) {
        info!(
            "Starting scheduled runner: '{}' every {}s",
            scenario, interval_secs
        );

        loop {
            if let Err(e) = self.run_scenario(scenario, config.clone()).await {
                error!("Scheduled run failed: {:?}", e);
            }
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        }
    }

    /// Announce when a competition a recent run made changes state. A run ends once its
    /// competition awaits attestation, but the money keeps moving after that: attestation,
    /// payouts, refunds.
    pub async fn watch_competitions(&self, interval_secs: u64) {
        let mut seen: HashMap<Uuid, String> = HashMap::new();
        loop {
            if let Err(e) = self.check_competitions(&mut seen).await {
                warn!("Cannot check the runs' competitions: {e:#}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        }
    }

    async fn check_competitions(&self, seen: &mut HashMap<Uuid, String>) -> Result<()> {
        let watched: Vec<Uuid> = self
            .db
            .list_runs(20)
            .await?
            .into_iter()
            .filter_map(|run| run.competition_id?.parse().ok())
            .collect();
        if watched.is_empty() {
            return Ok(());
        }
        for competition in self.client.list_competitions().await? {
            if !watched.contains(&competition.id) {
                continue;
            }
            let state = competition
                .state
                .clone()
                .unwrap_or_else(|| competition.inferred_status().to_string());
            let changed = seen
                .insert(competition.id, state.clone())
                .is_some_and(|previous| previous != state);
            if changed {
                self.events.send(Event::CompetitionChanged {
                    competition_id: competition.id,
                    state,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let live = Arc::new(std::sync::Mutex::new(Some(LiveRun {
            run_id: run_id.clone(),
            scenario: "full_lifecycle".into(),
            started_at: OffsetDateTime::now_utc(),
            current_step: None,
        })));
        let (steps, to_save) = mpsc::unbounded_channel();
        let saving = tokio::spawn(save_steps(
            db.clone(),
            events.clone(),
            run_id.clone(),
            to_save,
        ));
        let recorder = Recorder {
            run_id: run_id.clone(),
            events: events.clone(),
            live: live.clone(),
            steps,
        };

        RECORDER
            .scope(recorder, async {
                step_started("create_competition");
                assert_eq!(
                    live.lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .current_step
                        .as_deref(),
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
            })
            .await;
        saving.await.unwrap();
    }
}
