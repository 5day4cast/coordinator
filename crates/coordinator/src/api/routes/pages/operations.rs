//! Operator work queue and transaction evidence. All routes here are read-only.
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::Html,
    Extension,
};
use bitcoin::Transaction;
use maud::{html, Markup};
use serde::Deserialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::{
    api::{admin_auth::AdminCsrf, routes::OperatorCompetition},
    domain::{Competition, CompetitionKind, RefundProgress},
    startup::AppState,
};

use super::admin::render_admin_fragment;

#[derive(Default, Deserialize)]
pub struct QueueFilter {
    #[serde(default)]
    q: String,
    #[serde(default)]
    show: String,
}

fn next(
    c: &Competition,
    refunds: Option<&RefundProgress>,
    now: OffsetDateTime,
) -> (&'static str, &'static str, bool) {
    if c.kind == CompetitionKind::Queued && c.pools_formed_at.is_some() {
        return (
            "Follow child pools",
            "Each pool has its own contract, settlement and payouts.",
            false,
        );
    }
    if c.failed_at.is_some() || c.cancelled_at.is_some() {
        if refunds.is_some_and(|r| r.refunded < r.escrowed || r.released + r.settled < r.held) {
            return (
                "Review refunds",
                "Check the refund opening time and the recorded escrow progress.",
                true,
            );
        }
        return (
            "Review terminal state",
            "Check the retained errors and refund evidence.",
            true,
        );
    }
    if c.completed_at.is_some() {
        return (
            "Review payout evidence",
            "Contract processing completed. Player receipt must be checked separately.",
            false,
        );
    }
    if c.funding_broadcasted_at.is_some() && c.funding_confirmed_at.is_none() {
        let delayed = c
            .funding_broadcasted_at
            .is_some_and(|at| now - at >= time::Duration::minutes(20));
        return ("Check funding confirmation", "Look up the transaction and fee evidence. Twenty minutes without a recorded confirmation triggers review.", delayed);
    }
    if c.awaiting_attestation_at.is_some()
        && c.outcome_broadcasted_at.is_none()
        && c.expiry_broadcasted_at.is_none()
    {
        let due = now >= c.event_submission.signing_date;
        return (
            if due {
                "Check oracle settlement"
            } else {
                "Wait for weather"
            },
            if due {
                "Signing time has arrived. Check oracle evidence and the signed expiry path."
            } else {
                "The observation or signing window is still open."
            },
            due,
        );
    }
    if c.outcome_broadcasted_at.is_some() || c.expiry_broadcasted_at.is_some() {
        return (
            "Follow settlement outputs",
            "Check confirmation and relative timelocks before the next spend.",
            false,
        );
    }
    if now < c.event_submission.start_observation_date {
        return (
            "Collect entries",
            "Registration remains open until the observation window starts.",
            false,
        );
    }
    (
        "Check lifecycle progress",
        "Registration has closed. Inspect milestones, worker health and retained errors.",
        true,
    )
}

