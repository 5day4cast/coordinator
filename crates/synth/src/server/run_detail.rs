//! One run in detail: where each player's money went, drawn as a flow, then every hop with the
//! full id to look it up by, and a ledger checking that what went in came out.
//!
//! The page is drawn from synth's own database: the run's steps, and the money trail the tracker
//! keeps for it. It asks nobody else anything, so it loads fast however slow the coordinator is.

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use maud::{html, Markup};
use time::OffsetDateTime;
use uuid::Uuid;

use super::format::{self, copyable};
use super::live;
use super::money::{self, Ledger, Links, Row, Run, ScenarioRefund, Status};
use super::routes::{self, Dashboard};
use crate::client::competitions::CompetitionResponse;
use crate::db::{TestRun, TestStep};
use crate::settlement::{Decided, Settlement};
use crate::trail::tracker::entries_of;
use crate::trail::{payout_states, EntryTrace, PayoutSeen, PayoutState, Trail};

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

#[derive(Debug, Clone)]
struct Line {
    label: String,
    value: String,
    status: Status,
}

#[derive(Debug, Clone)]
struct FlowBox {
    title: &'static str,
    subtitle: String,
    status: Status,
    lines: Vec<Line>,
}

impl FlowBox {
    /// A box holding one line per player: failed if any did, done once all are.
    fn of_lines(title: &'static str, subtitle: impl Into<String>, lines: Vec<Line>) -> Self {
        Self {
            title,
            subtitle: subtitle.into(),
            status: Status::of_all(lines.iter().map(|line| line.status)),
            lines,
        }
    }
}

