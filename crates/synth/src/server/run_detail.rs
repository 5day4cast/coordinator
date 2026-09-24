//! One run in detail: where each player's money went, drawn as a flow, then every hop with the
//! full id to look it up by, and a ledger checking that what went in came out.
//!
//! The page is drawn from synth's own database: the run's steps, and the money trail the tracker
//! keeps for it. It asks nobody else anything, so it loads fast however slow the coordinator is.

use std::hash::{Hash, Hasher};

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use maud::{html, Markup};
use time::OffsetDateTime;
use uuid::Uuid;

use super::format;
use super::live;
use super::money::{self, Ledger, Links, Row, ScenarioRefund};
use super::routes::{self, Dashboard};
use crate::client::competitions::CompetitionResponse;
use crate::db::{TestRun, TestStep};
use crate::settlement::{Decided, Settlement};
use crate::trail::tracker::entries_of;
use crate::trail::{EntryTrace, Money, PayoutSeen, Trail};

/// How the competition settled, in words, naming players by the scenario's names.
fn outcome_words(settlement: &Settlement, payouts: &[PayoutSeen]) -> String {
    let name = |pubkey: &str| {
        payouts
            .iter()
            .find(|payout| payout.pubkey == pubkey)
            .map_or_else(|| short(pubkey), |payout| payout.user.clone())
    };
    let winners: Vec<String> = settlement
        .winners()
        .map(|share| name(&share.pubkey))
        .collect();
    match (&settlement.decided, winners.as_slice()) {
        (None, _) => "waiting for the oracle's attestation".to_string(),
        (Some(Decided::Expired), _) => {
            "the oracle never attested: the contract expired and paid its expiry split".to_string()
        }
        (Some(Decided::Attested(index)), [winner]) => format!("{winner} won (outcome {index})"),
        (Some(Decided::Attested(index)), []) => format!("outcome {index} paid nobody"),
        (Some(Decided::Attested(index)), winners) => {
            format!(
                "tie: split between {} (outcome {index})",
                winners.join(", ")
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Done,
    Active,
    Failed,
    Waiting,
}

impl Stage {
    fn class(self) -> &'static str {
        match self {
            Stage::Done => "done",
            Stage::Active => "active",
            Stage::Failed => "failed",
            Stage::Waiting => "waiting",
        }
    }

    /// The stage of a box that holds one line per player: failed if any did, done once all are.
    fn of_lines(lines: &[Line]) -> Stage {
        if lines.iter().any(|line| line.stage == Stage::Failed) {
            Stage::Failed
        } else if !lines.is_empty() && lines.iter().all(|line| line.stage == Stage::Done) {
            Stage::Done
        } else if lines.iter().any(|line| line.stage != Stage::Waiting) {
            Stage::Active
        } else {
            Stage::Waiting
        }
    }
}

#[derive(Debug, Clone)]
struct Line {
    label: String,
    value: String,
    stage: Stage,
}

#[derive(Debug, Clone)]
struct FlowBox {
    title: &'static str,
    subtitle: String,
    stage: Stage,
    lines: Vec<Line>,
}

fn short(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

fn reached(at: Option<OffsetDateTime>) -> bool {
    at.is_some()
}

/// Where each part of the run's money flow got to: the players' payments, their escrows and
/// entries, the funding, the contract, and how it settled. The ids are in the trail below it.
fn flow(
    entries: &[EntryTrace],
    refunds: &[(String, ScenarioRefund)],
    trail: Option<&Trail>,
    run_failed: bool,
) -> Vec<FlowBox> {
    let competition = trail.and_then(|trail| trail.competition.as_ref());
    let settlement = trail.and_then(|trail| trail.settlement.as_ref());
    let payouts = trail.map_or(&[][..], |trail| &trail.payouts[..]);
    // Once the run has failed, whatever an entry had not done it never will.
    let unfinished = if run_failed {
        Stage::Failed
    } else {
        Stage::Waiting
    };
    let ended = competition.is_some_and(|c| reached(c.cancelled_at) || reached(c.failed_at));

    let payments: Vec<Line> = entries
        .iter()
        .map(|entry| Line {
            label: entry.user.clone(),
            value: match (entry.amount_sats, &entry.payment) {
                (Some(sats), Some(payment)) if entry.paid => format!(
                    "{sats} sats, {} fee",
                    format::msat_as_sats(payment.fee_msat)
                ),
                (Some(sats), _) if entry.paid => format!("{sats} sats"),
                (Some(sats), _) => format!("{sats} sats unpaid"),
                (None, _) => "no ticket yet".to_string(),
            },
            stage: if entry.paid { Stage::Done } else { unfinished },
        })
        .collect();

    let escrows_confirmed = competition.is_some_and(|c| reached(c.escrow_funds_confirmed_at));
    let escrows: Vec<Line> = entries
        .iter()
        .map(|entry| {
            let swap = trail.and_then(|trail| {
                trail
                    .swaps
                    .iter()
                    .find(|swap| Some(&swap.payment_hash) == entry.payment_hash.as_ref())
            });
            Line {
                label: entry.user.clone(),
                value: match swap {
                    Some(swap) => format!("{} sats, swap {}", swap.amount_sat, swap.state),
                    None if entry.paid => "paid in".to_string(),
                    None => "-".to_string(),
                },
                stage: match (entry.paid, escrows_confirmed) {
                    (true, true) => Stage::Done,
                    (true, false) if ended => Stage::Failed,
                    (true, false) => Stage::Active,
                    (false, _) => unfinished,
                },
            }
        })
        .collect();

    let submitted: Vec<Line> = entries
        .iter()
        .map(|entry| Line {
            label: entry.user.clone(),
            value: if entry.entry_submitted {
                "entered".to_string()
            } else {
                "not entered".to_string()
            },
            stage: if entry.entry_submitted {
                Stage::Done
            } else if entry.paid {
                // Paid for, but never entered: the money is in an escrow the run left behind.
                Stage::Failed
            } else {
                unfinished
            },
        })
        .collect();

    let mut boxes = vec![
        FlowBox {
            title: "Payments",
            subtitle: "Lightning, from the payer's node to ark-swapd's invoice".to_string(),
            stage: Stage::of_lines(&payments),
            lines: payments,
        },
        FlowBox {
            title: "Escrows",
            subtitle: "ark-swapd pays each ticket's Arkade escrow".to_string(),
            stage: Stage::of_lines(&escrows),
            lines: escrows,
        },
        FlowBox {
            title: "Entries",
            subtitle: "each player's entry, against their escrow".to_string(),
            stage: Stage::of_lines(&submitted),
            lines: submitted,
        },
    ];

    let Some(competition) = competition else {
        boxes.push(FlowBox {
            title: "Competition",
            subtitle: "not looked up yet".to_string(),
            stage: Stage::Waiting,
            lines: Vec::new(),
        });
        return boxes;
    };

    let funding_stage = if reached(competition.funding_confirmed_at) {
        Stage::Done
    } else if ended {
        Stage::Failed
    } else if reached(competition.signed_at) || reached(competition.funding_broadcasted_at) {
        Stage::Active
    } else {
        Stage::Waiting
    };
    let funding = trail.and_then(|trail| trail.funding_tx.as_ref());
    boxes.push(FlowBox {
        title: "Funding",
        subtitle: "one transaction funds the contract on-chain".to_string(),
        stage: funding_stage,
        lines: funding
            .map(|funding| Line {
                label: "on-chain".to_string(),
                value: match funding.fee_sat {
                    Some(fee) => format!("{fee} sats fee"),
                    None => "fee not known yet".to_string(),
                },
                stage: funding_stage,
            })
            .into_iter()
            .collect(),
    });

    let attested = competition.state.as_deref() == Some("attested")
        || reached(competition.outcome_broadcasted_at)
        || reached(competition.completed_at);
    let contract_stage = if attested {
        Stage::Done
    } else if ended {
        Stage::Failed
    } else if reached(competition.contracted_at) {
        Stage::Active
    } else {
        Stage::Waiting
    };
    boxes.push(FlowBox {
        title: "Contract",
        subtitle: "signed, then settled by the oracle's attestation".to_string(),
        stage: contract_stage,
        lines: vec![Line {
            label: "state".to_string(),
            value: competition
                .state
                .clone()
                .unwrap_or_else(|| competition.inferred_status().to_string()),
            stage: contract_stage,
        }],
    });

    let refunded = !trail.map_or(true, |trail| trail.refunds.is_empty()) || !refunds.is_empty();
    // A competition that ended before its outcome gives the escrows back instead of paying out.
    let settled = if (ended || refunded) && !attested {
        refunds_box(entries, refunds, trail, unfinished)
    } else {
        payouts_box(competition, settlement, payouts)
    };
    boxes.push(settled);
    boxes
}

/// Each paid entry's refund: from the tracker, or the refund scenario's own steps.
fn refunds_box(
    entries: &[EntryTrace],
    refunds: &[(String, ScenarioRefund)],
    trail: Option<&Trail>,
    unfinished: Stage,
) -> FlowBox {
    let lines: Vec<Line> = entries
        .iter()
        .filter(|entry| entry.paid)
        .map(|entry| {
            let traced = trail.and_then(|trail| {
                trail
                    .refunds
                    .iter()
                    .find(|refund| Some(refund.ticket_id) == entry.ticket_id)
            });
            let scenario = refunds.iter().find(|(user, _)| *user == entry.user);
            match (traced, scenario) {
                (Some(refund), _) if refund.state == "settled" => Line {
                    label: entry.user.clone(),
                    value: format!("{} sats back", refund.paid_sats),
                    stage: Stage::Done,
                },
                (_, Some((_, refund))) => Line {
                    label: entry.user.clone(),
                    value: format!("{} sats back", refund.paid_sats),
                    stage: Stage::Done,
                },
                (Some(refund), None) => Line {
                    label: entry.user.clone(),
                    value: format!("refund {}", refund.state),
                    stage: Stage::Active,
                },
                (None, None) => Line {
                    label: entry.user.clone(),
                    value: "not refunded".to_string(),
                    stage: unfinished,
                },
            }
        })
        .collect();
    FlowBox {
        title: "Refunds",
        subtitle: "cancelled: each escrow goes back to its player".to_string(),
        stage: Stage::of_lines(&lines),
        lines,
    }
}

/// Each player's payout: their share under the deciding outcome, and whether it was sent. A
/// player the outcome does not pay is shown, but does not hold the box back.
fn payouts_box(
    competition: &CompetitionResponse,
    settlement: Option<&Settlement>,
    payouts: &[PayoutSeen],
) -> FlowBox {
    let decided = settlement.and_then(|settlement| settlement.decided.as_ref());
    let subtitle = settlement.map_or_else(
        || "winners are paid once the outcome settles".to_string(),
        |settlement| outcome_words(settlement, payouts),
    );
    if decided.is_none() {
        return FlowBox {
            title: "Payouts",
            subtitle,
            stage: Stage::Waiting,
            lines: Vec::new(),
        };
    }
    let failed = reached(competition.failed_at) || reached(competition.cancelled_at);
    let lines: Vec<(bool, Line)> = payouts
        .iter()
        .map(|payout| {
            let winner = payout.owed_sats > 0;
            let (value, stage) = match (winner, payout.sent_at, payout.is_confirmed()) {
                (false, _, _) => ("owed nothing".to_string(), Stage::Waiting),
                (true, _, true) => (format!("{} sats paid", payout.owed_sats), Stage::Done),
                (true, Some(_), false) => (
                    format!("{} sats sent; settlement unverified", payout.owed_sats),
                    Stage::Active,
                ),
                (true, None, _) if failed => (
                    format!("{} sats never sent", payout.owed_sats),
                    Stage::Failed,
                ),
                (true, None, _) => (format!("{} sats owed", payout.owed_sats), Stage::Active),
            };
            (
                winner,
                Line {
                    label: payout.user.clone(),
                    value,
                    stage,
                },
            )
        })
        .collect();
    let winning: Vec<Line> = lines
        .iter()
        .filter(|(winner, _)| *winner)
        .map(|(_, line)| line.clone())
        .collect();
    FlowBox {
        title: "Payouts",
        subtitle,
        stage: if winning.is_empty()
            || settlement.is_some_and(|settlement| winning.len() < settlement.winners().count())
        {
            Stage::Waiting
        } else {
            Stage::of_lines(&winning)
        },
        lines: lines.into_iter().map(|(_, line)| line).collect(),
    }
}

fn refunds_of(steps: &[TestStep]) -> Vec<(String, ScenarioRefund)> {
    steps
        .iter()
        .filter_map(|step| {
            let user = step.step_name.strip_prefix("refund_")?;
            let refund = serde_json::from_str(step.details_json.as_deref()?).ok()?;
            Some((user.to_string(), refund))
        })
        .collect()
}

fn flow_diagram(boxes: &[FlowBox]) -> Markup {
    html! {
        div.flow {
            @for (index, flow_box) in boxes.iter().enumerate() {
                @if index > 0 {
                    div class=(format!("arrow {}", boxes[index - 1].stage.class())) { "→" }
                }
                div class=(format!("box {}", flow_box.stage.class())) {
                    div.title { (flow_box.title) }
                    div.subtitle { (flow_box.subtitle) }
                    @for line in &flow_box.lines {
                        div class=(format!("line {}", line.stage.class())) {
                            span.dot {}
                            span.label { (line.label) }
                            span.value { (line.value) }
                        }
                    }
                }
            }
        }
        p.note {
            span class="legend done" { "done" } " "
            span class="legend active" { "in progress" } " "
            span class="legend failed" { "failed" } " "
            span class="legend waiting" { "not reached" }
        }
    }
}

/// Everything a run's page and its exports show, from synth's database alone.
struct RunView {
    run: TestRun,
    steps: Vec<TestStep>,
    entries: Vec<EntryTrace>,
    scenario_refunds: Vec<(String, ScenarioRefund)>,
    competition_id: Option<Uuid>,
    trail: Option<Trail>,
    rows: Vec<Row>,
    ledger: Ledger,
    links: Links,
}

impl RunView {
    async fn load(state: &Dashboard, id: &str) -> Option<Self> {
        let db = state.runner.db();
        let run = db.get_run(id).await.ok()??;
        let steps = db.get_steps(id).await.unwrap_or_default();
        let entries = entries_of(&steps);
        let scenario_refunds = refunds_of(&steps);
        let competition_id = run.competition_id.as_deref().and_then(|id| id.parse().ok());
        let trail = db.get_trail(id).await.ok().flatten();
        if trail.is_none() && competition_id.is_some() {
            // Nobody has followed this run's money yet; the page updates once someone has.
            state.tracker.request(id);
        }
        let config = state.tracker.config();
        let links = Links {
            explorer: config.explorer_url.clone(),
            oracle: config.oracle_url.clone(),
            coordinator: state.runner.client().base_url().to_string(),
            ark_swap: state.tracker.ark_swap_url().map(str::to_string),
        };
        let rows = money::rows(
            competition_id,
            &entries,
            &scenario_refunds,
            trail.as_ref(),
            &links,
        );
        let ledger = money::ledger(&entries, &scenario_refunds, trail.as_ref());
        Some(Self {
            run,
            steps,
            entries,
            scenario_refunds,
            competition_id,
            trail,
            rows,
            ledger,
            links,
        })
    }

    fn json(&self) -> String {
        money::json(
            &self.run.id,
            self.competition_id,
            self.trail.as_ref().map(|trail| &trail.money),
            &self.ledger,
            &self.rows,
        )
    }
}

pub async fn run_detail(
    State(state): State<Dashboard>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(live) = run_live(&state, &id).await else {
        return (StatusCode::NOT_FOUND, Html("No such run".to_string())).into_response();
    };
    if routes::from_htmx(&headers) {
        return routes::html_by_hx_request(live);
    }
    let header = html! { p { a href="/" { "← Dashboard" } } };
    routes::html_by_hx_request(live::page(
        &format!("Synth - run {}", short(&id)),
        &live::run_topic(&id),
        header,
        live,
    ))
}

/// The run's money trail as JSON: where the money stands, the ledger, and every hop.
pub async fn trail_json(State(state): State<Dashboard>, Path(id): Path<String>) -> Response {
    match RunView::load(&state, &id).await {
        Some(view) => export(&id, "json", "application/json", view.json()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The run's money trail as tab-separated lines, a header first.
pub async fn trail_tsv(State(state): State<Dashboard>, Path(id): Path<String>) -> Response {
    match RunView::load(&state, &id).await {
        Some(view) => export(
            &id,
            "tsv",
            "text/tab-separated-values; charset=utf-8",
            money::tsv(&view.rows),
        ),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn export(id: &str, extension: &str, content_type: &'static str, body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"synth-run-{id}-money.{extension}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response()
}

/// A full id, with a button to copy it.
fn copyable(id: &str) -> Markup {
    html! {
        code.id { (id) }
        @if !id.is_empty() { button.copy type="button" data-copy=(id) title="Copy" { "copy" } }
    }
}

/// An element id that changes whenever `content` does, so htmx keeps it (open, scrolled) across
/// pushes only while it shows the same thing.
fn keyed(prefix: &str, content: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{prefix}-{:x}", hasher.finish())
}

fn money_words(money: &Money) -> &'static str {
    match money {
        Money::Following => "following",
        Money::PaidOut => "paid out",
        Money::Refunded => "refunded",
        Money::NothingPaid => "nothing paid",
        Money::Stuck { .. } => "stuck",
        Money::TimedOut { .. } => "stopped following",
    }
}

/// A run's live part: what its page shows, and what is pushed to it as the run and its money
/// move on. None for a run that does not exist.
pub(super) async fn run_live(state: &Dashboard, id: &str) -> Option<Markup> {
    let view = RunView::load(state, id).await?;
    let now = OffsetDateTime::now_utc();
    let run = &view.run;
    let trail = view.trail.as_ref();
    let boxes = flow(
        &view.entries,
        &view.scenario_refunds,
        trail,
        run.status == "failed",
    );
    let took = match (
        format::parse(&run.started_at),
        run.completed_at.as_deref().and_then(format::parse),
    ) {
        (Some(started), Some(completed)) => Some(format::duration_ms(
            (completed - started).whole_milliseconds() as i64,
        )),
        _ => None,
    };
    let json = view.json();
    let tsv = money::tsv(&view.rows);
    let competition = trail.and_then(|trail| trail.competition.as_ref());
    let live_step = state
        .runner
        .live()
        .filter(|live| live.run_id == run.id)
        .and_then(|live| live.current_step);

    Some(html! {
        h1 { (run.scenario) " " span class=(format!("badge {}", run.status)) { (run.status) } }
        @if let Some(step) = &live_step {
            p.running { "Now: " strong { (step) } }
        }
        p.note {
            "Run " (copyable(&run.id))
            br;
            "Started " (format::time_text(&run.started_at, now))
            @if let Some(completed) = &run.completed_at { " · finished " (format::time_text(completed, now)) }
            @if let Some(took) = &took { " · took " (took) }
        }
        @if let Some(error) = &run.error_message {
            p.error { "Error: " (error) }
        }

        section {
            h2 { "Where the money went" }
            @match trail {
                Some(trail) => p {
                    span class=(format!("badge {}", trail.money.label())) { (money_words(&trail.money)) }
                    @if let Some(reason) = trail.money.reason() { " " (reason) }
                    @if !trail.money.is_settled() {
                        " · synth keeps looking until the winners are paid or the escrows refunded"
                    }
                    span.note { " · looked " (format::time(trail.refreshed_at, now)) }
                },
                None if view.competition_id.is_some() => p.note { "Looking the money up; this page updates when it has." },
                None => p.note { "The run made no competition, so no money moved past the payments." },
            }
            (flow_diagram(&boxes))

            h3 { "Every hop" }
            div.scroll { table.trail {
                thead { tr {
                    th { "Status" } th { "Step" } th { "From → to" } th.num { "Sats" } th.num { "Fee" }
                    th { "ID" } th { "Look it up" }
                } }
                tbody {
                    @for row in &view.rows {
                        tr {
                            td { span class=(format!("badge {}", row.status.class())) { (row.status.class()) } }
                            td { (row.step) }
                            td { (row.from) " → " (row.to) }
                            td.num { @if let Some(sats) = row.amount_sats { (sats) } @else { "-" } }
                            td.num { (row.fee_sats.as_deref().unwrap_or("-")) }
                            td {
                                (copyable(&row.id))
                                @if let Some(preimage) = &row.preimage {
                                    br; span.note { "preimage " } (copyable(preimage))
                                }
                            }
                            td {
                                @match &row.link {
                                    Some(link) => a href=(link) rel="noreferrer" { (link) },
                                    None if row.lookup.starts_with("lncli") || row.lookup.starts_with("curl") => {
                                        code.id { (row.lookup) }
                                        button.copy type="button" data-copy=(row.lookup) title="Copy" { "copy" }
                                    },
                                    None => span.note { (row.lookup) },
                                }
                            }
                        }
                    }
                }
            } }
            @if let Some(trail) = trail.filter(|trail| !trail.gaps.is_empty()) {
                p.note {
                    "Not looked up: "
                    @for (index, gap) in trail.gaps.iter().enumerate() {
                        @if index > 0 { "; " }
                        (gap)
                    }
                }
            }
            p {
                button.copy type="button" data-copy-from="#trail-json" { "Copy money trail as JSON" }
                " "
                button.copy type="button" data-copy-from="#trail-tsv" { "Copy as TSV" }
                " · "
                a href=(format!("/runs/{}/trail.json", run.id)) { "trail.json" }
                " · "
                a href=(format!("/runs/{}/trail.tsv", run.id)) { "trail.tsv" }
            }
            details id=(keyed("export", &json)) hx-preserve {
                summary.note { "Show the money trail as text" }
                textarea #trail-json readonly rows="12" aria-label="Money trail as JSON" { (json) }
                textarea #trail-tsv readonly rows="8" aria-label="Money trail as TSV" { (tsv) }
            }
        }

        section {
            h2 { "Ledger" }
            (ledger_table(&view.ledger, trail))
        }

        section {
            h2 { "Competition" }
            @match view.competition_id {
                None => p { "The run made no competition." },
                Some(id) => {
                    table {
                        tr { th { "Competition" } td { (copyable(&id.to_string())) } }
                        tr { th { "Pages" } td {
                            a href=(format!("{}/competitions/{id}/leaderboard", view.links.coordinator.trim_end_matches('/'))) rel="noreferrer" { "leaderboard" }
                            " · "
                            a href=(format!("{}/events/{id}", view.links.oracle.trim_end_matches('/'))) rel="noreferrer" { "oracle event" }
                            " · "
                            a href=(format!("{}/api/v1/competitions/{id}", view.links.coordinator.trim_end_matches('/'))) rel="noreferrer" { "json" }
                        } }
                        @if let Some(competition) = competition {
                            tr { th { "State" } td { (competition.state.as_deref().unwrap_or(competition.inferred_status())) } }
                            tr { th { "Entries" } td { (competition.total_paid_entries) " paid of " (competition.total_entries)
                                ", " (competition.total_paid_out_entries) " paid out" } }
                            @if let Some(settlement) = trail.and_then(|trail| trail.settlement.as_ref()) {
                                tr { th { "Outcome" } td { (outcome_words(settlement, &trail.map(|t| t.payouts.clone()).unwrap_or_default())) } }
                            }
                            @for (label, at) in milestones(competition) {
                                tr { th { (label) } td { (format::time(at, now)) } }
                            }
                            @if !competition.errors.is_empty() {
                                tr { th { "Errors" } td.error { (serde_json::to_string(&competition.errors).unwrap_or_default()) } }
                            }
                        }
                    }
                }
            }
        }

        section {
            h2 { "Steps" }
            div.scroll { table {
                thead { tr { th { "Step" } th { "Status" } th.num { "Took" } th { "Error / details" } } }
                tbody {
                    @for step in &view.steps {
                        tr {
                            td { (step.step_name) }
                            td { span class=(format!("badge {}", step.status)) { (step.status) } }
                            td.num { (format::duration_ms(step.duration_ms.unwrap_or(0))) }
                            td {
                                @if let Some(error) = &step.error_message { span.error { (error) } }
                                @if let Some(details) = &step.details_json {
                                    // A saved step never changes, so it stays open across pushes.
                                    details id=(format!("step-{}", step.id)) hx-preserve {
                                        summary { "details" }
                                        pre { (pretty(details)) }
                                    }
                                }
                            }
                        }
                    }
                }
            } }
        }
        p.note { "Updated " (format::time(now, now)) }
    })
}

/// When the competition reached each stage it has reached, in order.
fn milestones(competition: &CompetitionResponse) -> Vec<(&'static str, OffsetDateTime)> {
    [
        ("Created", Some(competition.created_at)),
        ("Escrows confirmed", competition.escrow_funds_confirmed_at),
        ("Contract made", competition.contracted_at),
        ("Signed", competition.signed_at),
        ("Funding sent", competition.funding_broadcasted_at),
        ("Funding confirmed", competition.funding_confirmed_at),
        ("Awaiting the oracle", competition.awaiting_attestation_at),
        ("Outcome on-chain", competition.outcome_broadcasted_at),
        ("Delta on-chain", competition.delta_broadcasted_at),
        ("Expiry on-chain", competition.expiry_broadcasted_at),
        ("Completed", competition.completed_at),
        ("Failed", competition.failed_at),
        ("Cancelled", competition.cancelled_at),
    ]
    .into_iter()
    .filter_map(|(label, at)| Some((label, at?)))
    .collect()
}

fn ledger_table(ledger: &Ledger, trail: Option<&Trail>) -> Markup {
    let sats = |value: Option<u64>| value.map_or("-".to_string(), |v| v.to_string());
    let msat = |value: Option<u64>| value.map_or("not visible".to_string(), format::msat_as_sats);
    let settled = trail.is_some_and(|trail| trail.money.is_settled());
    html! {
        div.scroll { table.ledger {
            thead { tr { th { "" } th.num { "Sats" } th { "" } } }
            tbody {
                tr.total {
                    td { "Players paid" }
                    td.num { (ledger.paid_in) }
                    td.note { (ledger.entries_paid) " entries" }
                }
                tr { td { "→ into the pot (the contract)" } td.num { (sats(ledger.pot)) } td {} }
                tr { td { "→ coordinator's fee" } td.num { (sats(ledger.coordinator_fee)) } td.note { "what players paid beyond the pot" } }
                @if ledger.pot.is_some() {
                    tr.total { td { "Pot" } td.num { (sats(ledger.pot)) } td {} }
                    tr { td { "→ owed to winners" } td.num { (ledger.owed) } td.note { "their shares under the outcome" } }
                    tr { td { "→ paid out over Lightning" } td.num { (ledger.paid_out) } td {} }
                    tr class=(if settled && ledger.unpaid > 0 { "flag" } else { "" }) {
                        td { "→ owed, not sent" } td.num { (ledger.unpaid) }
                        td.note { @if !settled && ledger.unpaid > 0 { "still to come" } }
                    }
                    @if ledger.left_in_pot > 0 {
                        tr.flag { td { "→ owed to nobody" } td.num { (ledger.left_in_pot) } td.note { "rounding" } }
                    }
                }
                @if ledger.refunded > 0 {
                    tr { td { "Refunded" } td.num { (ledger.refunded) } td {} }
                }
                tr.total { td { "Fees and operator costs" } td {} td {} }
                tr { td { "entry routing, paid by the payer's node" } td.num { (msat(ledger.entry_routing_fee_msat)) } td {} }
                tr { td { "ark-swapd's swap fees" } td.num { (sats(ledger.swap_fees)) } td.note { "included in the entry price: price less what reached each escrow" } }
                tr { td { "funding transaction" } td.num { (sats(ledger.funding_fee)) } td.note { "paid by the Arkade server's wallet" } }
                tr { td { "outcome transaction" } td.num { (sats(ledger.outcome_fee)) } td.note { "taken from the contract's output; the coordinator still pays shares in full" } }
                tr { td { "payout routing, paid by the coordinator's node" } td.num { (msat(ledger.payout_routing_fee_msat)) } td {} }
                tr class=(if ledger.remainder != 0 { "total flag" } else { "total" }) {
                    td { "Unaccounted for" }
                    td.num { (ledger.remainder) }
                    td.note { @if !settled { "judged once the money settles" } }
                }
            }
        } }
        @if ledger.flags.is_empty() {
            @if settled { p.note { "Player payments balance. Fees and operator costs are shown separately; unavailable fees are not treated as zero." } }
        } @else {
            ul { @for flag in &ledger.flags { li.flag { (flag) } } }
        }
    }
}

fn pretty(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| json.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::routes::short_id;
    use crate::trail::{EntryPayment, RefundSeen};

    fn entry(user: &str, paid: bool, entry_submitted: bool) -> EntryTrace {
        EntryTrace {
            user: user.to_string(),
            nostr_pubkey: "00".repeat(32),
            entry_id: Some(Uuid::now_v7()),
            ticket_id: Some(Uuid::now_v7()),
            amount_sats: Some(1100),
            payment_hash: Some("ab".repeat(32)),
            invoice: None,
            payment: Some(EntryPayment {
                fee_msat: 1001,
                ..EntryPayment::default()
            }),
            paid,
            entry_submitted,
        }
    }

    fn competition(json: serde_json::Value) -> CompetitionResponse {
        let mut base = serde_json::json!({
            "id": Uuid::now_v7(),
            "created_at": "2026-09-23T03:00:00Z",
            "event_submission": {},
        });
        base.as_object_mut()
            .unwrap()
            .extend(json.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn trail_of(competition: CompetitionResponse, settlement: Option<Settlement>) -> Trail {
        Trail {
            refreshed_at: OffsetDateTime::now_utc(),
            competition_id: competition.id,
            competition: Some(competition),
            settlement,
            swaps: Vec::new(),
            payouts: Vec::new(),
            refunds: Vec::new(),
            funding_tx: None,
            outcome_tx: None,
            money: Money::Following,
            gaps: Vec::new(),
        }
    }

    fn stages(boxes: &[FlowBox]) -> Vec<(&'static str, Stage)> {
        boxes.iter().map(|b| (b.title, b.stage)).collect()
    }

    #[test]
    fn a_funded_competition_awaiting_attestation_is_done_up_to_its_contract() {
        let entries = [entry("alice", true, true), entry("bob", true, true)];
        let competition = competition(serde_json::json!({
            "escrow_funds_confirmed_at": "2026-09-23T03:01:00Z",
            "contracted_at": "2026-09-23T03:02:00Z",
            "signed_at": "2026-09-23T03:03:00Z",
            "funding_broadcasted_at": "2026-09-23T03:03:00Z",
            "funding_confirmed_at": "2026-09-23T03:04:00Z",
            "awaiting_attestation_at": "2026-09-23T03:04:00Z",
            "funding_outpoint": "803c8ce3:0",
        }));
        let trail = trail_of(competition, None);
        let boxes = flow(&entries, &[], Some(&trail), false);
        assert_eq!(
            stages(&boxes),
            [
                ("Payments", Stage::Done),
                ("Escrows", Stage::Done),
                ("Entries", Stage::Done),
                ("Funding", Stage::Done),
                ("Contract", Stage::Active),
                ("Payouts", Stage::Waiting),
            ]
        );
        assert_eq!(boxes[0].lines[0].value, "1100 sats, 1.001 fee");
    }

    /// The case that lost alice's buy-in: paid for, never entered, so the money sits in an escrow.
    #[test]
    fn a_paid_entry_that_never_went_in_shows_where_the_money_stopped() {
        let entries = [entry("alice", true, false)];
        let trail = trail_of(competition(serde_json::json!({})), None);
        let boxes = flow(&entries, &[], Some(&trail), true);
        let entered = boxes.iter().find(|b| b.title == "Entries").unwrap();
        assert_eq!(entered.stage, Stage::Failed);
        assert_eq!(
            boxes[0].stage,
            Stage::Done,
            "the payment itself went through"
        );
    }

    #[test]
    fn a_run_not_traced_yet_still_shows_its_payments() {
        let entries = [entry("alice", true, true)];
        let boxes = flow(&entries, &[], None, false);
        assert_eq!(boxes.last().unwrap().title, "Competition");
        assert_eq!(boxes[0].stage, Stage::Done);
    }

    fn payout(user: &str, pubkey: &str, owed_sats: u64, sent: bool) -> PayoutSeen {
        PayoutSeen {
            user: user.to_string(),
            pubkey: pubkey.to_string(),
            weight: owed_sats * 100 / 3000,
            owed_sats,
            sent_at: sent.then(OffsetDateTime::now_utc),
            amount_sats: sent.then_some(owed_sats),
            preimage: sent
                .then(|| "1111111111111111111111111111111111111111111111111111111111111111".into()),
            payment_hash: sent
                .then(|| "02d449a31fbb267c8f352e9968a79e3e5fc95c1bbeaa502fd6454ebde5a4bedc".into()),
            ..PayoutSeen::default()
        }
    }

    fn settlement(decided: Option<Decided>, owed: [u64; 3]) -> Settlement {
        Settlement {
            pot_sats: 3000,
            decided,
            shares: ["a", "b", "c"]
                .iter()
                .zip(owed)
                .map(|(pubkey, owed_sats)| crate::settlement::Share {
                    pubkey: pubkey.to_string(),
                    weight: owed_sats * 100 / 3000,
                    owed_sats,
                })
                .collect(),
        }
    }

    /// What "3 of 3 paid out" was: a tie, paying each player their share.
    #[test]
    fn a_tie_pays_everyone_and_says_so() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-23T03:30:00Z" }));
        let settled = settlement(Some(Decided::Attested(3)), [1020, 990, 990]);
        let payouts = [
            payout("alice", "a", 1020, true),
            payout("bob", "b", 990, true),
            payout("charlie", "c", 990, true),
        ];
        let paid = payouts_box(&completed, Some(&settled), &payouts);
        assert_eq!(paid.stage, Stage::Done);
        assert_eq!(
            paid.subtitle,
            "tie: split between alice, bob, charlie (outcome 3)"
        );
        assert_eq!(paid.lines[0].value, "1020 sats paid");
    }

    #[test]
    fn a_single_winner_is_paid_and_the_rest_are_owed_nothing() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-23T03:30:00Z" }));
        let settled = settlement(Some(Decided::Attested(1)), [0, 3000, 0]);
        let payouts = [
            payout("alice", "a", 0, false),
            payout("bob", "b", 3000, true),
            payout("charlie", "c", 0, false),
        ];
        let paid = payouts_box(&completed, Some(&settled), &payouts);
        assert_eq!(paid.subtitle, "bob won (outcome 1)");
        assert_eq!(
            paid.stage,
            Stage::Done,
            "losers owed nothing do not hold it back"
        );
        assert_eq!(
            paid.lines
                .iter()
                .map(|l| (l.label.as_str(), l.value.as_str()))
                .collect::<Vec<_>>(),
            [
                ("alice", "owed nothing"),
                ("bob", "3000 sats paid"),
                ("charlie", "owed nothing")
            ]
        );
    }

    #[test]
    fn a_winner_whose_payout_is_not_sent_yet_keeps_it_in_progress() {
        let running = competition(serde_json::json!({}));
        let settled = settlement(Some(Decided::Attested(0)), [3000, 0, 0]);
        let payouts = [payout("alice", "a", 3000, false)];
        let paid = payouts_box(&running, Some(&settled), &payouts);
        assert_eq!(paid.stage, Stage::Active);
        assert_eq!(paid.lines[0].value, "3000 sats owed");
    }

    /// Run 01a0d0f5: the outcome went on-chain, then the competition failed with nothing sent.
    #[test]
    fn winners_never_paid_before_the_competition_failed_show_as_failed() {
        let failed = competition(serde_json::json!({
            "outcome_broadcasted_at": "2026-09-24T01:36:37Z",
            "failed_at": "2026-09-24T04:16:13Z",
            "cancelled_at": "2026-09-24T05:16:13Z",
        }));
        let settled = settlement(Some(Decided::Attested(3)), [1020, 990, 990]);
        let payouts = [payout("alice", "a", 1020, false)];
        let paid = payouts_box(&failed, Some(&settled), &payouts);
        assert_eq!(paid.stage, Stage::Failed);
        assert_eq!(paid.lines[0].value, "1020 sats never sent");
        let entries = [entry("alice", true, true)];
        let trail = trail_of(failed, Some(settled));
        assert_eq!(
            flow(&entries, &[], Some(&trail), false)
                .last()
                .unwrap()
                .title,
            "Payouts",
            "a contract that reached its outcome settles by payouts, not refunds"
        );
    }

    #[test]
    fn a_cancelled_competition_settles_by_refunds() {
        let entries = [entry("alice", true, true), entry("bob", true, true)];
        let competition = competition(serde_json::json!({
            "escrow_funds_confirmed_at": "2026-09-23T03:01:00Z",
            "cancelled_at": "2026-09-23T03:30:00Z",
        }));
        let refunds = [(
            "alice".to_string(),
            ScenarioRefund {
                paid_sats: 1000,
                ark_txid: None,
            },
        )];
        let mut trail = trail_of(competition, None);
        trail.refunds.push(RefundSeen {
            user: "bob".into(),
            ticket_id: entries[1].ticket_id.unwrap(),
            state: "submitted".into(),
            paid_sats: 0,
            ark_txid: None,
        });
        let boxes = flow(&entries, &refunds, Some(&trail), false);
        let settled = boxes.last().unwrap();
        assert_eq!(settled.title, "Refunds");
        assert_eq!(
            settled.stage,
            Stage::Active,
            "bob's refund is still to come"
        );
        assert_eq!(
            settled.lines.iter().map(|l| l.stage).collect::<Vec<_>>(),
            [Stage::Done, Stage::Active]
        );
    }

    #[test]
    fn an_element_keeps_its_id_only_while_its_content_does() {
        assert_eq!(keyed("export", "a"), keyed("export", "a"));
        assert_ne!(keyed("export", "a"), keyed("export", "b"));
        assert_eq!(short_id("01a0d0f5-0b58"), "01a0d0f5…");
    }
}
