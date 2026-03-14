use crate::runner::Runner;
use crate::scenarios::{ScenarioConfig, ScenarioStatus};
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use maud::{html, DOCTYPE};
use serde::Deserialize;

pub fn router(runner: Runner) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/api/run", post(trigger_run))
        .route("/api/status", get(status))
        .route("/api/history", get(history))
        .with_state(runner)
}

#[derive(Debug, Deserialize)]
struct RunParams {
    scenario: Option<String>,
    users: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct HistoryParams {
    limit: Option<i64>,
}

async fn dashboard(State(runner): State<Runner>) -> impl IntoResponse {
    let last = runner.last_result().await;
    let runs = runner.db().list_runs(10).await.unwrap_or_default();

    Html(
        html! {
            (DOCTYPE)
            html {
                head {
                    title { "Synth - Synthetic Testing Dashboard" }
                    style { (DASHBOARD_CSS) }
                }
                body {
                    h1 { "Synth Dashboard" }

                    section.status {
                        h2 { "Last Run" }
                        @if let Some(ref result) = last {
                            div class=(format!("result {}", status_class(&result.status))) {
                                p { "Scenario: " strong { (result.scenario) } }
                                p { "Status: " span class=(format!("badge {}", status_class(&result.status))) { (format!("{:?}", result.status)) } }
                                p { "Duration: " (result.total_duration_ms) "ms" }
                                @if let Some(ref err) = result.error {
                                    p.error { "Error: " (err) }
                                }
                            }
                            h3 { "Steps" }
                            table {
                                thead {
                                    tr { th { "Step" } th { "Status" } th { "Duration" } th { "Error" } }
                                }
                                tbody {
                                    @for step in &result.steps {
                                        tr class=(step_class(&step.status)) {
                                            td { (step.name) }
                                            td { (format!("{:?}", step.status)) }
                                            td { (step.duration_ms) "ms" }
                                            td { (step.error.as_deref().unwrap_or("-")) }
                                        }
                                    }
                                }
                            }
                        } @else {
                            p { "No runs yet" }
                        }
                    }

                    section.history {
                        h2 { "Recent Runs" }
                        table {
                            thead {
                                tr { th { "ID" } th { "Scenario" } th { "Status" } th { "Started" } th { "Duration" } }
                            }
                            tbody {
                                @for run in &runs {
                                    tr {
                                        td { (run.id.chars().take(8).collect::<String>()) "..." }
                                        td { (run.scenario) }
                                        td { span class=(format!("badge {}", run.status)) { (run.status) } }
                                        td { (run.started_at) }
                                        td {
                                            @if let Some(ref completed) = run.completed_at {
                                                (completed)
                                            } @else {
                                                "running..."
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    section.actions {
                        h2 { "Actions" }
                        form method="POST" action="/api/run" {
                            button type="submit" { "Run Full Lifecycle" }
                        }
                    }
                }
            }
        }
        .into_string(),
    )
}

async fn trigger_run(
    State(runner): State<Runner>,
    Query(params): Query<RunParams>,
) -> impl IntoResponse {
    let scenario = params
        .scenario
        .unwrap_or_else(|| "full_lifecycle".to_string());
    let mut config = ScenarioConfig::default();
    if let Some(users) = params.users {
        config.users = users;
    }

    let scenario_name = scenario.clone();

    // Run in background so we don't block the response
    let runner_clone = runner.clone();
    tokio::spawn(async move {
        if let Err(e) = runner_clone.run_scenario(&scenario, config).await {
            log::error!("Triggered run failed: {}", e);
        }
    });

    Json(serde_json::json!({
        "status": "started",
        "scenario": scenario_name
    }))
}

async fn status(State(runner): State<Runner>) -> impl IntoResponse {
    let last = runner.last_result().await;
    Json(serde_json::json!({
        "last_result": last
    }))
}

async fn history(
    State(runner): State<Runner>,
    Query(params): Query<HistoryParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(20);
    let runs = runner.db().list_runs(limit).await.unwrap_or_default();
    Json(serde_json::json!({ "runs": runs }))
}

fn status_class(status: &ScenarioStatus) -> &'static str {
    match status {
        ScenarioStatus::Passed => "passed",
        ScenarioStatus::Failed => "failed",
        ScenarioStatus::Running => "running",
    }
}

fn step_class(status: &crate::scenarios::StepStatus) -> &'static str {
    match status {
        crate::scenarios::StepStatus::Passed => "passed",
        crate::scenarios::StepStatus::Failed => "failed",
        crate::scenarios::StepStatus::Skipped => "skipped",
    }
}

const DASHBOARD_CSS: &str = r#"
body { font-family: monospace; max-width: 960px; margin: 0 auto; padding: 20px; background: #1a1a2e; color: #e0e0e0; }
h1 { color: #00d4ff; }
h2 { color: #7b68ee; border-bottom: 1px solid #333; padding-bottom: 5px; }
table { width: 100%; border-collapse: collapse; margin: 10px 0; }
th, td { padding: 8px; text-align: left; border-bottom: 1px solid #333; }
th { background: #16213e; }
.badge { padding: 2px 8px; border-radius: 4px; font-size: 0.85em; }
.passed, .passed .badge { color: #00ff88; }
.failed, .failed .badge { color: #ff4444; }
.running, .running .badge { color: #ffaa00; }
.skipped { color: #888; }
.error { color: #ff4444; font-size: 0.9em; }
.result { padding: 15px; background: #16213e; border-radius: 8px; margin: 10px 0; }
button { background: #7b68ee; color: white; border: none; padding: 10px 20px; border-radius: 4px; cursor: pointer; font-family: monospace; }
button:hover { background: #6a5acd; }
form { margin: 10px 0; }
"#;
