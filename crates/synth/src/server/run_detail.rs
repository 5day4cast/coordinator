//! One run in detail: where each player's money went, drawn as a flow, then every hop with the
//! full id to look it up by, and a ledger checking that what went in came out. Money that is
//! stuck gets a block of its own, laid out for debugging.
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
use super::stuck;
use crate::client::competitions::CompetitionResponse;
use crate::db::{TestRun, TestStep};
use crate::scenarios::EntryBehavior;
use crate::settlement::{Decided, Settlement};
use crate::trail::tracker::entries_of;
use crate::trail::{payout_states, EntryTrace, PayoutSeen, PayoutState, Trail};

/// The all-entry outcome has positive, equal relative weights, including the
/// older 34/33/33 rounding. A ranked outcome leaves other entries unpaid.
/// This mirrors the coordinator's return classification without changing amounts.
fn is_pot_return(settlement: &Settlement) -> bool {
    match &settlement.decided {
        Some(Decided::Expired) => true,
        Some(Decided::Attested(_)) => {
            settlement.shares.len() > 1
                && settlement.shares.iter().all(|share| share.weight > 0)
                && settlement
                    .shares
                    .iter()
                    .map(|share| share.weight)
                    .max()
                    .zip(settlement.shares.iter().map(|share| share.weight).min())
                    .is_some_and(|(max, min)| max - min <= 1)
        }
        None => false,
    }
}

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
            "Contract expired: pot return follows the signed expiry terms".to_string()
        }
        (Some(Decided::Attested(index)), _) if is_pot_return(settlement) => format!(
            "No-score outcome: no entry scored any points; each entry receives its signed share of the pot (outcome {index})"
        ),
        (Some(Decided::Attested(index)), [winner]) => format!("{winner} won (outcome {index})"),
        (Some(Decided::Attested(index)), []) => format!("outcome {index} allocates no payouts"),
        (Some(Decided::Attested(index)), winners) => {
            format!("Ranked payouts to {} (outcome {index})", winners.join(", "))
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

fn deliberately_unpaid(entry: &EntryTrace, paid: bool) -> bool {
    entry.behavior == Some(EntryBehavior::AbandonUnpaid)
        && entry.ticket_id.is_some()
        && entry.payment_started == Some(false)
        && !paid
}

fn entry_line(entry: &EntryTrace, paid: bool, run_failed: bool) -> Line {
    let incomplete = if run_failed {
        Status::Failed
    } else {
        Status::Active
    };
    let expected_rejection = entry.rejected_submission.as_ref().is_some_and(|rejection| {
        rejection.status == 400
            && (rejection.message == "Competition is no longer accepting entries"
                || (entry.behavior == Some(EntryBehavior::DuplicateSubmission)
                    && rejection.message == "Ticket has already been used"))
    });
    let (value, status) = match entry.behavior {
        Some(EntryBehavior::LateSubmission) if entry.entry_submitted => {
            ("late entry unexpectedly accepted", Status::Failed)
        }
        Some(EntryBehavior::AbandonUnpaid) if paid || entry.entry_submitted => {
            ("unpaid abandonment unexpectedly advanced", Status::Failed)
        }
        Some(EntryBehavior::DuplicateSubmission) if entry.entry_submitted => {
            if expected_rejection {
                ("entered; duplicate rejected", Status::Done)
            } else if entry.rejected_submission.is_some() {
                ("entered; duplicate check failed", Status::Failed)
            } else {
                ("entered; duplicate check incomplete", incomplete)
            }
        }
        _ if entry.entry_submitted => ("entered", Status::Done),
        _ if deliberately_unpaid(entry, paid) => ("left before payment (planned)", Status::Done),
        Some(EntryBehavior::AbandonPaid) if paid => {
            ("left before submitting (planned)", Status::Done)
        }
        Some(EntryBehavior::LateSubmission) if paid => {
            if expected_rejection {
                ("late submission rejected", Status::Done)
            } else if entry.rejected_submission.is_some() {
                ("late submission check failed", Status::Failed)
            } else {
                ("waiting to submit after entry closes", incomplete)
            }
        }
        Some(_) if paid && !run_failed => ("waiting to submit", Status::Active),
        _ if paid || run_failed => ("not entered", Status::Failed),
        _ => ("not entered yet", Status::Waiting),
    };
    Line {
        label: entry.user.clone(),
        value: value.into(),
        status,
    }
}

/// Where each part of the run's money flow got to: the players' payments, their escrows and
/// entries, the funding, the contract, and how it settled. The ids are in the hops below it.
fn flow(
    entries: &[EntryTrace],
    refunds: &[(String, ScenarioRefund)],
    trail: Option<&Trail>,
    run_failed: bool,
    scenario: &str,
    failed_entries: &[&str],
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
    let expects_refund = matches!(
        scenario,
        "escrow_refund" | "paid_abandonment" | "late_submission"
    );
    let refunded_entry = |entry: &EntryTrace| {
        trail
            .and_then(|trail| trail.refund_of(entry))
            .is_some_and(|refund| refund.is_settled())
            || refunds.iter().any(|(user, _)| *user == entry.user)
    };
    let planned_unfilled = expects_refund
        && !entries.iter().any(|entry| {
            matches!(
                entry.behavior,
                Some(EntryBehavior::LateSubmission | EntryBehavior::AbandonPaid)
            ) && entry.entry_submitted
        })
        && competition.is_some_and(|competition| {
            competition.failed_at.is_none()
                && competition.contracted_at.is_none()
                && competition.signed_at.is_none()
                && competition.funding_broadcasted_at.is_none()
                && competition.funding_confirmed_at.is_none()
        });

    let payments = entries
        .iter()
        .map(|entry| {
            let payment = entry
                .payment
                .as_ref()
                .or_else(|| trail?.late_payment(entry));
            let intentional = deliberately_unpaid(entry, paid(entry));
            let unexpected = entry.behavior == Some(EntryBehavior::AbandonUnpaid) && paid(entry);
            Line {
                label: entry.user.clone(),
                value: match (entry.amount_sats, payment) {
                    _ if intentional => "no payment (planned abandonment)".to_string(),
                    (Some(sats), _) if unexpected => {
                        format!("{} sats unexpectedly paid", format::sats(sats))
                    }
                    (Some(sats), Some(payment)) if paid(entry) => format!(
                        "{} sats, {} fee",
                        format::sats(sats),
                        format::group(&format::msat_as_sats(payment.fee_msat))
                    ),
                    (Some(sats), _) if paid(entry) => format!("{} sats", format::sats(sats)),
                    (Some(sats), _) => format!("{} sats unpaid", format::sats(sats)),
                    (None, _) => "no ticket yet".to_string(),
                },
                status: if unexpected {
                    Status::Failed
                } else if paid(entry) || intentional {
                    Status::Done
                } else if !run_failed && entry.behavior.is_some() && entry.ticket_id.is_some() {
                    Status::Active
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
            let intentional = deliberately_unpaid(entry, paid(entry));
            let refunded = paid(entry) && refunded_entry(entry);
            let awaiting_refund = paid(entry) && planned_unfilled;
            Line {
                label: entry.user.clone(),
                value: match swap {
                    _ if intentional => "not funded (planned abandonment)".to_string(),
                    _ if refunded => "escrow refunded".to_string(),
                    _ if awaiting_refund => if ended {
                        "awaiting escrow refund"
                    } else {
                        "held for planned cancellation"
                    }
                    .to_string(),
                    Some(swap) if swap.funded_without_vtxo() => {
                        format!(
                            "{} sats, swap {}, no output",
                            format::sats(swap.amount_sat),
                            swap.state
                        )
                    }
                    Some(swap) => {
                        format!(
                            "{} sats, swap {}",
                            format::sats(swap.amount_sat),
                            swap.state
                        )
                    }
                    None if paid(entry) => "paid in".to_string(),
                    None => "-".to_string(),
                },
                status: match (paid(entry), escrows_confirmed) {
                    _ if intentional || refunded => Status::Done,
                    _ if awaiting_refund => {
                        if (run_failed && ended)
                            || trail.is_some_and(|trail| {
                                matches!(trail.money, crate::trail::Money::Stuck { .. })
                            })
                        {
                            Status::Failed
                        } else {
                            Status::Active
                        }
                    }
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
        .map(|entry| {
            let mut line = entry_line(entry, paid(entry), run_failed);
            // A recorded rejection is evidence from one request. The actor may still
            // fail its count check or later retry, so preserve the actual step result.
            if failed_entries.contains(&entry.user.as_str())
                && matches!(
                    entry.behavior,
                    Some(
                        EntryBehavior::DuplicateSubmission
                            | EntryBehavior::LateSubmission
                            | EntryBehavior::AbandonUnpaid
                            | EntryBehavior::AbandonPaid
                    )
                )
                && line.status != Status::Failed
            {
                line.status = Status::Failed;
                line.value.push_str("; entry check failed");
            }
            line
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
    } else if planned_unfilled {
        Status::Waiting
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
        subtitle: if planned_unfilled {
            "not required: this scenario leaves the competition unfilled"
        } else {
            "one transaction funds the contract on-chain"
        }
        .to_string(),
        status: funding_status,
        lines: funding
            .map(|funding| Line {
                label: "on-chain".to_string(),
                value: match funding.fee_sat {
                    Some(fee) => format!("{} sats batch fee", format::sats(fee)),
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
    let contract_decided = attested
        || competition.expiry_broadcasted_at.is_some()
        || trail
            .and_then(|trail| trail.settlement.as_ref())
            .is_some_and(|settlement| settlement.decided.is_some());
    let contract_status = if contract_decided {
        Status::Done
    } else if planned_unfilled {
        Status::Waiting
    } else if ended {
        Status::Failed
    } else if competition.contracted_at.is_some() {
        Status::Active
    } else {
        Status::Waiting
    };
    boxes.push(FlowBox {
        title: "Contract",
        subtitle: if planned_unfilled {
            "not created: this scenario tests cancellation and escrow refunds"
        } else {
            "signed; the oracle's attestation or contract expiry determines its outcome"
        }
        .to_string(),
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
    boxes.push(
        if (ended || refunded || planned_unfilled) && !contract_decided {
            refunds_box(entries, refunds, trail, unfinished)
        } else {
            payouts_box(trail)
        },
    );
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
                (Some(refund), _) if refund.is_settled() => (
                    format!("{} sats back", format::sats(refund.paid_sats)),
                    Status::Done,
                ),
                (_, Some((_, refund))) => (
                    format!("{} sats back", format::sats(refund.paid_sats)),
                    Status::Done,
                ),
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
        "Escrow refunds",
        "refunds of entry funds held in each player's escrow",
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
            value: {
                let owed = format::sats(payout.owed_sats);
                match state {
                    PayoutState::OwedNothing => "owed nothing".to_string(),
                    PayoutState::Paid => format!("{owed} sats paid"),
                    PayoutState::SentUnconfirmed => {
                        format!("{owed} sats sent; settlement unverified")
                    }
                    PayoutState::OtherNode => format!("{owed} sats paid to another node"),
                    PayoutState::NeverSent => format!("{owed} sats never sent"),
                    PayoutState::Owed => format!("{owed} sats owed"),
                }
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
        title: if settlement.is_some_and(is_pot_return) {
            "Pot return"
        } else {
            "Payouts"
        },
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
        .live_run(&view.run.id)
        .and_then(|live| live.current_step);
    let pick = match view.competition_id {
        Some(id) => crate::picker::store::pick_for(state.runner.db(), id).await,
        None => None,
    };
    let run = view.money();
    let rows = money::rows(&run);
    Some(html! {
        (run_section(&view.run, view.trail.as_ref(), live_step.as_deref(), now))
        (money_section(&view, now))
        @for scope in money::scopes(&run) {
            @if let Some(stuck) = stuck::block(&scope.run(&run), now) { (stuck) }
        }
        (super::funds_flow::graph(&run))
        details { summary { "Individual transfer diagrams" } (transfer_diagrams(&rows)) }
        (hops_section(&view, &rows))
        (ledger_sections(&run))
        (competition_section(&view, now))
        @if let Some(pick) = &pick { (crate::picker::view::section(pick)) }
        (steps_section(&view.steps))
        p.note { "Updated " (format::time(now, now)) }
    })
}

fn ledger_sections(run: &Run) -> Markup {
    let scopes = money::scopes(run);
    html! {
        @for scope in &scopes {
            section {
                h2 { "Ledger" @if scopes.len() > 1 { " · " (scope.competition_id.map(|id| id.to_string()).unwrap_or_default()) } }
                (ledger_table(&money::ledger(&scope.run(run)), scope.trail))
            }
        }
    }
}

fn run_section(
    run: &TestRun,
    trail: Option<&Trail>,
    live_step: Option<&str>,
    now: OffsetDateTime,
) -> Markup {
    let money = trail.map(|trail| trail.money.label());
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
        h1 { (run.scenario) " " (routes::run_status(&run.status, money)) }
        @if run.status == "passed" {
            @match money {
                Some("stuck") => p.note { "Its steps passed, but its money is stuck: see where it is held below." },
                Some("unverified" | "timed_out") => p.note { "Its steps passed, but settlement is still unverified. Synth keeps checking the money: see below." },
                Some("written_off") => p.note { "Its steps passed, but an operator wrote off refunds that could not finish: see below." },
                Some("following") => p.note { "Its steps passed. Its competition is still running or paying out; synth follows the money until the payouts or refunds are confirmed." },
                _ => {},
            }
        }
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
    let failed_entries: Vec<_> = view
        .steps
        .iter()
        .filter(|step| step.status == "failed")
        .filter_map(|step| step.step_name.strip_prefix("user_")?.strip_suffix("_enter"))
        .collect();
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
            @for scope in money::scopes(&view.money()) {
                @if trail.is_some_and(|trail| !trail.pools.is_empty()) {
                    h3 {
                        (scope.competition_id.map(|id| id.to_string()).unwrap_or_default())
                        @if let Some(evidence) = scope.trail {
                            " · " span class=(format!("badge {}", evidence.money.label())) { (evidence.money.words()) }
                        }
                    }
                }
                (flow_diagram(&flow(&scope.entries, &scope.scenario_refunds, scope.trail, view.run.status == "failed", &view.run.scenario, &failed_entries)))
                @if let Some(evidence) = scope.trail.filter(|trail| !trail.pools.is_empty() || trail.competition.as_ref().is_some_and(|competition| competition.parent_id.is_some())) {
                    @if let Some(reason) = evidence.money.reason() { p.note { (reason) } }
                    @for gap in &evidence.gaps { p.note { (gap) } }
                }
            }
        }
    }
}

/// Every arrow is backed by a recorded hop; identifiers never imply an unobserved transfer.
fn transfer_diagrams(rows: &[Row]) -> Markup {
    let transfers: Vec<_> = rows
        .iter()
        .filter(|row| !row.from.is_empty() && !row.to.is_empty())
        .collect();
    html! {
        section {
            h2 { "Transfer diagrams" }
            p.note { "Each arrow follows one recorded transfer. Amounts and fees remain unknown when the tracker has no evidence. Expand a transfer for its identifiers." }
            @for row in transfers {
                details.transfer {
                    summary { (row.step) " · " (row.status.class()) " · "
                        @if let Some(amount) = row.amount_sats { (format::sats(amount)) " sats" } @else { "amount unknown" }
                    }
                    div.transfer-flow {
                        div.transfer-node { span.note { "From" } (endpoint(&row.from)) }
                        div.transfer-arrow aria-hidden="true" { "→" }
                        div.transfer-node {
                            strong { (row.step) }
                            p { span class=(format!("badge {}", row.status.class())) { (row.status.class()) } }
                            p { "Amount: " @if let Some(amount) = row.amount_sats { (format::sats(amount)) " sats" } @else { "unknown" } }
                            p { "Fee: " @if let Some(fee) = &row.fee_sats { (format::group(fee)) " sats" } @else { "unknown" } }
                            (format::copyable_short(&row.id))
                            @if let Some(link) = &row.link { p { a href=(link) rel="noreferrer" { "Inspect evidence" } } }
                        }
                        div.transfer-arrow aria-hidden="true" { "→" }
                        div.transfer-node { span.note { "To" } (endpoint(&row.to)) }
                    }
                    @if let Some(note) = &row.note { p.note { (note) } }
                }
            }
        }
    }
}

fn hops_section(view: &RunView, rows: &[Row]) -> Markup {
    let run_id = &view.run.id;
    html! {
        section {
            h2 { "Every hop" }
            // The money columns come first, so a long id never pushes them out of view; on a
            // phone each hop stacks into a card of its own.
            div.scroll { table.trail.stack {
                thead { tr {
                    th { "Status" } th { "Step" } th.num { "Sats" } th.num { "Fee" } th { "From → to" }
                    th { "ID" } th { "Look it up" }
                } }
                tbody {
                    @for row in rows {
                        tr {
                            td data-label="Status" { span class=(format!("badge {}", row.status.class())) { (row.status.class()) } }
                            td data-label="Step" { (row.step) }
                            td.num data-label="Sats" { @if let Some(sats) = row.amount_sats { (format::sats(sats)) } @else { "-" } }
                            td.num data-label="Fee" { (row.fee_sats.as_deref().map_or("-".to_string(), format::group)) }
                            td data-label="From → to" { (endpoint(&row.from)) " → " (endpoint(&row.to)) }
                            td data-label="ID" {
                                (format::copyable_short(&row.id))
                                @if let Some(preimage) = &row.preimage {
                                    br; span.note { "preimage " } (format::copyable_short(preimage))
                                }
                            }
                            td data-label="Look it up" {
                                @if let Some(link) = &row.link {
                                    a href=(link) rel="noreferrer" title=(link) { (format::link_text(link)) }
                                }
                                @for lookup in &row.lookups {
                                    div.lookup {
                                        @if let Some(on) = &lookup.on { span.note { "on " (on) } br; }
                                        (format::copyable_command(&lookup.command))
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

/// One end of a hop. A long bare address or id, such as an escrow's Arkade address, is shortened
/// with a button to copy it whole; names, Lightning Addresses and phrases are shown as they are.
fn endpoint(end: &str) -> Markup {
    let bare = !end.contains(char::is_whitespace) && !end.contains('@');
    if bare && end.chars().count() > 40 {
        format::copyable_short(end)
    } else {
        html! { (end) }
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
                Some(id) => div.scroll {
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
                },
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
    let sats = |value: Option<u64>| value.map_or("-".to_string(), format::sats);
    let msat = |value: Option<u64>| {
        value.map_or("not visible".to_string(), |msat| {
            format::group(&format::msat_as_sats(msat))
        })
    };
    let assessed = trail.is_some_and(|trail| trail.money.is_assessed());
    let pot_return = trail
        .and_then(|trail| trail.settlement.as_ref())
        .is_some_and(is_pot_return);
    html! {
        @if pot_return {
            p.note {
                "Pot returns follow the signed allocation. Ticket charges outside the pot are "
                "not included in the return; entry routing fees are shown separately."
            }
        }
        div.scroll { table.ledger {
            thead { tr { th { "" } th.num { "Sats" } th { "" } } }
            tbody {
                tr.total {
                    td { "Players paid" }
                    td.num { (format::sats(ledger.paid_in)) }
                    td.note { (ledger.entries_paid) " entries" }
                }
                tr {
                    td { "→ into their escrows" }
                    td.num { (sats(ledger.escrowed)) }
                    td.note { "what ark-swapd put in them; the rest is its fee" }
                }
                tr { td { "→ into the pot (the contract)" } td.num { (sats(ledger.pot)) } td {} }
                tr { td { "→ ticket charges outside the pot" } td.num { (sats(ledger.coordinator_fee)) } td.note { "what players paid beyond the pot, including network and entry swap fees" } }
                @if ledger.pot.is_some() {
                    tr.total { td { "Pot" } td.num { (sats(ledger.pot)) } td {} }
                    tr { td { @if pot_return { "→ allocated for pot return" } @else { "→ owed to winners" } } td.num { (format::sats(ledger.owed)) } td.note { "their shares under the outcome" } }
                    @if ledger.others_entries > 0 {
                        tr { td { "→ owed to other players" } td.num { (format::sats(ledger.owed_others)) } td.note { (ledger.others_entries) " entries synth's players did not make; their payouts are theirs to follow" } }
                    }
                    tr { td { "→ confirmed paid over Lightning" } td.num { (format::sats(ledger.paid_out)) } td.note { (ledger.confirmed_payouts) " payouts" } }
                    tr class=(if assessed && ledger.unpaid > 0 { "flag" } else { "" }) {
                        td { "→ owed, not confirmed paid" } td.num { (format::sats(ledger.unpaid)) }
                        td.note { @if !assessed && ledger.unpaid > 0 { "still to come" } }
                    }
                    @if ledger.rounding > 0 {
                        tr { td { "→ owed to nobody" } td.num { (format::sats(ledger.rounding)) } td.note { "rounding the shares down leaves it in the pot" } }
                    }
                }
                @if ledger.refunded > 0 {
                    tr { td { "Escrow refunds" } td.num { (format::sats(ledger.refunded)) } td.note {
                        @if let Some(fees) = ledger.refund_fees { (format::sats(fees)) " sats kept by the refunds' swaps and payments" }
                    } }
                }
                tr.total { td { "Fees" } td {} td {} }
                tr { td { "entry routing, paid by the payer's node" } td.num { (msat(ledger.entry_routing_fee_msat)) } td {} }
                tr { td { "ark-swapd's swap fees" } td.num { @match ledger.swap_fees { Some(fees) => (format::sats_signed(fees)), None => "-" } } td.note { "included in the entry price" } }
                tr { td { "funding batch" } td.num { (sats(ledger.funding_batch_fee)) } td.note { "the whole Arkade batch's fee, shared by every output in it" } }
                tr { td { "outcome transaction" } td.num { (sats(ledger.outcome_fee)) } td.note { "taken from the contract's output; the coordinator still pays shares in full" } }
                @if ledger.closing_fees.is_some() {
                    tr { td { "delta or expiry transactions" } td.num { (sats(ledger.closing_fees)) } td {} }
                }
                tr { td { "payout routing, paid by the coordinator's node" } td.num { (msat(ledger.payout_routing_fee_msat)) } td.note { "for the " (ledger.confirmed_payouts) " confirmed payouts" } }
                tr class=(if ledger.remainder != 0 { "total flag" } else { "total" }) {
                    td { "Unaccounted for" }
                    td.num { (format::sats_signed(ledger.remainder)) }
                    td.note { @if !assessed { "judged once settlement is assessed" } }
                }
            }
        } }
        @if ledger.flags.is_empty() {
            @if assessed { p.note { "Player payments balance. Fees are shown separately; unavailable fees are not treated as zero." } }
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

    #[test]
    fn customer_graph_joins_payouts_by_entry_and_distinguishes_test_payments() {
        let mut entries = [
            entry("same-name", true, true),
            entry("same-name", true, true),
        ];
        entries[0].settled_by_test_endpoint = true;
        let mut trail = trail_of(competition(serde_json::json!({})), None);
        trail.payouts = vec![PayoutSeen {
            entry_id: entries[1].entry_id.unwrap(),
            user: "same-name".into(),
            owed_sats: 1234,
            payment_hash: Some("payout-for-second-entry".into()),
            ..Default::default()
        }];
        let links = Links {
            explorer: String::new(),
            oracle: String::new(),
            coordinator: String::new(),
            ark_swap: None,
            arkd: None,
        };
        let run = Run {
            competition_id: Some(trail.competition_id),
            entries: &entries,
            scenario_refunds: &[],
            trail: Some(&trail),
            links: &links,
            paid_by_node: Some(true),
        };
        let rendered = super::super::funds_flow::graph(&run).into_string();
        assert_eq!(rendered.matches("payout-for-second-entry").count(), 1);
        assert!(rendered.contains("Test settlement · no real payment"));
        assert!(rendered.contains("No payout evidence for this entry"));
        if let Ok(out) = std::env::var("ADMIN_PREVIEW_DIR") {
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(std::path::Path::new(&out).join("synth-customer-funds.html"),format!("<!doctype html><html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><style>{}</style></head><body><main><h1>Synthetic customer flow</h1>{rendered}</main></body></html>",include_str!("assets/synth.css"))).unwrap();
        }
    }

    #[test]
    fn transfer_diagram_preserves_unknown_amounts_and_escapes_labels() {
        let rows = vec![Row {
            step: "Funding <pending>".into(),
            status: Status::Active,
            from: "payer <script>".into(),
            to: "contract".into(),
            amount_sats: None,
            fee_sats: None,
            id: "fixture-transaction".into(),
            preimage: None,
            lookups: vec![],
            link: None,
            note: None,
        }];
        let rendered = transfer_diagrams(&rows).into_string();
        assert!(rendered.contains("payer &lt;script&gt;"));
        assert!(rendered.contains("Amount: unknown"));
        assert!(rendered.contains("Fee: unknown"));
        assert!(!rendered.contains("0 sats"));
        assert!(!rendered.contains("<script>"));
        if let Ok(out) = std::env::var("ADMIN_PREVIEW_DIR") {
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(std::path::Path::new(&out).join("synth-transfer.html"), format!("<!doctype html><html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><style>{}</style></head><body><main><h1>Transfer layout fixture</h1>{rendered}</main></body></html>", include_str!("assets/synth.css"))).unwrap();
        }
    }

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
            follow_until: None,
            pools: Vec::new(),
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
        let boxes = flow(&entries, &[], Some(&trail), false, "full_lifecycle", &[]);
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
        assert_eq!(boxes[0].lines[0].value, "1,100 sats, 1.001 fee");
    }

    /// The case that lost alice's buy-in: paid for, never entered, so the money sits in an escrow.
    #[test]
    fn a_paid_entry_that_never_went_in_shows_where_the_money_stopped() {
        let entries = [entry("alice", true, false)];
        let trail = trail_of(competition(serde_json::json!({})), None);
        let boxes = flow(&entries, &[], Some(&trail), true, "full_lifecycle", &[]);
        let entered = boxes.iter().find(|b| b.title == "Entries").unwrap();
        assert_eq!(entered.status, Status::Failed);
        assert_eq!(
            boxes[0].status,
            Status::Done,
            "the payment itself went through"
        );
    }

    #[test]
    fn deliberate_unpaid_abandonment_is_complete_without_claiming_a_payment() {
        let mut dropout = entry("alice", false, false);
        dropout.behavior = Some(EntryBehavior::AbandonUnpaid);
        dropout.payment_started = Some(false);
        dropout.payment = None;
        let boxes = flow(
            std::slice::from_ref(&dropout),
            &[],
            None,
            false,
            "abandoned_unpaid",
            &[],
        );
        assert!(boxes[..3].iter().all(|stage| stage.status == Status::Done));
        let rendered = flow_diagram(&boxes).into_string();
        assert!(rendered.contains("no payment (planned abandonment)"));
        assert!(rendered.contains("not funded (planned abandonment)"));
        assert!(!rendered.contains("1,100 sats"));

        dropout.paid = true;
        let unexpected = flow(&[dropout], &[], None, true, "abandoned_unpaid", &[]);
        assert_eq!(unexpected[0].status, Status::Failed);
        assert_eq!(unexpected[2].status, Status::Failed);
        assert!(flow_diagram(&unexpected)
            .into_string()
            .contains("1,100 sats unexpectedly paid"));
    }

    #[test]
    fn planned_paid_abandonment_tracks_refund_progress_without_failed_entry_or_contract() {
        let mut dropout = entry("alice", true, false);
        dropout.behavior = Some(EntryBehavior::AbandonPaid);
        let mut trail = trail_of(competition(serde_json::json!({})), None);
        let pending = flow(
            std::slice::from_ref(&dropout),
            &[],
            Some(&trail),
            false,
            "paid_abandonment",
            &[],
        );
        assert_eq!(pending[1].status, Status::Active);
        assert_eq!(pending[2].status, Status::Done);
        assert_eq!(pending.last().unwrap().title, "Escrow refunds");
        assert_ne!(pending.last().unwrap().status, Status::Done);
        assert!(flow_diagram(&pending)
            .into_string()
            .contains("left before submitting (planned)"));

        trail.competition.as_mut().unwrap().cancelled_at = Some(OffsetDateTime::now_utc());
        let failed = flow(
            std::slice::from_ref(&dropout),
            &[],
            Some(&trail),
            true,
            "paid_abandonment",
            &[],
        );
        assert_eq!(
            failed[1].status,
            Status::Failed,
            "an unfinished refund remains a failure"
        );
        assert_eq!(failed.last().unwrap().status, Status::Failed);
        let refunds = [(
            "alice".into(),
            ScenarioRefund {
                paid_sats: 1000,
                ark_txid: None,
            },
        )];
        let returned = flow(
            &[dropout],
            &refunds,
            Some(&trail),
            false,
            "paid_abandonment",
            &[],
        );
        assert_eq!(
            statuses(&returned),
            [
                ("Payments", Status::Done),
                ("Escrows", Status::Done),
                ("Entries", Status::Done),
                ("Funding", Status::Waiting),
                ("Contract", Status::Waiting),
                ("Escrow refunds", Status::Done),
            ]
        );
        let rendered = flow_diagram(&returned).into_string();
        assert!(rendered.contains("escrow refunded"));
        assert!(rendered.contains("1,000 sats back"));
        assert!(rendered.contains("not required: this scenario leaves the competition unfilled"));
    }

    #[test]
    fn late_entry_flow_distinguishes_wait_rejection_and_unexpected_acceptance() {
        let mut late = entry("alice", true, false);
        late.behavior = Some(EntryBehavior::LateSubmission);
        let pending = entry_line(&late, true, false);
        assert_eq!(pending.status, Status::Active);
        assert_eq!(pending.value, "waiting to submit after entry closes");
        for (status, expected) in [(400, Status::Done), (500, Status::Failed)] {
            late.rejected_submission = Some(crate::client::entries::ApiRejection {
                status,
                message: "Competition is no longer accepting entries".into(),
            });
            assert_eq!(entry_line(&late, true, false).status, expected);
        }
        late.entry_submitted = true;
        let accepted = entry_line(&late, true, true);
        assert_eq!(accepted.status, Status::Failed);
        assert_eq!(accepted.value, "late entry unexpectedly accepted");
        let funded = trail_of(
            competition(serde_json::json!({
                "contracted_at": "2026-09-23T03:02:00Z",
                "funding_confirmed_at": "2026-09-23T03:04:00Z",
            })),
            None,
        );
        let actual = flow(
            &[late],
            &[],
            Some(&funded),
            true,
            "late_submission",
            &["alice"],
        );
        assert_eq!(
            actual.last().unwrap().title,
            "Payouts",
            "follow the actual funded contract after an unexpected acceptance"
        );
    }

    #[test]
    fn timed_normal_entries_and_duplicate_checks_remain_pending_until_completed() {
        let mut trace = entry("alice", true, false);
        trace.behavior = Some(EntryBehavior::Complete);
        assert_eq!(entry_line(&trace, true, false).status, Status::Active);
        assert_eq!(entry_line(&trace, true, true).status, Status::Failed);
        trace.entry_submitted = true;
        trace.behavior = Some(EntryBehavior::DuplicateSubmission);
        assert_eq!(entry_line(&trace, true, false).status, Status::Active);
        assert_eq!(entry_line(&trace, true, true).status, Status::Failed);
        trace.rejected_submission = Some(crate::client::entries::ApiRejection {
            status: 400,
            message: "Ticket has already been used".into(),
        });
        assert_eq!(entry_line(&trace, true, false).status, Status::Done);
        let failed_retry = flow(
            std::slice::from_ref(&trace),
            &[],
            None,
            true,
            "duplicate_submission",
            &["alice"],
        );
        assert_eq!(failed_retry[2].status, Status::Failed);
        assert!(flow_diagram(&failed_retry)
            .into_string()
            .contains("entry check failed"));
        trace.rejected_submission.as_mut().unwrap().status = 500;
        assert_eq!(entry_line(&trace, true, false).status, Status::Failed);
    }

    #[test]
    fn a_run_not_traced_yet_still_shows_its_payments() {
        let entries = [entry("alice", true, true)];
        let boxes = flow(&entries, &[], None, false, "full_lifecycle", &[]);
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

    /// Historical all-entry allocations retain their signed amounts but are not
    /// presented as an ordinary tie or proof that every payment completed.
    #[test]
    fn a_no_score_outcome_preserves_historical_amounts_and_tracks_payment_separately() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-23T03:30:00Z" }));
        let split = settlement(Some(Decided::Attested(3)), [1020, 990, 990]);
        let payouts = [
            payout("alice", "a", 1020, true),
            payout("bob", "b", 990, true),
            payout("charlie", "c", 990, true),
        ];
        let paid = payouts_box(Some(&settled(completed.clone(), split.clone(), &payouts)));
        assert_eq!(paid.title, "Pot return");
        assert_eq!(paid.status, Status::Done);
        assert_eq!(
            payouts_box(Some(&settled(completed, split, &payouts[..1]))).status,
            Status::Waiting,
            "one confirmed share does not verify the other winners"
        );
        assert_eq!(
            paid.subtitle,
            "No-score outcome: no entry scored any points; each entry receives its signed share of the pot (outcome 3)"
        );
        assert_eq!(paid.lines[0].value, "1,020 sats paid");
        assert_eq!(paid.lines[1].value, "990 sats paid");
        assert_eq!(paid.lines[2].value, "990 sats paid");
    }

    #[test]
    fn multiple_ranked_winners_are_not_mislabeled_as_a_tie_or_pot_return() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-23T03:30:00Z" }));
        let payouts = [
            payout("alice", "a", 2100, true),
            payout("bob", "b", 900, true),
            payout("charlie", "c", 0, false),
        ];
        let paid = payouts_box(Some(&settled(
            completed,
            settlement(Some(Decided::Attested(0)), [2100, 900, 0]),
            &payouts,
        )));
        assert_eq!(paid.title, "Payouts");
        assert_eq!(paid.subtitle, "Ranked payouts to alice, bob (outcome 0)");
        assert_eq!(paid.status, Status::Done);
    }

    #[test]
    fn contract_expiry_awaiting_payments_shows_a_pot_return_not_escrow_refunds() {
        let expired = competition(serde_json::json!({
            "expiry_broadcasted_at": "2026-09-23T03:30:00Z",
        }));
        let payouts = [
            payout("alice", "a", 1000, false),
            payout("bob", "b", 1000, false),
            payout("charlie", "c", 1000, false),
        ];
        let trail = settled(
            expired,
            settlement(Some(Decided::Expired), [1000; 3]),
            &payouts,
        );
        let entries = [entry("alice", true, true)];
        let boxes = flow(&entries, &[], Some(&trail), false, "full_lifecycle", &[]);
        let returned = boxes.last().unwrap();
        assert_eq!(returned.title, "Pot return");
        assert_eq!(
            returned.subtitle,
            "Contract expired: pot return follows the signed expiry terms"
        );
        assert_eq!(returned.status, Status::Active);
        assert_eq!(returned.lines[0].value, "1,000 sats owed");
        assert!(!returned.subtitle.contains("paid"));
        assert!(!boxes
            .iter()
            .any(|flow_box| flow_box.title == "Escrow refunds"));
    }

    #[test]
    fn pot_return_ledger_distinguishes_allocation_ticket_charges_and_routing_fees() {
        let trail = trail_of(
            competition(serde_json::json!({})),
            Some(settlement(Some(Decided::Attested(3)), [1000; 3])),
        );
        let ledger = Ledger {
            entries_paid: 3,
            paid_in: 3300,
            pot: Some(3000),
            coordinator_fee: Some(300),
            owed: 3000,
            entry_routing_fee_msat: Some(3003),
            ..Ledger::default()
        };
        let html = ledger_table(&ledger, Some(&trail)).into_string();
        assert!(html.contains("Ticket charges outside the pot are not included in the return"));
        assert!(html.contains("including network and entry swap fees"));
        assert!(html.contains("→ allocated for pot return</td><td class=\"num\">3,000"));
        assert!(html.contains("→ ticket charges outside the pot</td><td class=\"num\">300"));
        assert!(html.contains("entry routing, paid by the payer"));
        assert!(html.contains("<td class=\"num\">3.003</td>"));
        assert!(!html.contains("→ owed to winners"));
        assert!(!html.contains("→ coordinator"));
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
                ("bob", "3,000 sats paid"),
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
        assert_eq!(paid.lines[0].value, "3,000 sats owed");
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
        assert_eq!(paid.lines[0].value, "1,020 sats never sent");
        let entries = [entry("alice", true, true)];
        assert_eq!(
            flow(&entries, &[], Some(&trail), false, "full_lifecycle", &[])
                .last()
                .unwrap()
                .title,
            "Pot return",
            "a signed all-entry outcome uses contract payouts, not escrow refunds"
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
            written_off: false,
            created_at: None,
            updated_at: None,
        });
        let boxes = flow(
            &entries,
            &refunds,
            Some(&trail),
            false,
            "escrow_refund",
            &[],
        );
        let settled = boxes.last().unwrap();
        assert_eq!(settled.title, "Escrow refunds");
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
