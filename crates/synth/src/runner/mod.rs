use crate::client::CoordinatorClient;
use crate::db::SynthDb;
use crate::scenarios::{self, ScenarioConfig, ScenarioResult, ScenarioStatus};
use anyhow::Result;
use log::{error, info};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared state for the runner
#[derive(Clone)]
pub struct Runner {
    client: CoordinatorClient,
    db: SynthDb,
    last_result: Arc<Mutex<Option<ScenarioResult>>>,
}

impl Runner {
    pub fn new(client: CoordinatorClient, db: SynthDb) -> Self {
        Self {
            client,
            db,
            last_result: Arc::new(Mutex::new(None)),
        }
    }

    /// Run a scenario by name and record results
    pub async fn run_scenario(
        &self,
        scenario: &str,
        config: ScenarioConfig,
    ) -> Result<ScenarioResult> {
        let config_json = serde_json::to_string(&config)?;
        let run_id = self.db.create_run(scenario, Some(&config_json)).await?;

        info!("Starting scenario '{}' (run: {})", scenario, run_id);

        let result = match scenario {
            "full_lifecycle" => {
                scenarios::run_full_lifecycle(&self.client, &self.db, &config).await
            }
            other => {
                error!("Unknown scenario: {}", other);
                return Err(anyhow::anyhow!("Unknown scenario: {}", other));
            }
        };

        // Record results to DB
        let error_msg = result.error.as_deref();
        self.db.complete_run(&run_id, error_msg).await?;

        for step in &result.steps {
            let step_id = self.db.create_step(&run_id, &step.name).await?;
            let details = step.details.as_ref().map(|d| d.to_string());
            self.db
                .complete_step(
                    &step_id,
                    step.duration_ms,
                    step.error.as_deref(),
                    details.as_deref(),
                )
                .await?;
        }

        // Store last result for metrics/UI
        *self.last_result.lock().await = Some(result.clone());

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

    /// Get database reference
    pub fn db(&self) -> &SynthDb {
        &self.db
    }

    /// Start the scheduled runner loop
    pub async fn run_scheduled(&self, interval_secs: u64, scenario: &str) {
        info!(
            "Starting scheduled runner: '{}' every {}s",
            scenario, interval_secs
        );

        let config = ScenarioConfig::default();
        loop {
            if let Err(e) = self.run_scenario(scenario, config.clone()).await {
                error!("Scheduled run failed: {}", e);
            }
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        }
    }
}