fn short(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

/// Where each part of the run's money flow got to: the players' payments, their escrows and
/// entries, the funding, the contract, and how it settled. The ids are in the hops below it.
fn flow(
    entries: &[EntryTrace],
    refunds: &[(String, ScenarioRefund)],
    trail: Option<&Trail>,
    run_failed: bool,
) -> Vec<FlowBox> {
    let competition = trail.and_then(|trail| trail.competition.as_ref());
    // Once the run has failed, whatever an entry had not done it never will.
    let unfinished = if run_failed {
        Status::Failed
    } else {
        Status::Waiting
    };
    let ended = trail.is_some_and(Trail::ended);
    let paid = |entry: &EntryTrace| {
        entry.paid || trail.is_some_and(|trail| trail.late_payment(entry).is_some())
    };

    let payments = entries
        .iter()
        .map(|entry| {
            let payment = entry
                .payment
                .as_ref()
                .or_else(|| trail?.late_payment(entry));
            Line {
                label: entry.user.clone(),
                value: match (entry.amount_sats, payment) {
                    (Some(sats), Some(payment)) if paid(entry) => format!(
                        "{sats} sats, {} fee",
                        format::msat_as_sats(payment.fee_msat)
                    ),
                    (Some(sats), _) if paid(entry) => format!("{sats} sats"),
                    (Some(sats), _) => format!("{sats} sats unpaid"),
                    (None, _) => "no ticket yet".to_string(),
                },
                status: if paid(entry) {
                    Status::Done
                } else {
                    unfinished
                },
            }
        })
        .collect();

    let escrows_confirmed = competition.is_some_and(|c| c.escrow_funds_confirmed_at.is_some());
    let escrows = entries
        .iter()
        .map(|entry| {
            let swap = trail.and_then(|trail| trail.swap_of(entry));
            Line {
                label: entry.user.clone(),
                value: match swap {
                    Some(swap) if swap.funded_without_vtxo() => {
                        format!("{} sats, swap {}, no output", swap.amount_sat, swap.state)
                    }
                    Some(swap) => format!("{} sats, swap {}", swap.amount_sat, swap.state),
                    None if paid(entry) => "paid in".to_string(),
                    None => "-".to_string(),
                },
                status: match (paid(entry), escrows_confirmed) {
                    (true, true) => Status::Done,
                    (true, false) if ended => Status::Failed,
                    (true, false) => Status::Active,
                    (false, _) => unfinished,
                },
            }
        })
        .collect();

    let submitted = entries
        .iter()
        .map(|entry| Line {
            label: entry.user.clone(),
            value: if entry.entry_submitted {
                "entered".to_string()
            } else {
                "not entered".to_string()
            },
            status: if entry.entry_submitted {
                Status::Done
            } else if paid(entry) {
                // Paid for, but never entered: the money is in an escrow the run left behind.
                Status::Failed
            } else {
                unfinished
            },
        })
        .collect();

    let mut boxes = vec![
        FlowBox::of_lines(
            "Payments",
            "Lightning, from the payer's node to ark-swapd's invoice",
            payments,
        ),
        FlowBox::of_lines(
            "Escrows",
            "ark-swapd pays each ticket's Arkade escrow",
            escrows,
        ),
        FlowBox::of_lines(
            "Entries",
            "each player's entry, against their escrow",
            submitted,
        ),
    ];

    let Some(competition) = competition else {
        boxes.push(FlowBox {
            title: "Competition",
            subtitle: "not looked up yet".to_string(),
            status: Status::Waiting,
            lines: Vec::new(),
        });
        return boxes;
    };

    let funding_status = if competition.funding_confirmed_at.is_some() {
        Status::Done
    } else if ended {
        Status::Failed
    } else if competition.signed_at.is_some() || competition.funding_broadcasted_at.is_some() {
        Status::Active
    } else {
        Status::Waiting
    };
    let funding = trail.and_then(|trail| trail.funding_tx.as_ref());
    boxes.push(FlowBox {
        title: "Funding",
        subtitle: "one transaction funds the contract on-chain".to_string(),
        status: funding_status,
        lines: funding
            .map(|funding| Line {
                label: "on-chain".to_string(),
                value: match funding.fee_sat {
                    Some(fee) => format!("{fee} sats batch fee"),
                    None => "fee not known yet".to_string(),
                },
                status: funding_status,
            })
            .into_iter()
            .collect(),
    });

    let attested = competition.state.as_deref() == Some("attested")
        || competition.outcome_broadcasted_at.is_some()
        || competition.completed_at.is_some();
    let contract_status = if attested {
        Status::Done
    } else if ended {
        Status::Failed
    } else if competition.contracted_at.is_some() {
        Status::Active
    } else {
        Status::Waiting
    };
    boxes.push(FlowBox {
        title: "Contract",
        subtitle: "signed, then settled by the oracle's attestation".to_string(),
        status: contract_status,
        lines: vec![Line {
            label: "state".to_string(),
            value: competition
                .state
                .clone()
                .unwrap_or_else(|| competition.inferred_status().to_string()),
            status: contract_status,
        }],
    });

    let refunded = trail.is_some_and(|trail| !trail.refunds.is_empty()) || !refunds.is_empty();
    // A competition that ended before its outcome gives the escrows back instead of paying out.
    boxes.push(if (ended || refunded) && !attested {
        refunds_box(entries, refunds, trail, unfinished)
    } else {
        payouts_box(trail)
    });
    boxes
}

/// Each paid entry's refund: from the tracker, or the refund scenario's own steps.
fn refunds_box(
    entries: &[EntryTrace],
    refunds: &[(String, ScenarioRefund)],
    trail: Option<&Trail>,
    unfinished: Status,
) -> FlowBox {
    let lines = entries
        .iter()
        .filter(|entry| entry.paid || trail.is_some_and(|t| t.late_payment(entry).is_some()))
        .map(|entry| {
            let traced = trail.and_then(|trail| trail.refund_of(entry));
            let scenario = refunds.iter().find(|(user, _)| *user == entry.user);
            let (value, status) = match (traced, scenario) {
                (Some(refund), _) if refund.is_settled() => {
                    (format!("{} sats back", refund.paid_sats), Status::Done)
                }
                (_, Some((_, refund))) => (format!("{} sats back", refund.paid_sats), Status::Done),
                (Some(refund), None) => (format!("refund {}", refund.state), Status::Active),
                (None, None) => ("not refunded".to_string(), unfinished),
            };
            Line {
                label: entry.user.clone(),
                value,
                status,
            }
        })
        .collect();
    FlowBox::of_lines(
        "Refunds",
        "cancelled: each escrow goes back to its player",
        lines,
    )
}

/// Each player's payout: their share under the deciding outcome, and whether it was sent. A
/// player the outcome does not pay is shown, but does not hold the box back.
fn payouts_box(trail: Option<&Trail>) -> FlowBox {
    let settlement = trail.and_then(|trail| trail.settlement.as_ref());
    let payouts = trail.map_or(&[][..], |trail| &trail.payouts[..]);
    let subtitle = settlement.map_or_else(
        || "winners are paid once the outcome settles".to_string(),
        |settlement| outcome_words(settlement, payouts),
    );
    if settlement.is_none_or(|settlement| settlement.decided.is_none()) {
        return FlowBox {
            title: "Payouts",
            subtitle,
            status: Status::Waiting,
            lines: Vec::new(),
        };
    }
    let states = payout_states(payouts, trail.is_some_and(Trail::ended));
    let lines: Vec<Line> = payouts
        .iter()
        .zip(&states)
        .map(|(payout, state)| Line {
            label: payout.user.clone(),
            value: match state {
                PayoutState::OwedNothing => "owed nothing".to_string(),
                PayoutState::Paid => format!("{} sats paid", payout.owed_sats),
                PayoutState::SentUnconfirmed => {
                    format!("{} sats sent; settlement unverified", payout.owed_sats)
                }
                PayoutState::OtherNode => format!("{} sats paid to another node", payout.owed_sats),
                PayoutState::NeverSent => format!("{} sats never sent", payout.owed_sats),
                PayoutState::Owed => format!("{} sats owed", payout.owed_sats),
            },
            status: Status::from(*state),
        })
        .collect();
    let winners = settlement.map_or(0, |settlement| settlement.winners().count());
    let owed: Vec<Status> = states
        .iter()
        .filter(|state| state.is_owed())
        .map(|state| Status::from(*state))
        .collect();
    FlowBox {
        title: "Payouts",
        subtitle,
        // Until every winner has a payout record, one confirmed share does not settle the rest.
        status: if owed.contains(&Status::Failed) {
            Status::Failed
        } else if owed.len() < winners || owed.is_empty() {
            Status::Waiting
        } else {
            Status::of_all(owed)
        },
        lines,
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
                    div class=(format!("arrow {}", boxes[index - 1].status.class())) { "→" }
                }
                div class=(format!("box {}", flow_box.status.class())) {
                    div.title { (flow_box.title) }
                    div.subtitle { (flow_box.subtitle) }
                    @for line in &flow_box.lines {
                        div class=(format!("line {}", line.status.class())) {
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
    links: Links,
    paid_by_node: Option<bool>,
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
            arkd: config.arkd_url.clone(),
        };
        // A run's configuration says whether it paid entries from a node.
        let paid_by_node = run
            .config_json
            .as_deref()
            .and_then(|config| serde_json::from_str::<serde_json::Value>(config).ok())
            .map(|config| config.get("lnd").is_some_and(|lnd| !lnd.is_null()));
        Some(Self {
            run,
            steps,
            entries,
            scenario_refunds,
            competition_id,
            trail,
            links,
            paid_by_node,
        })
    }

    fn money(&self) -> Run<'_> {
        Run {
            competition_id: self.competition_id,
            entries: &self.entries,
            scenario_refunds: &self.scenario_refunds,
            trail: self.trail.as_ref(),
            links: &self.links,
            paid_by_node: self.paid_by_node,
        }
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

/// The run's money trail as JSON: where the money stands, where it was held, the ledger, and
/// every hop.
pub async fn trail_json(State(state): State<Dashboard>, Path(id): Path<String>) -> Response {
    match RunView::load(&state, &id).await {
        Some(view) => {
            let run = view.money();
            let rows = money::rows(&run);
            let body = money::json(&id, &run, &money::ledger(&run), &rows);
            export(&id, "json", "application/json", body)
        }
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
            money::tsv(&money::rows(&view.money())),
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

/// A run's live part: what its page shows, and what is pushed to it as the run and its money
/// move on. None for a run that does not exist.
pub(super) async fn run_live(state: &Dashboard, id: &str) -> Option<Markup> {
    let view = RunView::load(state, id).await?;
    let now = OffsetDateTime::now_utc();
    let live_step = state
        .runner
        .live()
        .filter(|live| live.run_id == view.run.id)
        .and_then(|live| live.current_step);
    let run = view.money();
    let rows = money::rows(&run);
    Some(html! {
        (run_section(&view.run, live_step.as_deref(), now))
        (money_section(&view, now))
        (hops_section(&view, &rows))
        section {
            h2 { "Ledger" }
            (ledger_table(&money::ledger(&run), view.trail.as_ref()))
        }
        (competition_section(&view, now))
        (steps_section(&view.steps))
        p.note { "Updated " (format::time(now, now)) }
    })
}

fn run_section(run: &TestRun, live_step: Option<&str>, now: OffsetDateTime) -> Markup {
    let took = match (
        format::parse(&run.started_at),
        run.completed_at.as_deref().and_then(format::parse),
    ) {
        (Some(started), Some(completed)) => Some(format::duration_ms(
            (completed - started).whole_milliseconds() as i64,
        )),
        _ => None,
    };
    html! {
        h1 { (run.scenario) " " span class=(format!("badge {}", run.status)) { (run.status) } }
        @if let Some(step) = live_step {
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
    }
}

fn money_section(view: &RunView, now: OffsetDateTime) -> Markup {
    let trail = view.trail.as_ref();
    html! {
        section {
            h2 { "Where the money went" }
            @match trail {
                Some(trail) => p {
                    span class=(format!("badge {}", trail.money.label())) { (trail.money.words()) }
                    @if let Some(reason) = trail.money.reason() { " " (reason) }
                    @if !trail.money.is_final() {
                        " · synth keeps looking until the winners are paid or the escrows refunded"
                    }
                    span.note { " · looked " (format::time(trail.refreshed_at, now)) }
                },
                None if view.competition_id.is_some() => p.note { "Looking the money up; this page updates when it has." },
                None => p.note { "The run made no competition, so no money moved past the payments." },
            }
            (flow_diagram(&flow(&view.entries, &view.scenario_refunds, trail, view.run.status == "failed")))
        }
    }
}

fn hops_section(view: &RunView, rows: &[Row]) -> Markup {
    let run_id = &view.run.id;
    html! {
        section {
            h2 { "Every hop" }
            div.scroll { table.trail {
                thead { tr {
                    th { "Status" } th { "Step" } th { "From → to" } th.num { "Sats" } th.num { "Fee" }
                    th { "ID" } th { "Look it up" }
                } }
                tbody {
                    @for row in rows {
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
                                @if let Some(link) = &row.link {
                                    a href=(link) rel="noreferrer" { (link) }
                                }
                                @for lookup in &row.lookups {
                                    div.lookup {
                                        @if let Some(on) = &lookup.on { span.note { "on " (on) } br; }
                                        (copyable(&lookup.command))
                                    }
                                }
                                @if let Some(note) = &row.note { div.note { (note) } }
                            }
                        }
                    }
                }
            } }
            @if let Some(trail) = view.trail.as_ref().filter(|trail| !trail.gaps.is_empty()) {
                p.note {
                    "Not looked up: "
                    @for (index, gap) in trail.gaps.iter().enumerate() {
                        @if index > 0 { "; " }
                        (gap)
                    }
                }
            }
            p {
                button.copy type="button" data-copy-url=(format!("/runs/{run_id}/trail.json")) { "Copy money trail as JSON" }
                " "
                button.copy type="button" data-copy-url=(format!("/runs/{run_id}/trail.tsv")) { "Copy as TSV" }
                " · "
                a href=(format!("/runs/{run_id}/trail.json")) { "trail.json" }
                " · "
                a href=(format!("/runs/{run_id}/trail.tsv")) { "trail.tsv" }
            }
        }
    }
}

fn competition_section(view: &RunView, now: OffsetDateTime) -> Markup {
    let trail = view.trail.as_ref();
    let competition = trail.and_then(|trail| trail.competition.as_ref());
    let coordinator = view.links.coordinator();
    html! {
        section {
            h2 { "Competition" }
            @match view.competition_id {
                None => p { "The run made no competition." },
                Some(id) => {
                    table {
                        tr { th { "Competition" } td { (copyable(&id.to_string())) } }
                        tr { th { "Pages" } td {
                            a href=(format!("{coordinator}/competitions/{id}/leaderboard")) rel="noreferrer" { "leaderboard" }
                            " · "
                            a href=(format!("{}/events/{id}", view.links.oracle.trim_end_matches('/'))) rel="noreferrer" { "oracle event" }
                            " · "
                            a href=(format!("{coordinator}/api/v1/competitions/{id}")) rel="noreferrer" { "json" }
                        } }
                        @if let Some(competition) = competition {
                            tr { th { "State" } td { (competition.state.as_deref().unwrap_or(competition.inferred_status())) } }
                            tr { th { "Entries" } td { (competition.total_paid_entries) " paid of " (competition.total_entries)
                                ", " (competition.total_paid_out_entries) " paid out" } }
                            @if let Some((settlement, trail)) = trail.and_then(|trail| Some((trail.settlement.as_ref()?, trail))) {
                                tr { th { "Outcome" } td { (outcome_words(settlement, &trail.payouts)) } }
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
    }
}

fn steps_section(steps: &[TestStep]) -> Markup {
    html! {
        section {
            h2 { "Steps" }
            div.scroll { table {
                thead { tr { th { "Step" } th { "Status" } th.num { "Took" } th { "Error / details" } } }
                tbody {
                    @for step in steps {
                        tr {
                            td { (step.step_name) }
                            td { span class=(format!("badge {}", step.status)) { (step.status) } }
                            td.num { (format::duration_ms(step.duration_ms.unwrap_or(0))) }
                            td {
                                @if let Some(error) = &step.error_message { span.error { (error) } }
                                @if let Some(details) = &step.details_json {
                                    // A saved step never changes once finished, so it stays open
                                    // across pushes.
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
    }
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
    let stopped = trail.is_some_and(|trail| {
        trail.money.is_final() || matches!(trail.money, crate::trail::Money::Stuck { .. })
    });
    html! {
        div.scroll { table.ledger {
            thead { tr { th { "" } th.num { "Sats" } th { "" } } }
            tbody {
                tr.total {
                    td { "Players paid" }
                    td.num { (ledger.paid_in) }
                    td.note { (ledger.entries_paid) " entries" }
                }
                tr {
                    td { "→ into their escrows" }
                    td.num { (sats(ledger.escrowed)) }
                    td.note { "what ark-swapd put in them; the rest is its fee" }
                }
                tr { td { "→ into the pot (the contract)" } td.num { (sats(ledger.pot)) } td {} }
                tr { td { "→ coordinator's fee" } td.num { (sats(ledger.coordinator_fee)) } td.note { "what players paid beyond the pot" } }
                @if ledger.pot.is_some() {
                    tr.total { td { "Pot" } td.num { (sats(ledger.pot)) } td {} }
                    tr { td { "→ owed to winners" } td.num { (ledger.owed) } td.note { "their shares under the outcome" } }
                    tr { td { "→ confirmed paid over Lightning" } td.num { (ledger.paid_out) } td.note { (ledger.confirmed_payouts) " payouts" } }
                    tr class=(if stopped && ledger.unpaid > 0 { "flag" } else { "" }) {
                        td { "→ owed, not confirmed paid" } td.num { (ledger.unpaid) }
                        td.note { @if !stopped && ledger.unpaid > 0 { "still to come" } }
                    }
                    @if ledger.rounding > 0 {
                        tr { td { "→ owed to nobody" } td.num { (ledger.rounding) } td.note { "rounding the shares down leaves it in the pot" } }
                    }
                }
                @if ledger.refunded > 0 {
                    tr { td { "Refunded" } td.num { (ledger.refunded) } td.note {
                        @if let Some(fees) = ledger.refund_fees { (fees) " sats kept by the refunds' swaps and payments" }
                    } }
                }
                tr.total { td { "Fees" } td {} td {} }
                tr { td { "entry routing, paid by the payer's node" } td.num { (msat(ledger.entry_routing_fee_msat)) } td {} }
                tr { td { "ark-swapd's swap fees" } td.num { @match ledger.swap_fees { Some(fees) => (fees), None => "-" } } td.note { "included in the entry price" } }
                tr { td { "funding batch" } td.num { (sats(ledger.funding_batch_fee)) } td.note { "the whole Arkade batch's fee, shared by every output in it" } }
                tr { td { "outcome transaction" } td.num { (sats(ledger.outcome_fee)) } td.note { "taken from the contract's output; the coordinator still pays shares in full" } }
                @if ledger.closing_fees.is_some() {
                    tr { td { "delta or expiry transactions" } td.num { (sats(ledger.closing_fees)) } td {} }
                }
                tr { td { "payout routing, paid by the coordinator's node" } td.num { (msat(ledger.payout_routing_fee_msat)) } td.note { "for the " (ledger.confirmed_payouts) " confirmed payouts" } }
                tr class=(if ledger.remainder != 0 { "total flag" } else { "total" }) {
                    td { "Unaccounted for" }
                    td.num { (ledger.remainder) }
                    td.note { @if !stopped { "judged once the money stops moving" } }
                }
            }
        } }
        @if ledger.flags.is_empty() {
            @if stopped { p.note { "Player payments balance. Fees are shown separately; unavailable fees are not treated as zero." } }
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
    use crate::trail::{EntryPayment, Money, RefundSeen};

    fn entry(user: &str, paid: bool, entry_submitted: bool) -> EntryTrace {
        EntryTrace {
            user: user.to_string(),
            nostr_pubkey: "00".repeat(32),
            entry_id: Some(Uuid::now_v7()),
            ticket_id: Some(Uuid::now_v7()),
            amount_sats: Some(1100),
            payment_hash: Some("ab".repeat(32)),
            payment: Some(EntryPayment {
                fee_msat: 1001,
                ..EntryPayment::default()
            }),
            paid,
            entry_submitted,
            ..EntryTrace::default()
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
            late_payments: Vec::new(),
            payouts: Vec::new(),
            refunds: Vec::new(),
            funding_tx: None,
            outcome_tx: None,
            closing_txs: Vec::new(),
            money: Money::Following,
            held: None,
            gaps: Vec::new(),
        }
    }

    fn statuses(boxes: &[FlowBox]) -> Vec<(&'static str, Status)> {
        boxes.iter().map(|b| (b.title, b.status)).collect()
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
            statuses(&boxes),
            [
                ("Payments", Status::Done),
                ("Escrows", Status::Done),
                ("Entries", Status::Done),
                ("Funding", Status::Done),
                ("Contract", Status::Active),
                ("Payouts", Status::Waiting),
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
        assert_eq!(entered.status, Status::Failed);
        assert_eq!(
            boxes[0].status,
            Status::Done,
            "the payment itself went through"
        );
    }

    #[test]
    fn a_run_not_traced_yet_still_shows_its_payments() {
        let entries = [entry("alice", true, true)];
        let boxes = flow(&entries, &[], None, false);
        assert_eq!(boxes.last().unwrap().title, "Competition");
        assert_eq!(boxes[0].status, Status::Done);
    }

    fn payout(user: &str, pubkey: &str, owed_sats: u64, sent: bool) -> PayoutSeen {
        use sha2::{Digest, Sha256};
        let preimage = Sha256::digest(user.as_bytes());
        PayoutSeen {
            user: user.to_string(),
            pubkey: pubkey.to_string(),
            weight: owed_sats * 100 / 3000,
            owed_sats,
            sent_at: sent.then(OffsetDateTime::now_utc),
            amount_sats: sent.then_some(owed_sats),
            preimage: sent.then(|| hex::encode(preimage)),
            payment_hash: sent.then(|| hex::encode(Sha256::digest(preimage))),
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

    fn settled(
        competition: CompetitionResponse,
        settlement: Settlement,
        payouts: &[PayoutSeen],
    ) -> Trail {
        let mut trail = trail_of(competition, Some(settlement));
        trail.payouts = payouts.to_vec();
        trail
    }

    /// What "3 of 3 paid out" was: a tie, paying each player their share.
    #[test]
    fn a_tie_pays_everyone_and_says_so() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-23T03:30:00Z" }));
        let split = settlement(Some(Decided::Attested(3)), [1020, 990, 990]);
        let payouts = [
            payout("alice", "a", 1020, true),
            payout("bob", "b", 990, true),
            payout("charlie", "c", 990, true),
        ];
        let paid = payouts_box(Some(&settled(completed.clone(), split.clone(), &payouts)));
        assert_eq!(paid.status, Status::Done);
        assert_eq!(
            payouts_box(Some(&settled(completed, split, &payouts[..1]))).status,
            Status::Waiting,
            "one confirmed share does not verify the other winners"
        );
        assert_eq!(
            paid.subtitle,
            "tie: split between alice, bob, charlie (outcome 3)"
        );
        assert_eq!(paid.lines[0].value, "1020 sats paid");
    }

    #[test]
    fn a_single_winner_is_paid_and_the_rest_are_owed_nothing() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-23T03:30:00Z" }));
        let payouts = [
            payout("alice", "a", 0, false),
            payout("bob", "b", 3000, true),
            payout("charlie", "c", 0, false),
        ];
        let paid = payouts_box(Some(&settled(
            completed,
            settlement(Some(Decided::Attested(1)), [0, 3000, 0]),
            &payouts,
        )));
        assert_eq!(paid.subtitle, "bob won (outcome 1)");
        assert_eq!(
            paid.status,
            Status::Done,
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
        let payouts = [payout("alice", "a", 3000, false)];
        let paid = payouts_box(Some(&settled(
            running,
            settlement(Some(Decided::Attested(0)), [3000, 0, 0]),
            &payouts,
        )));
        assert_eq!(paid.status, Status::Active);
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
        let split = settlement(Some(Decided::Attested(3)), [1020, 990, 990]);
        let payouts = [payout("alice", "a", 1020, false)];
        let trail = settled(failed, split, &payouts);
        let paid = payouts_box(Some(&trail));
        assert_eq!(paid.status, Status::Failed);
        assert_eq!(paid.lines[0].value, "1020 sats never sent");
        let entries = [entry("alice", true, true)];
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
            invoice: None,
            payment_hash: None,
            preimage: None,
            fee_msat: None,
            paid_by: None,
        });
        let boxes = flow(&entries, &refunds, Some(&trail), false);
        let settled = boxes.last().unwrap();
        assert_eq!(settled.title, "Refunds");
        assert_eq!(
            settled.status,
            Status::Active,
            "bob's refund is still to come"
        );
        assert_eq!(
            settled.lines.iter().map(|l| l.status).collect::<Vec<_>>(),
            [Status::Done, Status::Active]
        );
        assert_eq!(short_id("01a0d0f5-0b58"), "01a0d0f5…");
    }
}
