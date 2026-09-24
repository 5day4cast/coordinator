use super::live::{self, Live};
use crate::rebalance::Rebalancer;
use crate::runner::Runner;
use crate::scenarios::{ScenarioConfig, ScenarioStatus};
use crate::trail::tracker::Tracker;
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use maud::{html, Markup};
use serde::Deserialize;

/// What the dashboard's handlers share: the runner, and the configured run a trigger starts from.
#[derive(Clone)]
pub struct Dashboard {
    pub runner: Runner,
    pub scenario_config: ScenarioConfig,
    /// Absent when no rebalancing is configured.
    pub rebalancer: Option<Rebalancer>,
    /// Follows each run's money after its steps.
    pub tracker: Tracker,
    /// The pages being watched, and what is pushed to them.
    pub live: Live,
}

impl axum::extract::FromRef<Dashboard> for Runner {
    fn from_ref(dashboard: &Dashboard) -> Self {
        dashboard.runner.clone()
    }
}

pub fn router(state: Dashboard) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/api/run", post(trigger_run))
        .route("/api/status", get(status))
        .route("/api/history", get(history))
        .route("/api/rebalance", post(trigger_rebalance))
        .route("/runs/{id}", get(super::run_detail::run_detail))
        .route("/api/live", get(live::stream))
        .with_state(state)
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

/// How many of a scenario's recent runs the dashboard judges it by.
const HEALTH_WINDOW: i64 = 20;

