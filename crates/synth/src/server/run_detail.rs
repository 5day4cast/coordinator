//! One run in detail: where each player's money went, drawn as a flow, and every id needed to
//! follow it through the payer's node, ark-swapd, and the coordinator.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use super::routes::DASHBOARD_CSS;
use crate::client::competitions::CompetitionResponse;
use crate::db::TestStep;
use crate::runner::Runner;

/// An entry's trace, as a scenario recorded it in its step.
#[derive(Debug, Clone, Deserialize)]
struct EntryView {
    user: String,
    nostr_pubkey: String,
    entry_id: Option<Uuid>,
    ticket_id: Option<Uuid>,
    amount_sats: Option<u64>,
    payment_hash: Option<String>,
    #[serde(default)]
    paid: bool,
    #[serde(default)]
    entry_submitted: bool,
}

/// A settled refund, as the refund scenario recorded it.
#[derive(Debug, Clone, Deserialize)]
struct RefundView {
    paid_sats: u64,
    ark_txid: Option<String>,
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
    subtitle: &'static str,
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
/// entries, the Arkade funding, the contract, and how it settled.
fn flow(
    entries: &[EntryView],
    refunds: &[(String, RefundView)],
    competition: Option<&CompetitionResponse>,
    run_failed: bool,
) -> Vec<FlowBox> {
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
            value: match (entry.amount_sats, &entry.payment_hash) {
                (Some(sats), Some(hash)) => format!("{sats} sats · hash {}", short(hash)),
                _ => "no ticket yet".to_string(),
            },
            stage: if entry.paid { Stage::Done } else { unfinished },
        })
        .collect();

    let escrows_confirmed = competition.is_some_and(|c| reached(c.escrow_funds_confirmed_at));
    let escrows: Vec<Line> = entries
        .iter()
        .map(|entry| Line {
            label: entry.user.clone(),
            value: entry
                .ticket_id
                .map(|ticket| format!("ticket {}", short(&ticket.to_string())))
                .unwrap_or_else(|| "-".to_string()),
            stage: match (entry.paid, escrows_confirmed) {
                (true, true) => Stage::Done,
                (true, false) if ended => Stage::Failed,
                (true, false) => Stage::Active,
                (false, _) => unfinished,
            },
        })
        .collect();

    let submitted: Vec<Line> = entries
        .iter()
        .map(|entry| Line {
            label: entry.user.clone(),
            value: entry
                .entry_id
                .map(|id| format!("entry {}", short(&id.to_string())))
                .unwrap_or_else(|| "-".to_string()),
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
            subtitle: "Lightning, from the payer's node to the invoice's",
            stage: Stage::of_lines(&payments),
            lines: payments,
        },
        FlowBox {
            title: "Escrows",
            subtitle: "ark-swapd pays each ticket's Arkade escrow",
            stage: Stage::of_lines(&escrows),
            lines: escrows,
        },
        FlowBox {
            title: "Entries",
            subtitle: "each player's entry, against their escrow",
            stage: Stage::of_lines(&submitted),
            lines: submitted,
        },
    ];

    let Some(competition) = competition else {
        boxes.push(FlowBox {
            title: "Competition",
            subtitle: "the coordinator did not answer for it",
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
    boxes.push(FlowBox {
        title: "Funding",
        subtitle: "one Arkade transaction funds the contract",
        stage: funding_stage,
        lines: competition
            .funding_outpoint
            .iter()
            .map(|outpoint| Line {
                label: "outpoint".to_string(),
                value: short(outpoint),
                stage: funding_stage,
            })
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
        subtitle: "signed, then settled by the oracle's attestation",
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

    let settlement = if reached(competition.cancelled_at) {
        let lines: Vec<Line> = entries
            .iter()
            .filter(|entry| entry.paid)
            .map(
                |entry| match refunds.iter().find(|(user, _)| *user == entry.user) {
                    Some((_, refund)) => Line {
                        label: entry.user.clone(),
                        value: format!(
                            "{} sats back{}",
                            refund.paid_sats,
                            refund
                                .ark_txid
                                .as_deref()
                                .map(|txid| format!(" · ark tx {}", short(txid)))
                                .unwrap_or_default()
                        ),
                        stage: Stage::Done,
                    },
                    None => Line {
                        label: entry.user.clone(),
                        value: "not refunded".to_string(),
                        stage: unfinished,
                    },
                },
            )
            .collect();
        FlowBox {
            title: "Refunds",
            subtitle: "cancelled: each escrow goes back to its player",
            stage: Stage::of_lines(&lines),
            lines,
        }
    } else {
        let stage = if reached(competition.completed_at) {
            Stage::Done
        } else if reached(competition.failed_at) {
            Stage::Failed
        } else if attested {
            Stage::Active
        } else {
            Stage::Waiting
        };
        FlowBox {
            title: "Payouts",
            subtitle: "winners are paid once the outcome settles",
            stage,
            lines: vec![Line {
                label: "paid out".to_string(),
                value: format!(
                    "{} of {} entries",
                    competition.total_paid_out_entries, competition.total_entries
                ),
                stage,
            }],
        }
    };
    boxes.push(settlement);
    boxes
}

fn entries_of(steps: &[TestStep]) -> Vec<EntryView> {
    steps
        .iter()
        .filter_map(|step| step.details_json.as_deref())
        .filter_map(|details| serde_json::from_str::<EntryView>(details).ok())
        .collect()
}

fn refunds_of(steps: &[TestStep]) -> Vec<(String, RefundView)> {
    steps
        .iter()
        .filter_map(|step| {
            let user = step.step_name.strip_prefix("refund_")?;
            let refund = serde_json::from_str(step.details_json.as_deref()?).ok()?;
            Some((user.to_string(), refund))
        })
        .collect()
}

fn competition_of(steps: &[TestStep]) -> Option<Uuid> {
    steps
        .iter()
        .filter_map(|step| step.details_json.as_deref())
        .filter_map(|details| serde_json::from_str::<serde_json::Value>(details).ok())
        .find_map(|details| details.get("competition_id")?.as_str()?.parse().ok())
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

pub async fn run_detail(State(runner): State<Runner>, Path(id): Path<String>) -> impl IntoResponse {
    let Ok(Some(run)) = runner.db().get_run(&id).await else {
        return (StatusCode::NOT_FOUND, Html("No such run".to_string()));
    };
    let steps = runner.db().get_steps(&id).await.unwrap_or_default();
    let entries = entries_of(&steps);
    let refunds = refunds_of(&steps);
    let competition_id = competition_of(&steps);
    let competition = match competition_id {
        Some(id) => runner.client().get_competition(&id).await.ok(),
        None => None,
    };
    let boxes = flow(
        &entries,
        &refunds,
        competition.as_ref(),
        run.status == "failed",
    );
    let public_url = runner.client().base_url().trim_end_matches('/').to_string();

    let page = html! {
        (DOCTYPE)
        html {
            head {
                title { "Synth - run " (short(&run.id)) }
                style { (DASHBOARD_CSS) (FLOW_CSS) }
            }
            body {
                p { a href="/" { "← Dashboard" } }
                h1 { (run.scenario) " " span class=(format!("badge {}", run.status)) { (run.status) } }
                p.note { "Run " code { (run.id) } " · started " (run.started_at)
                    @if let Some(completed) = &run.completed_at { " · finished " (completed) } }
                @if let Some(error) = &run.error_message {
                    p.error { "Error: " (error) }
                }

                section {
                    h2 { "Where the money went" }
                    (flow_diagram(&boxes))
                }

                section {
                    h2 { "Competition" }
                    @match (competition_id, &competition) {
                        (None, _) => p { "The run made no competition." },
                        (Some(id), competition) => table.ids {
                            tr { th { "Competition" } td { code { (id) } " "
                                a href=(format!("{public_url}/api/v1/competitions/{id}")) { "json" } } }
                            @if let Some(competition) = competition {
                                tr { th { "State" } td { (competition.state.as_deref().unwrap_or(competition.inferred_status())) } }
                                tr { th { "Entries" } td { (competition.total_paid_entries) " paid of " (competition.total_entries)
                                    ", " (competition.total_paid_out_entries) " paid out" } }
                                tr { th { "Funding outpoint" } td { code { (competition.funding_outpoint.as_deref().unwrap_or("-")) } } }
                                @if !competition.errors.is_empty() {
                                    tr { th { "Errors" } td.error { (serde_json::to_string(&competition.errors).unwrap_or_default()) } }
                                }
                            } @else {
                                tr { th { "State" } td.error { "the coordinator did not answer for it" } }
                            }
                        },
                    }
                }

                section {
                    h2 { "Players" }
                    @if entries.is_empty() {
                        p { "No player entered." }
                    } @else {
                        table.ids {
                            thead { tr { th { "Player" } th { "Paid" } th { "Payment hash" } th { "Ticket" } th { "Entry" } } }
                            tbody {
                                @for entry in &entries {
                                    tr {
                                        td { (entry.user) br; span.note title=(entry.nostr_pubkey) { (short(&entry.nostr_pubkey)) } }
                                        td class=(if entry.paid { "passed" } else { "failed" }) {
                                            @match entry.amount_sats { Some(sats) => { (sats) " sats" }, None => "-" }
                                            @if !entry.paid { br; "unpaid" }
                                        }
                                        td { code { (entry.payment_hash.as_deref().unwrap_or("-")) } }
                                        td { code { (entry.ticket_id.map(|t| t.to_string()).unwrap_or_else(|| "-".into())) } }
                                        td class=(if entry.entry_submitted { "" } else { "failed" }) {
                                            code { (entry.entry_id.map(|e| e.to_string()).unwrap_or_else(|| "-".into())) }
                                            @if !entry.entry_submitted { br; "not entered" }
                                        }
                                    }
                                }
                            }
                        }
                        p.note { "Search the payment hash on the payer's node, the invoice's node, and in ark-swapd's swaps; the ticket in the coordinator." }
                    }
                }

                section {
                    h2 { "Steps" }
                    table {
                        thead { tr { th { "Step" } th { "Status" } th { "Duration" } th { "Error / details" } } }
                        tbody {
                            @for step in &steps {
                                tr class=(step.status) {
                                    td { (step.step_name) }
                                    td { (step.status) }
                                    td { (step.duration_ms.unwrap_or(0)) "ms" }
                                    td {
                                        @if let Some(error) = &step.error_message { span.error { (error) } }
                                        @if let Some(details) = &step.details_json {
                                            details { summary { "details" } pre { (pretty(details)) } }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    (StatusCode::OK, Html(page.into_string()))
}

fn pretty(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| json.to_string())
}

const FLOW_CSS: &str = r#"
.flow { display: flex; flex-wrap: wrap; align-items: stretch; gap: 6px; margin: 16px 0; }
.box { flex: 1 1 150px; background: #16213e; border: 2px solid #444; border-radius: 10px; padding: 10px; min-width: 150px; }
.box .title { font-weight: bold; font-size: 1.05em; }
.box .subtitle { color: #999; font-size: 0.75em; margin-bottom: 8px; }
.box.done { border-color: #00ff88; } .box.done .title { color: #00ff88; }
.box.active { border-color: #ffaa00; } .box.active .title { color: #ffaa00; }
.box.failed { border-color: #ff4444; } .box.failed .title { color: #ff4444; }
.box.waiting { border-style: dashed; opacity: 0.6; }
.arrow { align-self: center; font-size: 1.6em; color: #555; }
.arrow.done { color: #00ff88; } .arrow.active { color: #ffaa00; } .arrow.failed { color: #ff4444; }
.line { font-size: 0.8em; margin: 3px 0; display: flex; gap: 6px; align-items: baseline; flex-wrap: wrap; }
.line .label { font-weight: bold; }
.line .value { color: #bbb; word-break: break-all; }
.dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; background: #555; flex: none; }
.line.done .dot { background: #00ff88; } .line.active .dot { background: #ffaa00; }
.line.failed .dot { background: #ff4444; }
.legend { padding: 1px 6px; border-radius: 4px; border: 1px solid #555; }
.legend.done { color: #00ff88; } .legend.active { color: #ffaa00; } .legend.failed { color: #ff4444; }
table.ids code { font-size: 0.8em; word-break: break-all; }
pre { white-space: pre-wrap; font-size: 0.8em; }
a { color: #00d4ff; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(user: &str, paid: bool, entry_submitted: bool) -> EntryView {
        EntryView {
            user: user.to_string(),
            nostr_pubkey: "00".repeat(32),
            entry_id: Some(Uuid::now_v7()),
            ticket_id: Some(Uuid::now_v7()),
            amount_sats: Some(1100),
            payment_hash: Some("ab".repeat(32)),
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
        let boxes = flow(&entries, &[], Some(&competition), false);
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
    }

    /// The case that lost alice's buy-in: paid for, never entered, so the money sits in an escrow.
    #[test]
    fn a_paid_entry_that_never_went_in_shows_where_the_money_stopped() {
        let entries = [entry("alice", true, false)];
        let competition = competition(serde_json::json!({}));
        let boxes = flow(&entries, &[], Some(&competition), true);
        let entered = boxes.iter().find(|b| b.title == "Entries").unwrap();
        assert_eq!(entered.stage, Stage::Failed);
        assert_eq!(
            boxes[0].stage,
            Stage::Done,
            "the payment itself went through"
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
            RefundView {
                paid_sats: 1000,
                ark_txid: None,
            },
        )];
        let boxes = flow(&entries, &refunds, Some(&competition), false);
        let settled = boxes.last().unwrap();
        assert_eq!(settled.title, "Refunds");
        assert_eq!(
            settled.stage,
            Stage::Active,
            "bob's refund is still to come"
        );
        assert_eq!(
            settled.lines.iter().map(|l| l.stage).collect::<Vec<_>>(),
            [Stage::Done, Stage::Waiting]
        );
    }
}