pub async fn operations_page(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Query(filter): Query<QueueFilter>,
    headers: HeaderMap,
) -> Html<String> {
    let (competitions, refunds, monitoring) = tokio::join!(
        state.coordinator.list_competitions(),
        state.coordinator.refund_progress(None),
        state.admin_monitoring.read()
    );
    let content = match (competitions, refunds) {
        (Ok(mut competitions), Ok(refunds)) => {
            let now = OffsetDateTime::now_utc();
            competitions.sort_by_key(|c| {
                (
                    !next(c, refunds.get(&c.id), now).2,
                    std::cmp::Reverse(c.created_at),
                )
            });
            let search = filter.q.trim().to_lowercase();
            let rows: Vec<_> = competitions
                .iter()
                .filter(|c| {
                    (search.is_empty()
                        || c.id.to_string().contains(&search)
                        || c.event_submission
                            .locations
                            .iter()
                            .any(|s| s.to_lowercase().contains(&search)))
                        && (filter.show != "attention" || next(c, refunds.get(&c.id), now).2)
                })
                .collect();
            html! {
                main.admin-workspace {
                    p.eyebrow { "Operator desk" } h1 { "Competition operations" }
                    p { "Follow the next step, inspect the money, and distinguish expected waiting from a competition that needs review." }
                    (state.admin_monitoring.render(&monitoring))
                    form.discovery-filters method="get" action="/admin/operations" {
                        label { "Competition or station" input name="q" value=(filter.q) maxlength="100"; }
                        label { "Show" select name="show" { option value="all" selected[filter.show != "attention"] { "All competitions" } option value="attention" selected[filter.show == "attention"] { "Needs review" } } }
                        button type="submit" { "Filter" }
                    }
                    p.note { "Review order uses recorded milestones and scheduled times. Retained errors and historical failed jobs do not establish that money is still unpaid." }
                    h2 { (rows.len()) " competitions" }
                    div.scroll { table.ops-table {
                        thead { tr { th { "Competition" } th { "State" } th { "Entries" } th { "Next step" } } }
                        tbody {
                            @for c in rows.iter().take(200) {
                                @let action = next(c, refunds.get(&c.id), now);
                                tr {
                                    td { a href=(format!("/admin/operations/{}", c.id)) { (c.event_submission.locations.join(" · ")) }
                                        p.note { code { (c.id) } " · " (c.kind.as_str()) }
                                    }
                                    td { span.status { (c.get_state()) } }
                                    td { (c.total_paid_entries) " paid / " (c.total_entries) " entered" }
                                    td { strong class=[action.2.then_some("attention")] { (action.0) } p.note { (action.1) } }
                                }
                            }
                        }
                    } }
                    @if rows.len() > 200 { p.notice { "Showing the first 200 matches. Filter by competition or station to narrow the work queue." } }
                }
            }
        }
        _ => {
            html! { main.admin-workspace { h1 { "Competition operations" } p.notice { "Competition or refund records could not be loaded. Retry shortly." } } }
        }
    };
    render_admin_fragment(&headers, &state, &csrf, "Competition operations", content)
}