async fn dashboard(State(state): State<Dashboard>) -> Html<String> {
    let header = html! { h1 { "Synth Dashboard" " " span #live-status .note { "connecting…" } } };
    Html(
        live::page(
            "Synth - Synthetic Testing Dashboard",
            live::DASHBOARD,
            header,
            dashboard_live(&state).await,
        )
        .into_string(),
    )
}

/// The dashboard's live part: what the page shows, and what is pushed to it as things change.
pub(super) async fn dashboard_live(
    Dashboard {
        runner, rebalancer, ..
    }: &Dashboard,
) -> Markup {
    let last = runner.last_result().await;
    let observation = match &rebalancer {
        Some(rebalancer) => Some(rebalancer.last().await).filter(|o| o.checked_at.is_some()),
        None => None,
    };
    let rebalances = runner.db().list_rebalances(10).await.unwrap_or_default();
    let runs = runner.db().list_runs(10).await.unwrap_or_default();
    let live = runner.live();
    let health = runner
        .db()
        .scenario_health(HEALTH_WINDOW)
        .await
        .unwrap_or_default();

    html! {
                    @if let Some(live) = &live {
                        p.running {
                            "Running now: " a href=(format!("/runs/{}", live.run_id)) { (live.scenario) }
                            " — " strong { (live.current_step.as_deref().unwrap_or("starting")) }
                        }
                    }

                    section.health {
                        h2 { "Scenarios" }
                        @if health.is_empty() {
                            p { "No runs yet" }
                        } @else {
                            table {
                                thead {
                                    tr {
                                        th { "Scenario" } th { "Last" } th { "Passing" }
                                        th { "Last run" }
                                    }
                                }
                                tbody {
                                    @for scenario in &health {
                                        tr {
                                            td { (scenario.scenario) }
                                            td {
                                                span class=(format!("badge {}", scenario.last_status)) {
                                                    (scenario.last_status)
                                                }
                                            }
                                            td {
                                                (scenario.passed) "/" (scenario.runs)
                                                @if scenario.failed > 0 {
                                                    span.error { " (" (scenario.failed) " failed)" }
                                                }
                                            }
                                            td { (scenario.last_started_at) }
                                        }
                                    }
                                }
                            }
                            p.note {
                                "Of the last " (HEALTH_WINDOW) " runs of each scenario."
                            }
                        }
                    }

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
                            @if let Some(run) = runs.first() {
                                p { a href=(format!("/runs/{}", run.id)) { "See where the money went →" } }
                            }
                        } @else {
                            p { "No runs yet" }
                        }
                    }

                    section.history {
                        h2 { "Recent Runs" }
                        table {
                            thead {
                                tr { th { "Run" } th { "Scenario" } th { "Status" } th { "Competition" } th { "Started" } th { "Finished" } }
                            }
                            tbody {
                                @for run in &runs {
                                    tr {
                                        td { a href=(format!("/runs/{}", run.id)) { (run.id.chars().take(8).collect::<String>()) "…" } }
                                        td { (run.scenario) }
                                        td { span class=(format!("badge {}", run.status)) { (run.status) } }
                                        td { code { (run.competition_id.as_deref().map(|id| id.chars().take(8).collect::<String>()).unwrap_or_else(|| "-".into())) } }
                                        td { (run.started_at) }
                                        td {
                                            @if let Some(ref completed) = run.completed_at {
                                                (completed)
                                            } @else if let Some(live) = live.as_ref().filter(|live| live.run_id == run.id) {
                                                span.running { "running: " (live.current_step.as_deref().unwrap_or("starting")) }
                                            } @else {
                                                "never finished"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    section.liquidity {
                        h2 { "Liquidity" }
                        @match (&rebalancer, &observation) {
                            (None, _) => p { "Rebalancing is not configured." },
                            (Some(_), None) => p { "Not checked yet." },
                            (Some(rebalancer), Some(observation)) => {
                                @match &observation.channel {
                                    Some(channel) => p {
                                        "Lightning: the payer holds " strong { (channel.local_sats) }
                                        " of " (channel.local_sats + channel.remote_sats)
                                        " sats in channel " (channel.id)
                                        ", rebalancing below " (rebalancer.config().low_percent) "%."
                                    },
                                    None => p.error { "The payer has no active channel with the source node." },
                                }
                                @match (&rebalancer.config().arkade, &observation.arkade) {
                                    (None, _) => p.note { "Arkade: ark-swapd's wallet is not watched." },
                                    (Some(_), None) => p.error { "Arkade: ark-swapd did not report its wallet." },
                                    (Some(arkade), Some(wallet)) => p {
                                        "Arkade: ark-swapd can fund " strong { (wallet.spendable_sat()) }
                                        " sats of escrows, topped up with " (arkade.top_up_sats)
                                        " sats on-chain below " (arkade.low_sats) "."
                                    },
                                }
                                @if let Some(checked_at) = observation.checked_at {
                                    p.note { "Checked " (checked_at) }
                                }
                            }
                        }
                        @if !rebalances.is_empty() {
                            table {
                                thead {
                                    tr {
                                        th { "When" } th { "Leg" } th { "Moved" } th { "Held before" }
                                        th { "Status" } th { "Error / transaction" }
                                    }
                                }
                                tbody {
                                    @for rebalance in &rebalances {
                                        tr {
                                            td { (rebalance.created_at) }
                                            td { (rebalance.kind.as_deref().unwrap_or("channel")) }
                                            td { (rebalance.amount_sats) " sats" }
                                            td {
                                                (rebalance.local_before_sats)
                                                @if rebalance.capacity_sats > 0 { " / " (rebalance.capacity_sats) }
                                            }
                                            td { span class=(format!("badge {}", rebalance.status)) { (rebalance.status) } }
                                            td {
                                                @if let Some(error) = &rebalance.error_message { span.error { (error) } }
                                                @else if let Some(txid) = &rebalance.txid { code { (txid) } }
                                                @else { "-" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    section.actions {
                        h2 { "Actions" }
                        form method="POST" action="/api/run" data-async {
                            button type="submit" { "Run Full Lifecycle" }
                        }
                        form method="POST" action="/api/run?scenario=escrow_refund" data-async {
                            button type="submit" { "Run Escrow Refund" }
                        }
                        @if rebalancer.is_some() {
                            form method="POST" action="/api/rebalance" data-async {
                                button type="submit" { "Rebalance Now" }
                            }
                        }
                    }
    }
}

async fn trigger_run(
    State(Dashboard {
        runner,
        scenario_config,
        ..
    }): State<Dashboard>,
    Query(params): Query<RunParams>,
) -> impl IntoResponse {
    let scenario = params
        .scenario
        .unwrap_or_else(|| "full_lifecycle".to_string());
    let mut config = scenario_config;
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

async fn trigger_rebalance(
    State(Dashboard { rebalancer, .. }): State<Dashboard>,
) -> impl IntoResponse {
    let Some(rebalancer) = rebalancer else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "rebalancing is not configured" })),
        );
    };
    match rebalancer.rebalance().await {
        Ok(moved) => (axum::http::StatusCode::OK, Json(serde_json::json!(moved))),
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": format!("{e:#}") })),
        ),
    }
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

pub(super) const DASHBOARD_CSS: &str = r#"
body { font-family: monospace; max-width: 960px; margin: 0 auto; padding: 20px; background: #1a1a2e; color: #e0e0e0; }
h1 { color: #00d4ff; }
h2 { color: #7b68ee; border-bottom: 1px solid #333; padding-bottom: 5px; }
table { width: 100%; border-collapse: collapse; margin: 10px 0; }
th, td { padding: 8px; text-align: left; border-bottom: 1px solid #333; }
th { background: #16213e; }
.badge { padding: 2px 8px; border-radius: 4px; font-size: 0.85em; }
.passed, .passed .badge, .moved { color: #00ff88; }
.failed, .failed .badge { color: #ff4444; }
.running, .running .badge { color: #ffaa00; }
.skipped { color: #888; }
.error { color: #ff4444; font-size: 0.9em; }
.result { padding: 15px; background: #16213e; border-radius: 8px; margin: 10px 0; }
button { background: #7b68ee; color: white; border: none; padding: 10px 20px; border-radius: 4px; cursor: pointer; font-family: monospace; }
button:hover { background: #6a5acd; }
form { margin: 10px 0; display: inline-block; margin-right: 8px; }
.note { color: #888; font-size: 0.85em; }
"#;
