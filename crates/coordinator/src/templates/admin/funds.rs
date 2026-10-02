//! A customer's transfers and the shared pool they fund; no browser-side data fetching.
use crate::domain::{
    admin_funds::{invoice_sats, FundsPage, FundsTicket, TicketFlow},
    Competition,
};
use maud::{html, Markup};
use time::OffsetDateTime;

fn fact(label: &str, value: Option<&str>) -> Markup {
    html! { div.trace-fact { dt { (label) } dd { @if let Some(value)=value { code { (value) } } @else { "Not recorded" } } } }
}
fn amount(value: Option<u64>) -> Markup {
    html! { @if let Some(value)=value { (value) " sats" } @else { "Amount unknown" } }
}
fn identifier(value: &str) -> String {
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
fn sats(value: Option<i64>) -> Option<u64> {
    value.and_then(|v| v.try_into().ok())
}
fn payout_owed(c: &Competition, ticket: &FundsTicket) -> Option<u64> {
    let outcome = c.get_current_outcome().ok()?;
    let params = c.contract_parameters.as_ref()?;
    let key = dlctix::secp::Point::from_hex(ticket.entry_pubkey.as_deref()?).ok()?;
    let index = params.players.iter().position(|p| p.pubkey == key)?;
    let weights = params.outcome_payouts.get(&outcome)?;
    if !weights.contains_key(&index) {
        return Some(0);
    }
    crate::domain::winner_payout_sats(params, &outcome, &key).ok()
}
pub fn position(flow: &TicketFlow) -> &'static str {
    let t = &flow.ticket;
    if flow.payouts.iter().any(|p| p.succeeded_at.is_some()) {
        "Lightning payout recorded successful"
    } else if matches!(t.refund_state.as_deref(), Some("paid" | "settled")) {
        "Lightning refund recorded paid"
    } else if t.released_at.is_some() {
        "Held Lightning payment released"
    } else if t.write_off.is_some() {
        "Refund written off · support review required"
    } else if t.refund_state.is_some() {
        "Escrow refund in progress"
    } else if flow.payouts.iter().any(|p| p.failed_at.is_none()) {
        "Lightning payout pending · check sender"
    } else if t.sellback_at.is_some() || t.reclaimed_at.is_some() {
        "On-chain recovery recorded · verify destination"
    } else if t.funded_at.is_some() {
        "Escrow funded · follow pool and payout evidence"
    } else if t.paid_at.is_some() {
        "Entry payment recorded · verify its next handoff"
    } else {
        "Payment not recorded settled · inspect swap or invoice"
    }
}
fn node_summary(rail: &str, t: &FundsTicket, value: Option<u64>, status: &str) -> Markup {
    let (kind, id) = t
        .entry_id
        .as_deref()
        .map(|id| ("entry", id))
        .unwrap_or(("ticket", &t.ticket_id));
    html! { summary {
        strong.node-rail { (rail) }
        span.node-id { (kind) " " (identifier(id)) }
        span.node-amount { (amount(value)) }
        span.node-status { (status) }
    } }
}
fn identity(t: &FundsTicket) -> Markup {
    html! { dl { (fact("Competition",Some(&t.competition_id))) (fact("Entry",t.entry_id.as_deref())) (fact("Ticket",Some(&t.ticket_id))) } }
}

fn refund_trigger(c: &Competition) -> Option<(&'static str, OffsetDateTime)> {
    c.cancelled_at
        .map(|at| ("Competition cancelled", at))
        .or_else(|| c.failed_at.map(|at| ("Competition failed", at)))
        .or_else(|| {
            (c.kind == crate::domain::CompetitionKind::Queued)
                .then_some(c.pools_formed_at)
                .flatten()
                .map(|at| ("Queue closed; ticket was not assigned to a pool", at))
        })
}

