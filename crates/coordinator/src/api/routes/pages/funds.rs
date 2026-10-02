//! Operator-only, read-only support lookup and bounded service inspections.
use super::admin::render_admin_fragment;
use crate::{
    api::admin_auth::AdminCsrf,
    startup::AppState,
    templates::admin::funds::{chain_id, funds_graph, pool_funds, pool_navigation},
};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::Html,
    Extension,
};
use futures::{stream, StreamExt};
use maud::html;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Default, Deserialize)]
pub struct FundsFilter {
    #[serde(default)]
    pub q: String,
    pub competition: Option<Uuid>,
    pub ticket: Option<Uuid>,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub view: String,
}
pub async fn funds_page(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Query(filter): Query<FundsFilter>,
    headers: HeaderMap,
) -> Html<String> {
    let content = html! {
        main.admin-workspace {
            p.eyebrow { "Customer support" } h1 { "Follow the money" }
            form.discovery-filters method="get" action="/admin/funds" {
                label { "Competition, entry, ticket, payment hash, swap, batch or escrow outpoint" input name="q" value=(filter.q) maxlength="256" required; }
                button type="submit" { "Find funds" }
            }
            @if !filter.q.trim().is_empty() {
                @if filter.q.len()>256 { p.notice { "Identifier is too long." } }
                @else {
                    @match state.coordinator.competition_store.find_funds(filter.q.trim()).await {
                        Ok(matches)=> {
                            h2 { (matches.len().min(100)) " matching records" }
                            @for found in matches.iter().take(100) {
                                p { a href=(format!("/admin/funds?competition={}{}",found.competition_id,found.ticket_id.as_ref().map(|id|format!("&ticket={id}")).unwrap_or_default())) {
                                    "Competition " (found.competition_id) @if let Some(entry)=&found.entry_id { " · entry " (entry) } @if let Some(ticket)=&found.ticket_id { " · ticket " (ticket) }
                                } }
                            }
                            @if matches.len()>100 { p.notice { "More than 100 matches; open the competition to page through its tickets." } }
                            @if matches.is_empty() { p.notice { "No durable record matches this exact identifier. Try the entry payment hash or ticket ID. A missing record does not prove no payment occurred; service-only transactions must be checked through their swap or escrow." } }
                        },
                        Err(_)=>p.notice { "Support records could not be loaded. Retry shortly." },
                    }
                }
            }
            @if let Some(id)=filter.competition {
                @match tokio::join!(state.coordinator.get_competition(id),state.coordinator.competition_store.funds_page(id,filter.ticket,filter.offset)) {
                    (Ok(c),Ok(page))=> {
                        h2 { (c.event_submission.locations.join(" · ")) }
                        p { code { (id) } " · " (c.get_state()) " · " a href=(format!("/admin/operations/{id}")) { "Operations and retained errors" } }
                        @if let Some(parent)=c.parent_id {
                            @match state.coordinator.get_competition(parent).await {
                                Ok(parent)=>(pool_navigation(&parent,c.id)),
                                Err(_)=>p.notice { "Pool navigation unavailable. " a href=(format!("/admin/funds?competition={parent}")) { "Open parent competition" } },
                            }
                        } @else { (pool_navigation(&c,c.id)) }
                        p.note { "Database evidence read at " (time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()) ". Open a ticket's service check for current Lightning and Arkade status." }
                        @if page.tickets.len()>5 && filter.ticket.is_none() && filter.view!="flow" {
                            (pool_funds(&c,&page,&state.network,&state.explorer_url))
                        } @else if page.total>0 || c.queue.as_ref().is_none_or(|queue|queue.pools.is_empty()) {
                            @if filter.view=="flow" { p { a href=(format!("/admin/funds?competition={id}")) { "Compact pool view" } } }
                            (funds_graph(&c,&page,&state.network,&state.explorer_url))
                        } @else { p.note { "Choose a pool to follow its entries, contract funding and payouts." } }
                        @if page.total>0 { p.note { (page.total) " tickets in this scope. Showing " (page.tickets.len()) " starting at " (u64::from(filter.offset)+1) "." } }
                        @if filter.ticket.is_some() { p { a href=(format!("/admin/funds?competition={id}")) { "All tickets in this competition" } } }
                        @if filter.offset>0 { a href=(format!("/admin/funds?competition={id}&offset={}&view={}",filter.offset.saturating_sub(25),if filter.view=="flow"{"flow"}else{""})) { "← Previous tickets" } }
                        @if i64::from(filter.offset)+25<page.total { a href=(format!("/admin/funds?competition={id}&offset={}&view={}",filter.offset.saturating_add(25),if filter.view=="flow"{"flow"}else{""})) { "Next tickets →" } }
                    },
                    _=>p.notice { "Competition or fund evidence could not be loaded." },
                }
            }
        }
    };
    render_admin_fragment(&headers, &state, &csrf, "Funds support", content)
}
pub async fn funds_ticket(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Html<String> {
    let content = match state.coordinator.competition_store.funds_ticket(id).await {
        Ok(Some(flow)) => {
            let facts = state.coordinator.inspect_funds(&flow).await;
            html! { section.live-facts { h3 { "Current service evidence" } p.note { "Checked " (time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()) }
                a href=(format!("/admin/funds?competition={}&ticket={id}",flow.ticket.competition_id)) { "Full ticket trace" }
                @for fact in facts { p { strong { (fact.source) ": " } (fact.text) } }
            } }
        }
        _ => html! { p.notice { "Ticket evidence unavailable." } },
    };
    render_admin_fragment(&headers, &state, &csrf, "Funds support", content)
}
pub async fn funds_chain(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Html<String> {
    let content = match state.coordinator.get_competition(id).await {
        Ok(c) => {
            let mut outputs = Vec::new();
            if let (Some(point), Some(tx)) = (c.funding_outpoint, c.funding_transaction.as_ref()) {
                if point.txid == tx.compute_txid() {
                    if let Some(output) = tx.output.get(point.vout as usize) {
                        outputs.push(("DLC funding".to_string(), point, output.clone()));
                    }
                }
            }
            if let Some(tx) = &c.outcome_transaction {
                for (index, output) in tx.output.iter().enumerate().take(50) {
                    outputs.push((
                        "Outcome / expiry".to_string(),
                        bitcoin::OutPoint::new(tx.compute_txid(), index as u32),
                        output.clone(),
                    ));
                }
            }
            let chain = state.bitcoin.clone();
            let facts: Vec<_> = stream::iter(outputs)
                .map(|(label, point, output)| {
                    let chain = chain.clone();
                    async move {
                        let (status, spend) = tokio::join!(
                            tokio::time::timeout(
                                std::time::Duration::from_secs(4),
                                chain.payout_output_status(point, output.clone())
                            ),
                            tokio::time::timeout(
                                std::time::Duration::from_secs(4),
                                chain.spending_transaction(point, output)
                            )
                        );
                        (label, point, status, spend)
                    }
                })
                .buffered(8)
                .collect()
                .await;
            html! { section.live-facts { h3 { "Chain evidence" }
                p.note { "Checked " (time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()) ". Funding output and first 50 outcome outputs; follows each recorded output's direct spend. A spend alone does not prove a customer received funds." }
                @if facts.is_empty() { p { "No recorded contract outputs to inspect." } }
                @for (label,point,status,spend) in facts {
                    details { summary { (label) " · output " (point.vout) }
                        (chain_id("Source transaction",&point.txid.to_string(),&state.explorer_url))
                        @match status {
                            Ok(Ok(status))=>p { "Confirmation height: " (status.confirmation_height.map(|v|v.to_string()).unwrap_or_else(||"unknown / unconfirmed".into())) ". " @if status.unspent { "Output listed unspent." } @else { "Output not listed unspent; inspect spending evidence." } },
                            _=>p { "Output status lookup unavailable." },
                        }
                        @match spend {
                            Ok(Ok(Some(tx)))=>(crate::templates::admin::transactions::transaction_diagram(&tx,&state.network,&state.explorer_url,"Spending","Observed spend of this exact output; check confirmation in explorer",None,c.outcome_transaction.as_ref().filter(|t|t.compute_txid()==point.txid).or(c.funding_transaction.as_ref()))),
                            Ok(Ok(None))=>p { "No spending transaction found in the checked history." },
                            _=>p { "Spending transaction lookup incomplete or unavailable." },
                        }
                    }
                }
            } }
        }
        _ => html! { p.notice { "Competition evidence unavailable." } },
    };
    render_admin_fragment(&headers, &state, &csrf, "Funds support", content)
}