pub async fn operation_detail(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Html<String> {
    let ids = [id];
    let (competition, refunds, written_off) = tokio::join!(
        state.coordinator.get_competition(id),
        state.coordinator.refund_status(&ids),
        state.coordinator.refund_write_offs(Some(id))
    );
    let content = match (competition, refunds, written_off) {
        (Ok(c), Ok(refunds), Ok(written_off)) => {
            let action = next(&c, refunds.get(&id), OffsetDateTime::now_utc());
            let operator = OperatorCompetition::new(&c, refunds.get(&id).copied(), written_off);
            let funding = transaction(
                &state,
                c.funding_transaction.as_ref(),
                "Funding",
                c.funding_broadcasted_at.is_some(),
                c.funding_psbt_base64.as_deref(),
                None,
            )
            .await;
            let outcome = transaction(
                &state,
                c.outcome_transaction.as_ref(),
                if c.expiry_broadcasted_at.is_some() {
                    "Expiry"
                } else {
                    "Outcome"
                },
                c.outcome_broadcasted_at.is_some() || c.expiry_broadcasted_at.is_some(),
                None,
                c.funding_transaction.as_ref(),
            )
            .await;
            html! {
                main.admin-workspace {
                    a href="/admin/operations" { "← All competitions" }
                    h1 { (c.event_submission.locations.join(" · ")) }
                    p.note { code { (id) } " · " (c.get_state()) }
                    @if let Some(parent) = c.parent_id { p { "Pool of " a href=(format!("/admin/operations/{parent}")) { (parent) } } }
                    div.notice { strong { (action.0) } p { (action.1) } }
                    @if let Some(queue) = &c.queue {
                        h2 { "Child pools" }
                        @for pool in &queue.pools { p { a href=(format!("/admin/operations/{}", pool.competition_id)) { "Pool " (pool.pool_index) } " · " (pool.players) " players" } }
                    }
                    h2 { "Customer funds" }
                    p { a href=(format!("/admin/funds?competition={id}")) { "Trace Lightning payments → Arkade escrows → DLC funding → Lightning payouts" } }
                    p.note { "Includes individual tickets, every recorded payout attempt, refunds, write-offs and live service checks." }
                    div id="chain-evidence" {}
                    (funding) (outcome)
                    section {
                        h2 { "Recovery decision" }
                        p { "Confirm the transaction status and its spendable outputs before choosing a fee bump. These signed contract transactions reference exact outpoints; changing a parent's transaction ID can invalidate descendants." }
                        p { "Fee bumping requires a review in the wallet that controls the transaction inputs or spendable outputs. Preserve the contract’s signed descendants." }
                        a href="/admin/wallet" { "Inspect coordinator wallet" }
                    }
                    @if let Some(r) = operator.refunds {
                        h2 { "Refund progress" }
                        p { (r.refunded) " refunded of " (r.escrowed) " funded escrows; " (r.written_off) " written off." }
                        p { (r.held) " held invoices: " (r.released) " released, " (r.settled) " settled." }
                        @if let Some(at) = r.opens_at { p { "Earliest refund opening: " (at.format(&Rfc3339).unwrap_or_default()) } }
                    }
                    @if !operator.refund_write_offs.is_empty() { details { summary { "Refund write-offs" } pre { (serde_json::to_string_pretty(&operator.refund_write_offs).unwrap_or_default()) } } }
                    h2 { "Milestones" }
                    div.scroll { table.ops-table { tbody { @for milestone in &operator.milestones { tr { th { (milestone.name) } td { (milestone.at.format(&Rfc3339).unwrap_or_default()) } } } } } }
                    details { summary { "Retained errors (" (operator.errors.len()) ")" } pre { (serde_json::to_string_pretty(&operator.errors).unwrap_or_default()) } }
                    @if c.total_paid_entries == 0 {
                        details {
                            summary { "Delete this unpaid competition" }
                            p.note { "The coordinator rechecks that nobody has paid before deleting." }
                            form hx-post="/admin/api/competitions/delete" hx-target="#delete-result" hx-swap="innerHTML" hx-confirm="Delete this unpaid competition? This cannot be undone." {
                                input type="hidden" name="competition_id" value=(id);
                                button type="submit" { "Delete competition" }
                            }
                            div id="delete-result" role="status" {}
                        }
                    }

                }
            }
        }
        _ => {
            html! { main.admin-workspace { h1 { "Competition unavailable" } p { "The competition or its refund evidence could not be loaded." } a href="/admin/operations" { "Return to operations" } } }
        }
    };
    render_admin_fragment(&headers, &state, &csrf, "Competition funds", content)
}

async fn transaction(
    state: &AppState,
    tx: Option<&Transaction>,
    title: &str,
    broadcast: bool,
    psbt: Option<&str>,
    previous: Option<&Transaction>,
) -> Markup {
    let Some(tx) = tx else {
        return html! { p.note { (title) ": no transaction recorded." } };
    };
    let txid = tx.compute_txid();
    let status = if broadcast {
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            state.bitcoin.get_tx_confirmation_height(&txid),
        )
        .await
        {
            Ok(Ok(Some(height))) => format!("Confirmed at block {height}"),
            Ok(Ok(None)) => "No confirmation found; check mempool presence in the explorer".into(),
            _ => "Chain lookup unavailable; confirmation unknown".into(),
        }
    } else {
        "Prepared transaction; no broadcast recorded".into()
    };
    crate::templates::admin::transactions::transaction_diagram(
        tx,
        &state.network,
        &state.explorer_url,
        title,
        &status,
        psbt,
        previous,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CoordinatorFee, CreateEvent};

    fn competition(now: OffsetDateTime) -> Competition {
        Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: now + time::Duration::days(2),
            start_observation_date: now + time::Duration::DAY,
            end_observation_date: now + time::Duration::days(2) - time::Duration::HOUR,
            locations: vec!["KSEA".into()],
            number_of_values_per_entry: 1,
            number_of_places_win: 1,
            total_allowed_entries: 3,
            entry_fee: 5000,
            coordinator_fee: CoordinatorFee::whole_percent(5),
            total_competition_pool: 15000,
            relative_locktime_block_delta: None,
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
        })
    }

    #[test]
    fn planned_weather_wait_and_completed_contract_do_not_imply_stuck_money() {
        let now = OffsetDateTime::now_utc();
        let mut c = competition(now);
        c.awaiting_attestation_at = Some(now);
        assert_eq!(
            next(&c, None, now),
            (
                "Wait for weather",
                "The observation or signing window is still open.",
                false
            )
        );
        assert!(next(&c, None, now + time::Duration::days(3)).2);
        c.completed_at = Some(now);
        assert_eq!(next(&c, None, now).0, "Review payout evidence");
        assert!(!next(&c, None, now).2);
    }

    #[test]
    fn unconfirmed_funding_is_reviewed_after_the_stated_interval() {
        let now = OffsetDateTime::now_utc();
        let mut c = competition(now);
        c.funding_broadcasted_at = Some(now - time::Duration::minutes(19));
        assert!(!next(&c, None, now).2);
        assert!(next(&c, None, now + time::Duration::minutes(2)).2);
        c.funding_confirmed_at = Some(now);
        assert!(!next(&c, None, now).2);
    }
}
