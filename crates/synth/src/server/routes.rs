use super::format;
use super::live::{self, Live};
use crate::db::HeldRun;
use crate::rebalance::Rebalancer;
use crate::runner::Runner;
use crate::scenarios::{ScenarioConfig, ScenarioStatus};
use crate::trail::tracker::{Tracker, UnrecordedSwaps};
use crate::trail::{label_words, Held};
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
        runner,
        rebalancer,
        tracker,
        ..
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
    let held = runner.db().held_runs().await.unwrap_or_default();
    let unrecorded = tracker.unrecorded_swaps();

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

        (stuck_money(&held, unrecorded.as_ref(), now))

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
                            td { (run_status(&run.status, run.money.as_deref())) }
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
                            ", holds " strong { (format::sats(channel.local_sats)) } " of " (format::sats(channel.local_sats + channel.remote_sats))
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
                            "Arkade: ark-swapd can fund " strong { (format::sats(wallet.spendable_sat())) }
                            " sats of escrows, topped up with " (format::sats(arkade.top_up_sats))
                            " sats on-chain below " (format::sats(arkade.low_sats)) "."
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
                                td.num { (format::sats_signed(rebalance.amount_sats)) " sats" }
                                td.num {
                                    (format::sats_signed(rebalance.local_before_sats))
                                    @if rebalance.capacity_sats > 0 { " / " (format::sats_signed(rebalance.capacity_sats)) }
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

/// Every run whose money is stuck now, oldest first, and the swaps ark-swapd says funded an escrow
/// without recording its output, whoever made them.
fn stuck_money(
    held: &[HeldRun],
    unrecorded: Option<&UnrecordedSwaps>,
    now: OffsetDateTime,
) -> Markup {
    let mut held: Vec<(&HeldRun, &Held)> = held
        .iter()
        .filter_map(|run| Some((run, run.trail.held.as_ref()?)))
        .collect();
    held.sort_by_key(|(_, held)| held.since);
    let total: u64 = held.iter().map(|(_, held)| held.sats).sum();
    let nearest = held
        .iter()
        .filter_map(|(_, held)| held.nearest_expiry)
        .min();
    let swaps = unrecorded.map_or(&[][..], |unrecorded| &unrecorded.swaps[..]);
    let run_of = |payment_hash: &str| {
        held.iter()
            .find(|(run, _)| {
                run.trail
                    .swaps
                    .iter()
                    .any(|swap| swap.payment_hash == payment_hash)
            })
            .map(|(run, _)| run.run.id.clone())
    };
    html! {
        section class=(if held.is_empty() { "stuck-money" } else { "stuck-money held" }) {
            h2 { "Stuck money" }
            @if held.is_empty() {
                p.note { "No run's money is stuck." }
            } @else {
                p {
                    strong { (format::sats(total)) " sats" } " held by " (held.len()) " run(s)"
                    @if let Some(at) = nearest.and_then(|at| OffsetDateTime::from_unix_timestamp(at).ok()) {
                        "; the nearest escrow expiry or refund opening is " (format::time(at, now))
                    }
                    ". Each run's page lays out what holds it and what should move it."
                }
                div.scroll { table {
                    thead { tr { th { "Run" } th { "Held since" } th.num { "Sats" } th { "Nearest expiry" } th { "Why" } } }
                    tbody {
                        @for (run, held) in &held {
                            tr {
                                td { a href=(format!("/runs/{}", run.run.id)) title=(run.run.id) { (short_id(&run.run.id)) } br; span.note { (run.run.scenario) } }
                                td { (format::time(held.since, now)) }
                                td.num { (format::sats(held.sats)) }
                                td {
                                    @match held.nearest_expiry.and_then(|at| OffsetDateTime::from_unix_timestamp(at).ok()) {
                                        Some(at) => (format::time(at, now)),
                                        None => span.note { "not known" },
                                    }
                                }
                                td { (held.reason) }
                            }
                        }
                    }
                } }
            }
            @if let Some(unrecorded) = unrecorded {
                h3 { "Swaps without an escrow output" }
                @if let Some(error) = &unrecorded.error {
                    p.error { "ark-swapd did not list them: " (error) }
                } @else if swaps.is_empty() {
                    p.note { "ark-swapd has none." }
                } @else {
                    p.note {
                        "ark-swapd says these paid an escrow, or were settled, but records no escrow output, "
                        "so nothing can spend or refund them until it is found. Not all are synth's."
                    }
                    div.scroll { table {
                        thead { tr { th { "Swap" } th { "State" } th.num { "Sats" } th { "Escrow address" } th { "Run" } } }
                        tbody {
                            @for swap in swaps {
                                tr {
                                    td { (format::copyable(&swap.id.to_string())) br; span.note { (swap_created(swap.created_at, now)) } }
                                    td { (swap.state) @if let Some(error) = &swap.error { br; span.note { (error) } } }
                                    td.num { (format::sats(swap.amount_sat)) }
                                    td { (format::copyable(&swap.escrow_address)) }
                                    td {
                                        @match run_of(&swap.payment_hash) {
                                            Some(run) => a href=(format!("/runs/{run}")) { (short_id(&run)) },
                                            None => span.note { "not a stuck synth run" },
                                        }
                                    }
                                }
                            }
                        }
                    } }
                }
                p.note { "Asked ark-swapd " (format::time(unrecorded.checked_at, now)) "." }
            }
        }
    }
}

fn swap_created(at: i64, now: OffsetDateTime) -> Markup {
    match OffsetDateTime::from_unix_timestamp(at) {
        Ok(at) if at.unix_timestamp() > 0 => html! { "made " (format::time(at, now)) },
        _ => html! {},
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
                (moved.channel_sats.map_or("nothing".to_string(), |sats| format!("{} sats", format::sats(sats))))
                " over the channel, "
                (moved.arkade_sats.map_or("nothing".to_string(), |sats| format!("{} sats", format::sats(sats))))
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

/// A run's status badge. A run whose steps passed but whose money is stuck now reads as stuck, so
/// it never shows green over money nobody can move. Runs that finished before synth failed them
/// for it keep "passed" in the database, so this is decided when the page is drawn.
pub(super) fn run_status(status: &str, money: Option<&str>) -> Markup {
    if status == "passed" && money == Some("stuck") {
        html! {
            span class="badge stuck" title="Its steps passed, but its money is stuck" { "money stuck" }
        }
    } else {
        html! { span class=(format!("badge {status}")) { (status) } }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SynthDb;
    use std::time::{Duration, Instant};

    /// The load-time budget: anything slower to load is a bug.
    const BUDGET: Duration = Duration::from_millis(400);

    /// A run whose contract failed with its winners unpaid, as run 01a0d0f5's did, with every hop
    /// synth follows and the stuck block: the heaviest run page there is.
    ///
    /// With `fail` unset its status stays "passed", as for a run that finished before synth
    /// failed runs for stuck money.
    async fn seed_stuck_run(db: &SynthDb, competition: &serde_json::Value, fail: bool) -> String {
        use serde_json::json;
        let hash = |n: u8| hex::encode([n; 32]);
        let address = "freya@lnurl.5day4cast.com";
        let run = db
            .create_run("full_lifecycle", Some(r#"{"lnd":{}}"#))
            .await
            .unwrap();
        let details = json!({ "competition_id": competition["id"] }).to_string();
        db.add_step(&run, "create_competition", 1, None, Some(&details))
            .await
            .unwrap();
        let (mut swaps, mut payouts, mut shares) = (Vec::new(), Vec::new(), Vec::new());
        for (n, user) in (1u8..).zip(["alice", "bob", "charlie"]) {
            let entry_id = uuid::Uuid::now_v7();
            let entry = json!({
                "user": user, "nostr_pubkey": hash(0x10 + n), "entry_id": entry_id,
                "ticket_id": uuid::Uuid::now_v7(), "amount_sats": 1100, "payment_hash": hash(n),
                "invoice": format!("lntbs11u1{}", "q".repeat(300)), "lightning_address": address,
                "escrow": { "refund_at": 1_790_300_000, "solo_delay_secs": 86_528 },
                "payment": {
                    "payer_alias": "thor", "payer_pubkey": hash(0x20), "preimage": hash(0x30 + n),
                    "fee_msat": 1001,
                    "route": [
                        { "chan_id": "3771505203178766336", "pubkey": hash(0x21), "alias": "odin" },
                        { "chan_id": "3771505203178766337", "pubkey": hash(0x22), "alias": "swapd" },
                    ],
                },
                "paid": true, "entry_submitted": true,
            });
            db.add_step(
                &run,
                &format!("user_{user}_enter"),
                1,
                None,
                Some(&entry.to_string()),
            )
            .await
            .unwrap();
            swaps.push(json!({
                "user": user, "payment_hash": hash(n), "id": uuid::Uuid::now_v7(),
                "state": "settled", "amount_sat": 1090,
                "escrow_address": format!("tark1{}", "q".repeat(60)),
                "escrow_vtxo": (n > 1).then(|| format!("{}:0", hash(0x40 + n))),
                "ark_txid": hash(0x40 + n), "invoice_state": "SETTLED",
            }));
            payouts.push(json!({
                "user": user, "entry_id": entry_id, "pubkey": hash(0x10 + n), "weight": 33,
                "owed_sats": 1000, "lightning_address": address,
            }));
            shares.push(json!({ "pubkey": hash(0x10 + n), "weight": 33, "owed_sats": 1000 }));
        }
        db.complete_run(&run, None).await.unwrap();
        let mut competition = competition.clone();
        competition["completed_at"] = serde_json::Value::Null;
        competition["delta_broadcasted_at"] = json!("2026-09-24T04:15:12Z");
        competition["failed_at"] = json!("2026-09-24T04:16:13Z");
        let chain = |n: u8| json!({ "txid": hash(n), "vout": 0, "fee_sat": 199, "value_sat": 3000, "confirmed": true });
        let reason = "the competition failed at 04:16 UTC after its outcome went on-chain, and no payout was sent";
        let trail: crate::trail::Trail = serde_json::from_value(json!({
            "refreshed_at": "2026-09-24T05:00:00Z",
            "competition_id": competition["id"],
            "competition": competition,
            "settlement": { "pot_sats": 3000, "decided": { "Attested": 3 }, "shares": shares },
            "swaps": swaps,
            "payouts": payouts,
            "funding_tx": chain(0x50),
            "outcome_tx": chain(0x51),
            "closing_txs": [chain(0x52)],
            "money": { "status": "stuck", "reason": reason, "since": "2026-09-24T04:16:13Z" },
            "held": {
                "since": "2026-09-24T04:16:13Z", "found": "2026-09-24T05:00:00Z",
                "reason": reason, "sats": 3000, "nearest_expiry": 1_790_300_000,
            },
            "gaps": ["the Arkade indexer is not configured"],
        }))
        .unwrap();
        db.record_money(
            &run,
            &crate::db::Verdict {
                trail: &trail,
                follow: true,
                step: Some(("money_stuck", 1, Some(reason), "{}")),
                fail_passed_run: fail.then_some(reason),
            },
        )
        .await
        .unwrap();
        run
    }

    /// A dashboard over one stuck run that passed its steps, and that run's id.
    async fn stuck_dashboard(directory: &tempfile::TempDir) -> (Dashboard, String) {
        let db = SynthDb::new(&directory.path().join("synth.sqlite").display().to_string())
            .await
            .unwrap();
        let competition: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/lab-competition.json")).unwrap();
        let run = seed_stuck_run(&db, &competition, false).await;
        (Dashboard::for_tests(db), run)
    }

    /// The text between `start` and the `end` after it.
    fn between<'a>(page: &'a str, start: &str, end: &str) -> &'a str {
        let from = page
            .find(start)
            .unwrap_or_else(|| panic!("no {start} in {page}"));
        let rest = &page[from..];
        &rest[..rest.find(end).unwrap_or(rest.len())]
    }

    #[tokio::test]
    async fn a_run_that_passed_with_its_money_stuck_reads_as_stuck() {
        let directory = tempfile::tempdir().unwrap();
        let (dashboard, run) = stuck_dashboard(&directory).await;
        let page = super::super::run_detail::run_live(&dashboard, &run)
            .await
            .unwrap()
            .into_string();
        let header = between(&page, "<h1>", "</h1>");
        assert!(header.contains(r#"class="badge stuck""#), "{header}");
        assert!(header.contains("money stuck"), "{header}");
        assert!(!header.contains(r#"class="badge passed""#), "{header}");
        assert!(!header.contains(">passed<"), "{header}");
        assert!(page.contains("Its steps passed, but its money is stuck"));

        let home = dashboard_live(&dashboard).await.into_string();
        let recent = between(&home, "Recent Runs", "</section>");
        assert!(recent.contains("money stuck"), "{recent}");
        assert!(!recent.contains(r#"class="badge passed""#), "{recent}");
    }

    #[test]
    fn only_a_passed_run_with_stuck_money_is_relabelled() {
        let badge = |status, money| run_status(status, money).into_string();
        assert!(badge("passed", Some("paid_out")).contains(r#"class="badge passed""#));
        assert!(badge("passed", None).contains(r#"class="badge passed""#));
        assert!(badge("failed", Some("stuck")).contains(r#"class="badge failed""#));
        assert!(badge("passed", Some("stuck")).contains("money stuck"));
    }

    /// Pages render from synth's own database. With a year of hourly runs behind it, and a run
    /// whose money is stuck, the dashboard, a run's page, its exports, and the tracker's queue
    /// must come back well inside the budget. Each is timed five times; the slowest counts.
    ///
    /// With SYNTH_TIMING_DB set, the database is kept at that path, to time the release binary
    /// over HTTP against it.
    #[tokio::test]
    #[ignore = "timing: run with --ignored --nocapture to measure"]
    async fn pages_render_within_the_load_budget_with_a_year_of_runs() {
        let directory = tempfile::tempdir().unwrap();
        let path = std::env::var("SYNTH_TIMING_DB")
            .unwrap_or_else(|_| directory.path().join("synth.sqlite").display().to_string());
        let db = SynthDb::new(&path).await.unwrap();
        let competition: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/lab-competition.json")).unwrap();
        let trail = serde_json::json!({
            "refreshed_at": "2026-09-24T01:00:00Z",
            "competition_id": competition["id"],
            "competition": competition,
            "money": { "status": "paid_out" },
        })
        .to_string();
        let runs = db.seed_history(24 * 365, &trail).await.unwrap();
        let stuck = seed_stuck_run(&db, &competition, true).await;
        let dashboard = Dashboard::for_tests(db.clone());
        eprintln!(
            "seeded {} runs at {path}; stuck run {stuck}",
            runs.len() + 1
        );

        async fn slowest<F: std::future::Future>(label: &str, mut work: impl FnMut() -> F) {
            let mut slowest = Duration::ZERO;
            for _ in 0..5 {
                let start = Instant::now();
                work().await;
                slowest = slowest.max(start.elapsed());
            }
            eprintln!("{label}: {slowest:?}");
            assert!(slowest < BUDGET, "{label} took {slowest:?}");
        }
        let id = |run: &str| axum::extract::Path(run.to_string());
        slowest("dashboard", || dashboard_live(&dashboard)).await;
        slowest("a paid-out run's page", || {
            super::super::run_detail::run_live(&dashboard, runs.last().unwrap())
        })
        .await;
        slowest("the stuck run's page", || {
            super::super::run_detail::run_live(&dashboard, &stuck)
        })
        .await;
        slowest("the stuck run's trail.json", || {
            super::super::run_detail::trail_json(State(dashboard.clone()), id(&stuck))
        })
        .await;
        slowest("the stuck run's trail.tsv", || {
            super::super::run_detail::trail_tsv(State(dashboard.clone()), id(&stuck))
        })
        .await;
        slowest("the tracker's queue", || async {
            let queue = db.runs_to_follow().await.unwrap();
            assert_eq!(queue.len(), 1, "only the stuck run is still watched");
        })
        .await;
    }
}
