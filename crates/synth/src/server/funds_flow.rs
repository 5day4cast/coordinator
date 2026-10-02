//! Join the customer's rails by ticket, entry and payment hash; shared funding is drawn once.
use super::{
    format,
    money::{self, Run},
};
use crate::trail::{payout_states, ChainTx, EntryTrace};
use maud::{html, Markup};
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
                                @if let Some(r)=trail.and_then(|t|t.refund_of(entry)) {
                                    details.money-node.refund-branch {
                                        (summary("↳ LN refund",entry,Some(r.paid_sats),if r.written_off{"Written off"}else{&r.state}))
                                        div.node-detail {
                                            (identity(entry))
                                            (id("Ark transaction",r.ark_txid.as_deref()))
                                            (id("Lightning payment",r.payment_hash.as_deref()))
                                            @if r.written_off { p.note { "Written off · not evidence of payment" } }
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
