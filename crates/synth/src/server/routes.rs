use super::format;
use super::live::{self, Live};
use crate::rebalance::Rebalancer;
use crate::runner::Runner;
use crate::scenarios::{ScenarioConfig, ScenarioStatus};
use crate::trail::label_words;
use crate::trail::tracker::Tracker;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use maud::{html, Markup};
use serde::Deserialize;
use time::OffsetDateTime;

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

#[cfg(test)]
impl Dashboard {
    /// A dashboard on `db` that reaches no coordinator, node or ark-swapd.
    pub(crate) fn for_tests(db: crate::db::SynthDb) -> Self {
        let client = crate::client::CoordinatorClient::new("http://127.0.0.1:1", None);
        let events = crate::events::Events::new();
        let tracker = Tracker::new(
            client.clone(),
            db.clone(),
            events.clone(),
            None,
            Vec::new(),
            None,
            None,
            crate::trail::tracker::TrailConfig::default(),
        )
        .expect("a tracker that reaches nothing");
        Self {
            runner: Runner::new(client, db, events),
            scenario_config: ScenarioConfig::default(),
            rebalancer: None,
            tracker,
            live: Live::new(),
        }
    }
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
        .route("/runs/{id}/trail.json", get(super::run_detail::trail_json))
        .route("/runs/{id}/trail.tsv", get(super::run_detail::trail_tsv))
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

async fn dashboard(State(state): State<Dashboard>, headers: HeaderMap) -> Response {
    let live = dashboard_live(&state).await;
    if from_htmx(&headers) {
        return html_by_hx_request(live);
    }
    let header = html! { h1 { "Synth Dashboard" } };
    html_by_hx_request(live::page(
        "Synth - Synthetic Testing Dashboard",
        live::DASHBOARD,
        header,
        live,
    ))
}

/// A page, or its live part alone when htmx asks for it. The two share a URL, so caches must
/// tell them apart.
pub(super) fn html_by_hx_request(markup: Markup) -> Response {
    ([(header::VARY, "HX-Request")], Html(markup.into_string())).into_response()
}

/// The dashboard's live part: what the page shows, and what is pushed to it as things change.
pub(super) async fn dashboard_live(
    Dashboard {
        runner, rebalancer, ..
    }: &Dashboard,
) -> Markup {
    let now = OffsetDateTime::now_utc();
    let last = runner.last_result().await;
    let observation = match &rebalancer {
        Some(rebalancer) => Some(rebalancer.last().await).filter(|o| o.checked_at.is_some()),
        None => None,
    };
    let rebalances = runner.db().list_rebalances(10).await.unwrap_or_default();
    // The rebalanced channel joins the payer and the source node, whatever it is called now.
    let channel_ends = observation.as_ref().map(|observation| {
        format!(
            "{} ↔ {}",
            name(observation.payer.as_ref(), "the payer"),
            name(observation.source.as_ref(), "the source node")
        )
    });
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
                div.scroll { table {
                    thead { tr { th { "Scenario" } th { "Last" } th { "Passing" } th { "Last run" } } }
                    tbody {
                        @for scenario in &health {
                            tr {
                                td { (scenario.scenario) }
                                td { span class=(format!("badge {}", scenario.last_status)) { (scenario.last_status) } }
                                td {
                                    (scenario.passed) "/" (scenario.runs)
                                    @if scenario.failed > 0 { " (" (scenario.failed) " failed)" }
                                }
                                td { (format::time_text(&scenario.last_started_at, now)) }
                            }
                        }
                    }
                } }
                p.note {
                    "Of the last " (HEALTH_WINDOW) " runs of each scenario. A run whose money "
                    "got stuck counts as failed; money synth could not verify does not."
                }
            }
        }

        section.status {
            h2 { "Last Run" }
            @if let Some(ref result) = last {
                div.result {
                    p { "Scenario: " strong { (result.scenario) } " "
                        span class=(format!("badge {}", status_class(&result.status))) { (status_class(&result.status)) } }
                    p { "Took " (format::duration_ms(result.total_duration_ms)) }
                    @if let Some(ref err) = result.error {
                        p.error { "Error: " (err) }
                    }
                }
                h3 { "Steps" }
                div.scroll { table {
                    thead { tr { th { "Step" } th { "Status" } th.num { "Took" } th { "Error" } } }
                    tbody {
                        @for step in &result.steps {
                            tr {
                                td { (step.name) }
                                td { span class=(format!("badge {}", step_class(&step.status))) { (step_class(&step.status)) } }
                                td.num { (format::duration_ms(step.duration_ms)) }
                                td { @if let Some(error) = &step.error { span.error { (error) } } @else { "-" } }
                            }
                        }
                    }
                } }
                @if let Some(run) = runs.first() {
                    p { a href=(format!("/runs/{}", run.id)) { "See where the money went →" } }
                }
            } @else {
                p { "No runs since synth started." }
            }
        }

        section.history {
            h2 { "Recent Runs" }
            div.scroll { table {
                thead { tr { th { "Run" } th { "Scenario" } th { "Status" } th { "Money" } th { "Started" } th { "Finished" } } }
                tbody {
                    @for run in &runs {
                        tr {
                            td { a href=(format!("/runs/{}", run.id)) title=(run.id) { (short_id(&run.id)) } }
                            td { (run.scenario) }
                            td { span class=(format!("badge {}", run.status)) { (run.status) } }
                            td {
                                @match run.money.as_deref() {
                                    Some(money) => span class=(format!("badge {money}")) { (label_words(money)) },
                                    None => "-",
                                }
                            }
                            td { (format::time_text(&run.started_at, now)) }
                            td {
                                @if let Some(ref completed) = run.completed_at {
                                    (format::time_text(completed, now))
                                } @else if let Some(live) = live.as_ref().filter(|live| live.run_id == run.id) {
                                    span.running { "running: " (live.current_step.as_deref().unwrap_or("starting")) }
                                } @else {
                                    "not yet"
                                }
                            }
                        }
                    }
                }
            } }
        }

        section.liquidity {
            h2 { "Liquidity" }
            @match (&rebalancer, &observation) {
                (None, _) => p { "Rebalancing is not configured." },
                (Some(_), None) => p { "Not checked yet." },
                (Some(rebalancer), Some(observation)) => {
                    @match &observation.channel {
                        Some(channel) => p {
                            "Lightning: the payer, " strong { (name(observation.payer.as_ref(), "the payer")) }
                            ", holds " strong { (channel.local_sats) } " of " (channel.local_sats + channel.remote_sats)
                            " sats in channel " code { (channel.id) } " with "
                            strong { (name(observation.source.as_ref(), "the source node")) }
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
                        p.note { "Checked " (format::time(checked_at, now)) }
                    }
                }
            }
            @if !rebalances.is_empty() {
                div.scroll { table {
                    thead { tr {
                        th { "When" } th { "Leg" } th.num { "Moved" } th.num { "Held before" }
                        th { "Status" } th { "Error / transaction" }
                    } }
                    tbody {
                        @for rebalance in &rebalances {
                            tr {
                                td { (format::time_text(&rebalance.created_at, now)) }
                                td {
                                    (rebalance.kind.as_deref().unwrap_or("channel"))
                                    @if rebalance.kind.as_deref() != Some("arkade") {
                                        br; code { (rebalance.channel_id) }
                                        @if let Some(ends) = &channel_ends { br; span.note { (ends) } }
                                    }
                                }
                                td.num { (rebalance.amount_sats) " sats" }
                                td.num {
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
                } }
            }
        }

        section.actions {
            h2 { "Actions" }
            button hx-post="/api/run" hx-target="#action-result"
                hx-confirm="Start a full lifecycle run? It pays real entries from the payer's node." {
                "Run Full Lifecycle"
            }
            button hx-post="/api/run?scenario=escrow_refund" hx-target="#action-result"
                hx-confirm="Start an escrow refund run? It pays real entries and waits for their refunds." {
                "Run Escrow Refund"
            }
            @if rebalancer.is_some() {
                // A payment and an on-chain send can outlast htmx 4's 60 s default timeout.
                button hx-post="/api/rebalance" hx-target="#action-result" hx-config="timeout:5m"
                    hx-confirm="Rebalance now? This moves real sats between the nodes and may send ark-swapd coins on-chain." {
                    "Rebalance Now"
                }
            }
        }
        p.note { "Updated " (format::time(now, now)) }
    }
}

/// A node by the name it gives itself, else by the start of its key.
fn name(node: Option<&crate::lnd::NodeIdentity>, fallback: &str) -> String {
    match node {
        Some(node) if !node.alias.is_empty() => node.alias.clone(),
        Some(node) => short_id(&node.pubkey),
        None => fallback.to_string(),
    }
}

pub(super) fn short_id(id: &str) -> String {
    let short: String = id.chars().take(8).collect();
    if short.len() < id.len() {
        format!("{short}…")
    } else {
        short
    }
}

/// Whether htmx sent the request, so it wants a fragment rather than a whole page or JSON.
pub(super) fn from_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

async fn trigger_run(
    State(Dashboard {
        runner,
        scenario_config,
        ..
    }): State<Dashboard>,
    Query(params): Query<RunParams>,
    headers: HeaderMap,
) -> Response {
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

    if from_htmx(&headers) {
        return Html(
            html! { "Started " strong { (scenario_name) } "; it shows under Running now." }
                .into_string(),
        )
        .into_response();
    }
    Json(serde_json::json!({
        "status": "started",
        "scenario": scenario_name
    }))
    .into_response()
}

async fn trigger_rebalance(
    State(Dashboard { rebalancer, .. }): State<Dashboard>,
    headers: HeaderMap,
) -> Response {
    let htmx = from_htmx(&headers);
    let Some(rebalancer) = rebalancer else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "rebalancing is not configured" })),
        )
            .into_response();
    };
    // A rebalance moves money, so it runs to the end even if the page that asked goes away.
    let moved = match tokio::spawn(async move { rebalancer.rebalance().await }).await {
        Ok(moved) => moved,
        Err(e) => Err(anyhow::anyhow!("the rebalance stopped: {e}")),
    };
    match (moved, htmx) {
        (Ok(moved), true) => Html(
            html! {
                "Rebalanced: "
                (moved.channel_sats.map_or("nothing".to_string(), |sats| format!("{sats} sats")))
                " over the channel, "
                (moved.arkade_sats.map_or("nothing".to_string(), |sats| format!("{sats} sats")))
                " on-chain to ark-swapd."
            }
            .into_string(),
        )
        .into_response(),
        (Ok(moved), false) => Json(serde_json::json!(moved)).into_response(),
        // htmx swaps only successful responses in, so a failure is said with a 200.
        (Err(e), true) => {
            Html(html! { span.error { "Rebalance failed: " (format!("{e:#}")) } }.into_string())
                .into_response()
        }
        (Err(e), false) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": format!("{e:#}") })),
        )
            .into_response(),
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