fn refund_stage(c: &Competition, t: &FundsTicket, now: i64) -> &'static str {
    if matches!(t.refund_state.as_deref(), Some("paid" | "settled")) {
        return "Paid recorded";
    }
    if t.write_off.is_some() {
        return "Written off";
    }
    if t.refund_error.is_some() {
        return "Needs attention";
    }
    match t.refund_state.as_deref() {
        Some("minted") => return "Swap prepared",
        Some("submitting") => return "Submitting to Arkade",
        Some("submitted") => return "Awaiting Lightning",
        Some(_) => return "Check refund state",
        None => {}
    }
    if t.escrow_pooled {
        return "Moved into DLC";
    }
    if t.funded_at.is_none() {
        return "Escrow funding unconfirmed";
    }
    if refund_trigger(c).is_none() {
        return "Fallback only";
    }
    match t.refund_opens_at {
        Some(at) if at > now => "Timelocked",
        Some(_) => "Awaiting chain / worker",
        None => "Opening time unknown",
    }
}

fn timestamp(at: Option<i64>) -> Markup {
    let at = at.and_then(|at| OffsetDateTime::from_unix_timestamp(at).ok());
    html! { @if let Some(at)=at {
        time datetime=(at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()) {
            (at.format(time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second] UTC")).unwrap_or_default())
        }
    } @else { "Not recorded" } }
}

/// The return branches from the escrow, before the shared contract. The opening time is a
/// locktime, never a promise of a payment at that wall-clock instant.
fn refund_nodes(c: &Competition, t: &FundsTicket) -> Markup {
    let recorded = t.refund_id.is_some() || t.refund_state.is_some() || t.write_off.is_some();
    let visible = recorded || (t.escrow_address.is_some() && !t.escrow_pooled);
    let paid = matches!(t.refund_state.as_deref(), Some("paid" | "settled"));
    let returned = t.refund_invoice.as_deref().and_then(invoice_sats);
    html! {
        @if visible {
            section.escrow-refund aria-label=(format!("Refund path for ticket {}",t.ticket_id)) {
                p.refund-caption { strong { "↳ Escrow refund" } span { (refund_stage(c,t,OffsetDateTime::now_utc().unix_timestamp())) }
                    @if let Some(at)=t.refund_updated_at { span { "Updated " (timestamp(Some(at))) } }
                    @else { span { "Opens " (timestamp(t.refund_opens_at)) } }
                }
                div.entry-flow {
                    details.money-node.refund-branch {
                        (node_summary("Ark return",t,sats(t.escrow_sats),if matches!(t.refund_state.as_deref(),Some("submitted"|"paid"|"settled")){"Transfer recorded"}else if t.refund_ark_txid.is_some(){"Transaction prepared"}else{"No transfer record"}))
                        div.node-detail {
                            (identity(t))
                            dl {
                                div.trace-fact { dt { "Trigger" } dd { @if let Some((reason,at))=refund_trigger(c) { (reason) " · " (timestamp(Some(at.unix_timestamp()))) } @else { "Cancellation, failure, or a ticket left outside the formed pools. No trigger recorded." } } }
                                div.trace-fact { dt { "Refund opens" } dd { (timestamp(t.refund_opens_at)) } }
                                div.trace-fact { dt { "Current swap created" } dd { (timestamp(t.refund_created_at)) } }
                                div.trace-fact { dt { "Last state update" } dd { (timestamp(t.refund_updated_at)) } }
                                (fact("Refund state",t.refund_state.as_deref()))
                                (fact("Refund swap",t.refund_id.as_deref()))
                                (fact("Ark return transaction",t.refund_ark_txid.as_deref()))
                            }
                            p.note { "Cleanup retries after the trigger and escrow locktime. Confirmed chain time can lag the clock. These records do not retain every retry or the exact Lightning settlement time." }
                            @if let Some(error)=&t.refund_error { p.attention { (error) } }
                            @if t.write_off.is_some() { dl { (fact("Write-off reason",t.write_off.as_deref())) div.trace-fact { dt { "Written off" } dd { (timestamp(t.write_off_at)) } } } }
                            a href=(format!("/admin/funds/tickets/{}",t.ticket_id)) { "Check refund services now" }
                        }
                    }
                    span.fund-arrow aria-hidden="true" { "→" }
                    details.money-node.refund-branch {
                        (node_summary("LN refund",t,returned,if paid{"Paid recorded"}else if t.write_off.is_some(){"Written off"}else if returned.is_some(){"Invoice · not paid"}else{"No payment record"}))
                        div.node-detail {
                            (identity(t))
                            dl { (fact("Lightning hash",t.refund_hash.as_deref()))
                                div.trace-fact { dt { "Last state update" } dd { (timestamp(t.refund_updated_at)) } }
                            }
                            @if let Some(fee)=t.refund_fee_sats { p { "Refund fee: " (fee) " sats." } }
                            @if paid { p.note { "The coordinator recorded payment success. Check services for current sender evidence." } }
                            @else { p.note { "The invoice amount is what the refund intends to return after fees. A transfer to the swap or a write-off does not prove payment to the customer." } }
                        }
                    }
                }
            }
        }
        @if t.released_at.is_some() {
            details.money-node.refund-branch {
                (node_summary("↳ LN release",t,t.invoice.as_deref().and_then(invoice_sats),"Hold cancelled"))
                div.node-detail { (identity(t)) dl { (fact("Released",t.released_at.as_deref())) (fact("Original payment hash",Some(&t.payment_hash))) }
                    p.note { "The held payment was released back to the payer. This is a cancellation of the original payment." }
                }
            }
        }
    }
}
pub fn funds_graph(c: &Competition, page: &FundsPage, network: &str, explorer: &str) -> Markup {
    html! {
        section.funds-trace {
            h2 { "Where is the money?" }
            details.flow-key { summary { "Reading this flow" }
                p.note { "Open any node for its evidence. Arrows connect payment obligations and their backing. Outgoing Lightning uses coordinator liquidity; shared pool funding appears once. Amounts marked owed are contract entitlements, not payment receipts." }
            }
            @if page.tickets.is_empty() { p.notice { "No matching reserved or paid tickets in this scope. Tickets assigned to child pools are shown on those pools." } }
            @else {
                div.money-network {
                    div.money-entries {
                        h3 { "Payment → escrow" }
                        @for flow in &page.tickets {
                            (payment_nodes(c,flow,network,explorer))
                        }
                    }
                    div.money-bridge aria-hidden="true" { "→" }
                    (pool_node(c,page,explorer))
                    div.money-bridge aria-hidden="true" { "→" }
                    div.money-payouts {
                        h3 { "Payout / return" }
                        @for flow in &page.tickets {
                            (payout_nodes(c,flow))
                        }
                    }
                }
            }
            (transaction_records(c,network,explorer))

        }
    }
}
fn payment_nodes(c: &Competition, flow: &TicketFlow, network: &str, explorer: &str) -> Markup {
    let t = &flow.ticket;
    html! { article.money-entry id=(format!("ticket-{}",t.ticket_id)) {
        div.entry-flow {
            details.money-node {
                (node_summary("LN in",t,t.invoice.as_deref().and_then(invoice_sats),if t.settled_at.is_some(){"Settled"}else if t.released_at.is_some(){"Released"}else if t.paid_at.is_some(){"Accepted"}else{"Unknown"}))
                div.node-detail {
                    (identity(t))
                    dl { (fact("Payment hash",Some(&t.payment_hash))) (fact("Accepted",t.paid_at.as_deref())) (fact("Settled",t.settled_at.as_deref())) (fact("Released",t.released_at.as_deref())) }
                    p { "Stake: " (c.event_submission.entry_fee) " sats; service fee: " (c.event_submission.coordinator_fee.fee_for(c.event_submission.entry_fee as u64)) " sats; recorded network fee: " (t.network_fee_sats) " sats." }
                    p.note { "Invoice face value excludes payer routing fees, which this coordinator cannot observe." }
                }
            }
            span.fund-arrow aria-hidden="true" { "→" }
            details.money-node {
                (node_summary(if t.escrow_address.is_some(){"Ark"}else{"Legacy"},t,sats(t.escrow_sats),if t.funded_at.is_some(){"Funded"}else{"Unknown"}))
                div.node-detail {
                    p.trace-position { (position(flow)) }
                    (identity(t))
                    dl { (fact("Swap",t.swap_id.as_deref())) (fact("VTXO outpoint",t.vtxo.as_deref())) (fact("Escrow address",t.escrow_address.as_deref())) }
                    p.note { "Escrow funding alone does not prove the customer's Lightning payment settled. Check current swap and spend evidence." }
                    @if let Some(tx)=&t.legacy_escrow_tx { @if let Ok(tx)=bitcoin::consensus::encode::deserialize_hex::<bitcoin::Transaction>(tx) { (super::transactions::transaction_diagram(&tx,network,explorer,"Legacy escrow","Recorded escrow transaction",None,None)) } }
                    dl { (fact("Legacy escrow reclaimed",t.legacy_reclaimed_at.as_deref())) }
                    @if let Some(reason)=&t.write_off { p.attention { "Written off: " (reason) } p.note { "A write-off is not payment to the customer." } }
                    a href=(format!("/admin/funds/tickets/{}",t.ticket_id)) hx-get=(format!("/admin/funds/tickets/{}",t.ticket_id)) hx-target=(format!("#live-{}",t.ticket_id)) hx-swap="innerHTML" { "Check services now" }
                    div id=(format!("live-{}",t.ticket_id)) aria-live="polite" {}
                }
            }
        }
        (refund_nodes(c,t))
    } }
}
fn payout_nodes(c: &Competition, flow: &TicketFlow) -> Markup {
    let t = &flow.ticket;
    html! { div.entry-payouts {
        details.money-node {
            @let latest=flow.payouts.iter().rev().find(|p|p.succeeded_at.is_some()).or_else(||flow.payouts.iter().rev().find(|p|p.failed_at.is_none())).or_else(||flow.payouts.last());
            (node_summary("LN out",t,latest.and_then(|p|sats(Some(p.amount_sats))).or_else(||payout_owed(c,t)),if flow.payouts.iter().any(|p|p.succeeded_at.is_some()){"Success recorded"}else if flow.payouts.iter().any(|p|p.failed_at.is_none()){"Pending"}else if !flow.payouts.is_empty(){"Failed"}else if payout_owed(c,t)==Some(0){"No payout owed"}else{"No payout recorded"}))
            div.node-detail {
                (identity(t))
                p { "Contract entitlement: " (amount(payout_owed(c,t))) }
                @if flow.payouts.is_empty() { p.note { "No outgoing payout recorded. Missing payment records do not establish a zero entitlement." } }
                @for p in &flow.payouts {
                    details.payout-attempt {
                        summary { (p.amount_sats) " sats · " @if p.succeeded_at.is_some() { "Success recorded" } @else if p.failed_at.is_some() { "Failed attempt" } @else { "Pending" } }
                        dl { (fact("Payment hash",p.payment_hash.as_deref())) (fact("Payout ID",Some(&p.id))) (fact("Destination",p.lightning_address.as_deref())) (fact("Initiated",Some(&p.initiated_at))) (fact("Succeeded",p.succeeded_at.as_deref())) (fact("Failed",p.failed_at.as_deref())) }
                        p.note { (p.send_attempts) " failed sends of this invoice. " @if let Some(at)=p.next_send_at { "Next send at Unix " (at) "." } }
                    }
                }
                @if !flow.jobs.is_empty() { details { summary { "Preparation / release jobs (" (flow.jobs.len()) ")" }
                    @for job in &flow.jobs { dl { (fact("Job",Some(&job.id))) (fact("Payout",job.payout_id.as_deref())) } p.note { (job.attempts) " attempts · " @if job.completed_at.is_some() { "completed" } @else if job.failed_at.is_some() { "failed" } @else { "retry at Unix " (job.retry_at) } } }
                } }
            }
        }
        @if t.sellback_at.is_some() || t.reclaimed_at.is_some() { details.money-node.recovery-branch {
            (node_summary("↳ Chain recovery",t,None,"Verify receipt"))
            div.node-detail { (identity(t)) dl { (fact("Sellback broadcast",t.sellback_at.as_deref())) (fact("Reclaim broadcast",t.reclaimed_at.as_deref())) } p.note { "These timestamps do not prove receipt by the customer. Inspect the spending transactions and destinations." } }
        } }
    } }
}
fn pool_node(c: &Competition, page: &FundsPage, explorer: &str) -> Markup {
    let funding = c.funding_outpoint;
    let funding_amount = funding
        .and_then(|outpoint| {
            c.funding_transaction
                .as_ref()
                .filter(|tx| tx.compute_txid() == outpoint.txid)
                .and_then(|tx| tx.output.get(outpoint.vout as usize))
        })
        .map(|out| out.value.to_sat());
    html! { div.money-pool {
        h3 { "Shared funding" }
        details.money-node {
            summary {
                strong.node-rail { "DLC" }
                span.node-id { "pool " (identifier(&c.id.to_string())) }
                span.node-amount { (amount(funding_amount)) }
                span.node-status { @if c.funding_confirmed_at.is_some() { "Confirmed" } @else if c.funding_broadcasted_at.is_some() { "Broadcast" } @else { "Not broadcast" } }
            }
            div.node-detail {
                dl { (fact("Competition",Some(&c.id.to_string()))) }
                @if let Some(outpoint)=funding { (chain_id("Funding",&outpoint.txid.to_string(),explorer)) p { "DLC output: " (outpoint.vout) } }
                @else { p { "No funding outpoint recorded" } }
                @if let Some(batch)=&page.commitment { dl { (fact("Arkade batch",Some(&batch.batch_id))) } p.note { "Batch funding output " (batch.funding_vout) ". A batch can also contain unrelated outputs." } }
                p.note { "Escrow membership follows the ticket's current competition. Verify the transfer with the Arkade indexer's settled-into-batch field." }
                a href="#recorded-transactions" { "Transaction records ↓" }
                a href=(format!("/admin/operations/{}#chain-evidence",c.id)) { "Operations and chain evidence" }
                a href=(format!("/admin/funds/chain/{}",c.id)) hx-get=(format!("/admin/funds/chain/{}",c.id)) hx-target="#chain-live" hx-swap="innerHTML" { "Check chain spends now" }
                div id="chain-live" aria-live="polite" {}
            }
        }
    } }
}
fn transaction_records(c: &Competition, network: &str, explorer: &str) -> Markup {
    html! { @if c.funding_transaction.is_some() || c.outcome_transaction.is_some() {
        section id="recorded-transactions" { h3 { "Transactions" }
            @if let Some(tx)=&c.funding_transaction { (super::transactions::transaction_diagram(tx,network,explorer,"Funding","Recorded transaction; use chain check for current confirmation",None,None)) }
            @if let Some(tx)=&c.outcome_transaction { (super::transactions::transaction_diagram(tx,network,explorer,if c.expiry_broadcasted_at.is_some(){"Expiry"}else{"Outcome"},"Recorded transaction; use chain check for current confirmation",None,c.funding_transaction.as_ref())) }
        }
    } }
}

/// Navigation uses the queue's actual membership; the stake is not a receipt of funding.
pub fn pool_navigation(parent: &Competition, selected: uuid::Uuid) -> Markup {
    let Some(queue) = &parent.queue else {
        return html! {};
    };
    if queue.pools.is_empty() {
        return html! {};
    }
    html! {
        section.pool-navigation {
            p { a href=(format!("/admin/funds?competition={}",parent.id)) aria-current=[(selected==parent.id).then_some("page")] { "Competition overview" } " · " (queue.entries) " paid entries · " (queue.pools.len()) " pools" }
            nav.pool-choices aria-label="Competition pools" {
                @for pool in &queue.pools {
                    a.pool-choice href=(format!("/admin/funds?competition={}",pool.competition_id)) aria-current=[(selected==pool.competition_id).then_some("page")] {
                        strong { "Pool " (u64::from(pool.pool_index)+1) }
                        span { (pool.players) " entries" }
                        span.note { @if let Some(stake)=(pool.players as u64).checked_mul(queue.stake_sats) { (stake) " sats stake" } @else { "Stake unknown" } }
                    }
                }
            }
        }
    }
}

fn outgoing(c: &Competition, flow: &TicketFlow) -> (Option<u64>, &'static str) {
    let t = &flow.ticket;
    if let Some(p) = flow.payouts.iter().rev().find(|p| p.succeeded_at.is_some()) {
        return (sats(Some(p.amount_sats)), "LN payout · success");
    }
    if matches!(t.refund_state.as_deref(), Some("paid" | "settled")) {
        return (
            t.refund_invoice.as_deref().and_then(invoice_sats),
            "LN refund · paid",
        );
    }
    if t.released_at.is_some() {
        return (t.invoice.as_deref().and_then(invoice_sats), "Hold released");
    }
    if t.write_off.is_some() {
        return (
            t.refund_invoice.as_deref().and_then(invoice_sats),
            "Written off",
        );
    }
    if t.refund_state.is_some() {
        return (
            t.refund_invoice.as_deref().and_then(invoice_sats),
            "LN refund · pending",
        );
    }
    if refund_trigger(c).is_some() && t.funded_at.is_some() && !t.escrow_pooled {
        return (
            None,
            refund_stage(c, t, OffsetDateTime::now_utc().unix_timestamp()),
        );
    }
    if let Some(p) = flow.payouts.iter().rev().find(|p| p.failed_at.is_none()) {
        return (sats(Some(p.amount_sats)), "LN payout · pending");
    }
    if let Some(p) = flow.payouts.last() {
        return (sats(Some(p.amount_sats)), "LN payout · failed");
    }
    if t.sellback_at.is_some() || t.reclaimed_at.is_some() {
        return (None, "Chain recovery");
    }
    if payout_owed(c, t) == Some(0) {
        return (Some(0), "No payout owed");
    }
    (payout_owed(c, t), "No payout recorded")
}

/// A pool remains one contract; each row expands into that customer's payment rails.
pub fn pool_funds(c: &Competition, page: &FundsPage, network: &str, explorer: &str) -> Markup {
    html! {
        section.pool-funds {
            div.pool-heading {
                div { h2 { @if let Some(index)=c.pool_index { "Pool " (u64::from(index)+1) } @else { "Pool funds" } }
                    p.note { (page.tickets.len()) " tickets shown · " (page.total) " in this pool" }
                    p.note { "LN payment → Ark escrow → shared DLC → LN payout. Expand a row for its evidence." }
                    a href=(format!("/admin/funds?competition={}&view=flow",c.id)) { "Show all nodes" }
                }
                (pool_node(c,page,explorer))
            }
            div.pool-row-heading aria-hidden="true" { span { "Entry / ticket" } span { "Lightning in" } span { "Ark escrow" } span { "Payout / return" } span {} }
            div.pool-entries {
                @for flow in &page.tickets {
                    @let t=&flow.ticket;
                    @let out=outgoing(c,flow);
                    details.pool-entry {
                        summary {
                            span.pool-entry-id { @if let Some(id)=&t.entry_id { "Entry " (identifier(id)) } @else { "Ticket " (identifier(&t.ticket_id)) } }
                            span.pool-cell { span.pool-cell-label { "LN in" } strong { (amount(t.invoice.as_deref().and_then(invoice_sats))) } small { @if t.settled_at.is_some(){"Settled"}@else if t.released_at.is_some(){"Released"}@else if t.paid_at.is_some(){"Accepted"}@else{"Unknown"} } }
                            span.pool-cell { span.pool-cell-label { "Ark" } strong { (amount(sats(t.escrow_sats))) } small { @if t.funded_at.is_some(){"Funded"}@else{"Unknown"} } }
                            span.pool-cell { span.pool-cell-label { "Out" } strong { (amount(out.0)) } small { (out.1) } }
                        }
                        div.pool-entry-detail {
                            p { (position(flow)) " · " a href=(format!("/admin/funds?competition={}&ticket={}",c.id,t.ticket_id)) { "Open full entry trace ↗" } }
                            div.pool-entry-nodes {
                                div { h3 { "Payment → escrow" } (payment_nodes(c,flow,network,explorer)) }
                                div { h3 { "Payout / return" } (payout_nodes(c,flow)) }
                            }
                        }
                    }
                }
            }
            (transaction_records(c,network,explorer))
        }
    }
}

pub fn chain_id(label: &str, id: &str, explorer: &str) -> Markup {
    html! { dl { div.trace-fact { dt { (label) } dd { @if explorer.is_empty() { code { (id) } } @else { a href=(format!("{}/tx/{id}",explorer.trim_end_matches('/'))) rel="noreferrer" { code { (id) } } } } } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin_funds::FundsPayout;

    fn competition() -> Competition {
        Competition::new(&serde_json::from_value(serde_json::json!({
            "id":uuid::Uuid::now_v7(),"signing_date":"2026-10-04T01:00:00Z",
            "start_observation_date":"2026-10-03T00:00:00Z","end_observation_date":"2026-10-04T00:00:00Z",
            "locations":["KSEA"],"number_of_values_per_entry":3,"number_of_places_win":1,
            "total_allowed_entries":3,"entry_fee":5000,"coordinator_fee_percentage":5,"total_competition_pool":15000
        })).unwrap())
    }

    #[test]
    fn refund_path_is_visible_before_a_swap_and_never_promises_a_clock_time_payment() {
        let mut c = competition();
        let mut t = FundsTicket {
            escrow_address: Some("ark-escrow".into()),
            funded_at: Some(1),
            refund_opens_at: Some(1_800_000_100),
            ..Default::default()
        };
        assert_eq!(refund_stage(&c, &t, 1_800_000_000), "Fallback only");
        c.cancelled_at = OffsetDateTime::from_unix_timestamp(1_800_000_000).ok();
        assert_eq!(refund_stage(&c, &t, 1_800_000_000), "Timelocked");
        assert_eq!(
            refund_stage(&c, &t, 1_800_000_101),
            "Awaiting chain / worker"
        );
        let html = refund_nodes(&c, &t).into_string();
        assert!(
            html.contains("Ark return")
                && html.contains("LN refund")
                && html.contains("Refund opens")
        );
        assert!(html.contains("Competition cancelled") && html.contains("chain time can lag"));
        t.escrow_pooled = true;
        assert!(
            refund_nodes(&c, &t).into_string().is_empty(),
            "spent escrows must not promise another refund"
        );
        t.escrow_pooled = false;
        t.refund_opens_at = None;
        assert_eq!(refund_stage(&c, &t, 1_800_000_000), "Opening time unknown");
        c.cancelled_at = None;
        c.kind = crate::domain::CompetitionKind::Queued;
        c.pools_formed_at = Some(OffsetDateTime::UNIX_EPOCH);
        assert!(refund_trigger(&c).unwrap().0.contains("not assigned"));
    }

    #[test]
    fn refund_transfer_and_writeoff_do_not_claim_lightning_payment() {
        let c = competition();
        let mut t = FundsTicket {
            refund_id: Some("refund-id".into()),
            refund_state: Some("submitted".into()),
            refund_ark_txid: Some("ark-tx".into()),
            refund_created_at: Some(1_800_000_000),
            refund_updated_at: Some(1_800_000_010),
            ..Default::default()
        };
        let html = refund_nodes(&c, &t).into_string();
        assert!(
            html.contains("Transfer recorded")
                && html.contains("Current swap created")
                && html.contains("Last state update")
        );
        assert!(!html.contains("Paid recorded"));
        t.write_off = Some("<script>unsafe</script>".into());
        let html = refund_nodes(&c, &t).into_string();
        assert!(html.contains("Written off") && !html.contains("<script>"));
        assert!(!html.contains("Paid recorded"));
        t.refund_state = Some("paid".into());
        assert_eq!(refund_stage(&c, &t, 0), "Paid recorded");
        let hold = FundsTicket {
            released_at: Some("2026-10-02 12:00:00".into()),
            ..Default::default()
        };
        let html = refund_nodes(&c, &hold).into_string();
        assert!(html.contains("LN release") && !html.contains("Ark return"));
    }
    #[test]
    fn customer_position_keeps_payment_success_separate_from_writeoffs_and_attempts() {
        let mut flow = TicketFlow::default();
        flow.ticket.write_off = Some("unrecoverable".into());
        assert!(position(&flow).contains("support review"));
        flow.payouts.push(FundsPayout {
            failed_at: Some("failed".into()),
            ..Default::default()
        });
        assert!(!position(&flow).contains("successful"));
        flow.payouts.push(FundsPayout {
            succeeded_at: Some("paid".into()),
            ..Default::default()
        });
        assert!(position(&flow).contains("successful"));
        let escaped = fact("Hash", Some("<script>bad</script>")).into_string();
        assert!(!escaped.contains("<script>"));
        assert!(amount(None).into_string().contains("unknown"));
    }
}
