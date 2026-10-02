//! Join the customer's rails by ticket, entry and payment hash; shared funding is drawn once.
use super::{
    format,
    money::{self, Run},
};
use crate::trail::{payout_states, ChainTx, EntryTrace};
use maud::{html, Markup};
use time::OffsetDateTime;

fn timestamp(at: Option<i64>) -> Markup {
    html! { @if let Some(at)=at.and_then(|at|OffsetDateTime::from_unix_timestamp(at).ok()) {
        time datetime=(at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()) {
            (at.format(time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second] UTC")).unwrap_or_default())
        }
    } @else { "Not recorded" } }
}

fn refund_nodes(run: &Run, entry: &EntryTrace) -> Markup {
    let trail = run.trail;
    let refund = trail.and_then(|t| t.refund_of(entry));
    let competition = trail.and_then(|t| t.competition.as_ref());
    let pooled = competition.is_some_and(crate::trail::contracted)
        || trail.is_some_and(|t| t.funding_tx.is_some());
    if refund.is_none() && (entry.escrow.is_none() || pooled) {
        return html! {};
    }
    let trigger = competition.and_then(|c| {
        c.cancelled_at
            .map(|at| ("Competition cancelled", at))
            .or_else(|| c.failed_at.map(|at| ("Competition failed", at)))
            .or_else(|| {
                (c.kind == crate::client::competitions::CompetitionKind::Queued)
                    .then_some(c.pools_formed_at)
                    .flatten()
                    .map(|at| ("Queue closed; ticket was not assigned to a pool", at))
            })
    });
    let opens = entry.escrow.map(|e| e.refund_at);
    let paid = refund.is_some_and(|r| matches!(r.state.as_str(), "paid" | "settled"));
    let written_off = refund.is_some_and(|r| r.written_off);
    let status = if paid {
        "Paid recorded"
    } else if written_off {
        "Written off"
    } else if let Some(r) = refund {
        match r.state.as_str() {
            "minted" => "Swap prepared",
            "submitting" => "Submitting to Arkade",
            "submitted" => "Awaiting Lightning",
            _ => "Check refund state",
        }
    } else if trigger.is_none() {
        "Fallback only"
    } else if opens.is_some_and(|at| at > OffsetDateTime::now_utc().unix_timestamp()) {
        "Timelocked"
    } else if opens.is_some() {
        "Awaiting chain / worker"
    } else {
        "Opening time unknown"
    };
    html! { section.escrow-refund aria-label=(format!("Refund path for {}",entry.user)) {
        p.refund-caption { strong { "↳ Escrow refund" } span { (status) }
            @if let Some(at)=refund.and_then(|r|r.updated_at) { span { "Updated " (timestamp(Some(at))) } }
            @else { span { "Opens " (timestamp(opens)) } }
        }
        div.customer-entry-flow {
            details.money-node.refund-branch {
                (summary("Ark return",entry,trail.and_then(|t|t.swap_of(entry)).map(|s|s.amount_sat),if refund.is_some_and(|r|matches!(r.state.as_str(),"submitted"|"paid"|"settled")){"Transfer recorded"}else if refund.is_some_and(|r|r.ark_txid.is_some()){"Transaction prepared"}else{"No transfer record"}))
                div.node-detail {
                    (identity(entry))
                    p { "Trigger: " @if let Some((reason,at))=trigger { (reason) " · " (timestamp(Some(at.unix_timestamp()))) } @else { "Cancellation, failure, or a ticket left outside the formed pools. No trigger recorded." } }
                    p { "Refund opens: " (timestamp(opens)) }
                    p { "Current swap created: " (timestamp(refund.and_then(|r|r.created_at))) }
                    p { "Last state update: " (timestamp(refund.and_then(|r|r.updated_at))) }
                    (id("Refund state",refund.map(|r|r.state.as_str())))
                    (id("Ark return transaction",refund.and_then(|r|r.ark_txid.as_deref())))
                    p.note { "Cleanup retries after the trigger and escrow locktime. Confirmed chain time can lag the clock. The latest state update is not an exact Lightning settlement timestamp." }
                }
            }
            span.customer-arrow aria-hidden="true" { "→" }
            details.money-node.refund-branch {
                (summary("LN refund",entry,refund.filter(|r|r.state!="written_off").map(|r|r.paid_sats),if paid{"Paid recorded"}else if written_off{"Written off"}else if refund.is_some(){"Expected · not paid"}else{"No payment record"}))
                div.node-detail {
                    (identity(entry))
                    (id("Lightning hash",refund.and_then(|r|r.payment_hash.as_deref())))
                    (id("Destination",entry.lightning_address.as_deref()))
                    p { "Last state update: " (timestamp(refund.and_then(|r|r.updated_at))) }
                    @if paid { p.note { "The coordinator recorded payment success." } }
                    @else { p.note { "This is the expected return after fees. An Ark transfer or write-off does not prove payment to the customer." } }
                }
            }
        }
    } }
}
fn id(label: &str, value: Option<&str>) -> Markup {
    html! { p { span.note { (label) ": " } @if let Some(value)=value { code { (value) } } @else { "unknown" } } }
}
fn short(value: &str) -> String {
    format!(
        "{}…{}",
        value.chars().take(4).collect::<String>(),
        value
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    )
}
fn summary(rail: &str, entry: &EntryTrace, value: Option<u64>, status: &str) -> Markup {
    let label = entry
        .entry_id
        .map(|id| format!("entry {}", short(&id.to_string())))
        .or_else(|| {
            entry
                .ticket_id
                .map(|id| format!("ticket {}", short(&id.to_string())))
        })
        .unwrap_or_else(|| "Unassigned".into());
    html! { summary {
        strong.node-rail { (rail) } span.node-id { (label) }
        span.node-amount { @if let Some(value)=value { (format::sats(value)) " sats" } @else { "Amount unknown" } }
        span.node-status { (status) }
    } }
}
fn identity(entry: &EntryTrace) -> Markup {
    html! { strong { (entry.user) } (id("Ticket",entry.ticket_id.map(|id|id.to_string()).as_deref())) (id("Entry",entry.entry_id.map(|id|id.to_string()).as_deref())) }
}
fn transaction(tx: &ChainTx, links: &money::Links, title: &str) -> Markup {
    html! { details.transaction-record {
        summary { (title) " · " (short(&tx.txid)) }
        a href=(links.tx(&tx.txid)) { "Open in explorer ↗" }
        pre.transaction-json tabindex="0" aria-label=(format!("{title} recorded transaction JSON")) { code { (serde_json::to_string_pretty(tx).expect("chain transaction JSON")) } }
        p.note { "Recorded explorer fields. value_sat covers all outputs; raw transaction bytes are not retained here." }
    } }
}
pub(super) fn graph(run: &Run) -> Markup {
    html! {
        section { h2 { "Customer fund flow" }
            details.flow-key { summary { "Reading this flow" } p.note { "Open nodes for full IDs and evidence. Tickets join payments to escrows; entry IDs join payouts to their pool. Shared funding appears once per competition. Outgoing Lightning uses coordinator liquidity. Every hop below has routes, fees and lookups." } }
            @for scope in money::scopes(run) {
                @let scoped=scope.run(run);
                @let trail=scoped.trail;
                h3 { "Competition " (scope.competition_id.map(|id|id.to_string()).unwrap_or_else(||"not assigned".into())) }
                @if let Some(trail)=trail { p.note { "Evidence from " (trail.refreshed_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()) } }
                div.customer-flow {
                    div {
                        h3 { "Payment → escrow" }
                        @for entry in scoped.entries {
                            @let swap=trail.and_then(|t|t.swap_of(entry));
                            @let payment=entry.payment.as_ref().or_else(||trail.and_then(|t|t.late_payment(entry)));
                            article.customer-entry {
                                div.customer-entry-flow {
                                    details.money-node {
                                        (summary("LN in",entry,entry.amount_sats,if entry.settled_by_test_endpoint{"Test only"}else if payment.is_some(){"Recorded"}else{"Unknown"}))
                                        div.node-detail {
                                            (identity(entry))
                                            p { @if entry.settled_by_test_endpoint { "Test settlement · no real payment" } @else if payment.is_some() { "Payer payment evidence recorded" } @else { "Payer receipt unavailable" } }
                                            (id("Payment hash",entry.payment_hash.as_deref()))
                                        }
                                    }
                                    span.customer-arrow aria-hidden="true" { "→" }
                                    details.money-node {
                                        (summary("Ark",entry,swap.map(|s|s.amount_sat),swap.map(|s|s.state.as_str()).unwrap_or("Unknown")))
                                        div.node-detail {
                                            (identity(entry))
                                            @if let Some(swap)=swap {
                                                (id("Swap",Some(&swap.id.to_string())))
                                                (id("Ark transaction",swap.ark_txid.as_deref()))
                                                (id("VTXO",swap.escrow_vtxo.as_deref()))
                                                @if swap.state=="escrow_paid" { p.note { "Escrow funded; customer invoice settlement not yet proven." } }
                                                @if swap.state=="unsettled" { p.note { "Customer payment returned; the escrow holds swap-service funds." } }
                                                @if let Some(v)=&swap.vtxo { p.note { "Spent: " (v.spent) "; swept: " (v.swept) } (id("Spending transaction",v.spent_by.as_deref())) (id("Settled into batch",v.settled_by.as_deref())) }
                                            } @else { p.note { "No matching swap evidence" } }
                                        }
                                    }
                                }
                                (refund_nodes(&scoped,entry))
                            }
                        }
                    }
                    span.customer-arrow aria-hidden="true" { "→" }
                    div.customer-pool {
                        h3 { "Shared funding" }
                        details.money-node {
                            summary {
                                strong.node-rail { "DLC" }
                                span.node-id { "pool " (scope.competition_id.map(|id|short(&id.to_string())).unwrap_or_else(||"unassigned".into())) }
                                span.node-amount { @if let Some(s)=trail.and_then(|t|t.settlement.as_ref()) { (format::sats(s.pot_sats)) " sats" } @else { "Amount unknown" } }
                                span.node-status { @if let Some(tx)=trail.and_then(|t|t.funding_tx.as_ref()) { @if tx.confirmed { "Confirmed" } @else { "Unconfirmed" } } @else { "Unknown" } }
                            }
                            div.node-detail {
                                @if let Some(tx)=trail.and_then(|t|t.funding_tx.as_ref()) { (transaction(tx,scoped.links,"Funding")) } @else { p.note { "Funding evidence unavailable / not reached" } }
                                @if let Some(tx)=trail.and_then(|t|t.outcome_tx.as_ref()) { (transaction(tx,scoped.links,"Outcome")) }
                                @if let Some(trail)=trail { @for tx in &trail.closing_txs { (transaction(tx,scoped.links,"Recovery / closing spend")) } }
                            }
                        }
                    }
                    span.customer-arrow aria-hidden="true" { "→" }
                    div {
                        h3 { "Payout / return" }
                        @for entry in scoped.entries {
                            div.customer-entry {
                                @let matching=trail.map(|t|t.payouts.iter().zip(payout_states(&t.payouts,t.ended())).filter(|(p,_)|Some(p.entry_id)==entry.entry_id).collect::<Vec<_>>()).unwrap_or_default();
                                @if matching.is_empty() {
                                    details.money-node {
                                        (summary("LN out",entry,None,"No payout"))
                                        div.node-detail { (identity(entry)) p.note { "No payout evidence for this entry. Entitlement unknown here." } }
                                    }
                                }
                                @for (p,state) in matching {
                                    details.money-node {
                                        (summary("LN out",entry,p.amount_sats.or(Some(p.owed_sats)),&format!("{state:?}{}",if p.amount_sats.is_some() || matches!(state,crate::trail::PayoutState::Owed|crate::trail::PayoutState::OwedNothing){""}else{" · owed"})))
                                        div.node-detail {
                                            (identity(entry))
                                            p { "Contract entitlement: " (format::sats(p.owed_sats)) " sats" }
                                            (id("Lightning payment",p.payment_hash.as_deref()))
                                            (id("Destination",p.lightning_address.as_deref()))
                                            p.note { "Recipient evidence: " (format!("{:?}",p.payee)) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trail::{EscrowTerms, RefundSeen, Trail};

    #[test]
    fn refund_path_carries_times_and_keeps_ark_transfer_separate_from_payment() {
        let ticket = uuid::Uuid::now_v7();
        let entry = EntryTrace {
            user: "alice".into(),
            ticket_id: Some(ticket),
            escrow: Some(EscrowTerms {
                refund_at: 1_800_000_100,
                solo_delay_secs: 512,
            }),
            paid: true,
            ..Default::default()
        };
        let links = money::Links {
            explorer: String::new(),
            oracle: String::new(),
            coordinator: String::new(),
            ark_swap: None,
            arkd: None,
        };
        let mut trail:Trail=serde_json::from_value(serde_json::json!({
            "refreshed_at":"2026-10-02T12:00:00Z","competition_id":uuid::Uuid::now_v7(),"money":{"status":"following"}
        })).unwrap();
        let draw = |trail: &Trail| {
            refund_nodes(
                &Run {
                    competition_id: Some(trail.competition_id),
                    entries: std::slice::from_ref(&entry),
                    scenario_refunds: &[],
                    trail: Some(trail),
                    links: &links,
                    paid_by_node: Some(true),
                },
                &entry,
            )
            .into_string()
        };
        assert!(draw(&trail).contains("Fallback only"));
        let old:RefundSeen=serde_json::from_value(serde_json::json!({"user":"alice","ticket_id":ticket,"state":"submitted","paid_sats":5000,"ark_txid":"ark-return"})).unwrap();
        assert_eq!(old.updated_at, None);
        trail.refunds.push(old);
        let html = draw(&trail);
        assert!(html.contains("Transfer recorded") && html.contains("Expected · not paid"));
        assert!(!html.contains("Paid recorded"));
        trail.refunds[0].created_at = Some(1_800_000_000);
        trail.refunds[0].updated_at = Some(1_800_000_020);
        trail.refunds[0].state = "settled".into();
        let html = draw(&trail);
        assert!(
            html.contains("Paid recorded")
                && html.contains("Current swap created")
                && html.contains("datetime=")
        );
        let saved = serde_json::to_value(&trail.refunds[0]).unwrap();
        let loaded: RefundSeen = serde_json::from_value(saved).unwrap();
        assert_eq!(loaded.updated_at, Some(1_800_000_020));
        assert_eq!(loaded.created_at, Some(1_800_000_000));
        trail.refunds[0].state = "submitted".into();
        trail.refunds[0].written_off = true;
        assert!(!draw(&trail).contains("Paid recorded"));
    }
}
