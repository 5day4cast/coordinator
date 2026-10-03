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
    domain::{
        leaderboard::Phase, Competition, CompetitionKind, OperatorPayoutProgress, RefundProgress,
    },
    startup::AppState,
};

use super::admin::render_admin_fragment;

#[derive(Default, Deserialize)]
pub struct QueueFilter {
    #[serde(default)]
    q: String,
    #[serde(default)]
    show: QueueShow,
    #[serde(default)]
    sort: QueueSort,
    #[serde(default)]
    page: usize,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum QueueSort {
    CreatedAsc,
    #[default]
    #[serde(other)]
    CreatedDesc,
}

impl QueueSort {
    fn as_str(self) -> &'static str {
        match self {
            Self::CreatedDesc => "created_desc",
            Self::CreatedAsc => "created_asc",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum QueueShow {
    Pending,
    Live,
    AwaitingResult,
    Finished,
    PaidOut,
    Attention,
    #[default]
    #[serde(other)]
    All,
}

impl QueueShow {
    const VIEWS: [Self; 7] = [
        Self::All,
        Self::Pending,
        Self::Live,
        Self::AwaitingResult,
        Self::Finished,
        Self::PaidOut,
        Self::Attention,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Pending => "pending",
            Self::Live => "live",
            Self::AwaitingResult => "awaiting_result",
            Self::Finished => "finished",
            Self::PaidOut => "paid_out",
            Self::Attention => "attention",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All competitions",
            Self::Pending => "Pending / open",
            Self::Live => "Live weather",
            Self::AwaitingResult => "Awaiting result",
            Self::Finished => "Finished",
            Self::PaidOut => "Paid out (Lightning)",
            Self::Attention => "Needs review",
        }
    }

    fn includes(
        self,
        c: &Competition,
        refunds: Option<&RefundProgress>,
        now: OffsetDateTime,
        payout: Option<&OperatorPayoutProgress>,
    ) -> bool {
        match self {
            Self::All => true,
            Self::Attention => next(c, refunds, now).2,
            // A queue that formed its pools is a navigation record. Its child pools carry
            // the active weather and settlement work, so do not count it a second time.
            _ if c.kind == CompetitionKind::Queued && c.pools_formed_at.is_some() => false,
            Self::Pending => Phase::of(c, now) == Phase::Upcoming,
            Self::Live => Phase::of(c, now) == Phase::Live,
            Self::AwaitingResult => Phase::of(c, now) == Phase::AwaitingResult,
            Self::Finished => finished(c, now),
            Self::PaidOut => finished(c, now) && payout.is_some_and(|p| p.all_paid()),
        }
    }
}

fn finished(c: &Competition, now: OffsetDateTime) -> bool {
    c.completed_at.is_some() || matches!(Phase::of(c, now), Phase::Scored | Phase::Expired)
}

const PAGE_SIZE: usize = 100;

fn page_bounds(matches: usize, requested_page: usize) -> (usize, usize, usize) {
    let pages = matches.div_ceil(PAGE_SIZE).max(1);
    let page = requested_page.min(pages - 1);
    (page, page * PAGE_SIZE, pages)
}

fn queue_url(filter: &QueueFilter, page: usize) -> String {
    let mut url = reqwest::Url::parse("http://localhost/admin/operations").expect("static URL");
    url.query_pairs_mut()
        .append_pair("q", &filter.q)
        .append_pair("show", filter.show.as_str())
        .append_pair("sort", filter.sort.as_str())
        .append_pair("page", &page.to_string());
    format!("{}?{}", url.path(), url.query().unwrap_or_default())
}

fn phase_label(c: &Competition, now: OffsetDateTime) -> &'static str {
    if c.kind == CompetitionKind::Queued && c.pools_finished_at.is_some() {
        return "Pools finished";
    }
    if c.kind == CompetitionKind::Queued && c.pools_formed_at.is_some() {
        return "Pools formed";
    }
    match Phase::of(c, now) {
        Phase::Upcoming => "Pending / open",
        Phase::Live => "Live weather",
        Phase::AwaitingResult => "Awaiting result",
        Phase::Scored => "Result signed",
        Phase::Expired => "Expired",
        Phase::Unfilled => "Unfilled",
        Phase::Cancelled => "Cancelled",
        Phase::Failed => "Failed",
    }
}

fn matches_search(c: &Competition, search: &str) -> bool {
    search.is_empty()
        || c.id.to_string().contains(search)
        || c.event_submission
            .locations
            .iter()
            .any(|station| station.to_lowercase().contains(search))
}

fn sort_competitions(competitions: &mut [Competition], sort: QueueSort) {
    competitions.sort_by(|left, right| {
        let created = left
            .created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id));
        match sort {
            QueueSort::CreatedDesc => created.reverse(),
            QueueSort::CreatedAsc => created,
        }
    });
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
            let payout_progress = state
                .coordinator
                .operator_payout_progress(&competitions)
                .await;
            sort_competitions(&mut competitions, filter.sort);
            let search = filter.q.trim().to_lowercase();
            let rows: Vec<_> = competitions
                .iter()
                .filter(|c| {
                    matches_search(c, &search)
                        && filter.show.includes(
                            c,
                            refunds.get(&c.id),
                            now,
                            payout_progress.as_ref().ok().and_then(|p| p.get(&c.id)),
                        )
                })
                .collect();
            let (page, start, pages) = page_bounds(rows.len(), filter.page);
            html! {
                main.admin-workspace {
                    p.eyebrow { "Operator desk" } h1 { "Competition operations" }
                    p { "Follow the next step, inspect the money, and distinguish expected waiting from a competition that needs review." }
                    (state.admin_monitoring.render(&monitoring))
                    nav.discovery-filters aria-label="Competition views" {
                        @for show in QueueShow::VIEWS {
                            @let count = competitions.iter().filter(|c| show.includes(c, refunds.get(&c.id), now, payout_progress.as_ref().ok().and_then(|p| p.get(&c.id)))).count();
                            a href=(queue_url(&QueueFilter { show, ..QueueFilter::default() }, 0)) aria-current=[(filter.show == show && search.is_empty()).then_some("page")] {
                                (show.label()) " (" (count) ")"
                            }
                        }
                    }
                    form.discovery-filters method="get" action="/admin/operations" {
                        label { "Competition or station" input name="q" value=(&filter.q) maxlength="100"; }
                        label { "Show" select name="show" { @for show in QueueShow::VIEWS { option value=(show.as_str()) selected[filter.show == show] { (show.label()) } } } }
                        label { "Sort by" select name="sort" {
                            option value="created_desc" selected[matches!(filter.sort, QueueSort::CreatedDesc)] { "Newest created" }
                            option value="created_asc" selected[matches!(filter.sort, QueueSort::CreatedAsc)] { "Oldest created" }
                        } }
                        button type="submit" { "Filter" }
                        a href="/admin/operations" { "Clear filters" }
                    }
                    p.note {
                        @match filter.sort {
                            QueueSort::CreatedDesc => { "Newest competitions first, by creation time. " },
                            QueueSort::CreatedAsc => { "Oldest competitions first, by creation time. " },
                        }
                        "Creation times are UTC. Retained errors and historical failed jobs do not establish that money is still unpaid."
                    }
                    h2 { (rows.len()) " competitions" }
                    p.note { "Pending / open means the observation window has not started; a game may already be full. Live weather means observations are in progress, even when the contract state says awaiting_attestation. View counts include all stations and reset the search." }
                    @if matches!(filter.show, QueueShow::Finished | QueueShow::PaidOut) {
                        p.note { "Finished includes signed results and ended contracts. Paid out requires successful recorded Lightning payments for every recipient and amount in the settled outcome. On-chain claims and unknown payout coverage remain in Finished; inspect the fund trace. Contract cleanup can continue after payment." }
                    }
                    @if payout_progress.is_err() { p.notice { "Payout evidence is unavailable. Finished competitions remain listed; paid-out coverage is unknown." } }
                    @if rows.is_empty() { p.notice { "No competitions match these filters. " a href="/admin/operations" { "Show all competitions" } } }
                    div.scroll { table.ops-table {
                        thead { tr { th { "Competition" } th { "Created (UTC)" } th { "State" } th { "Entries" } th { "Payouts" } th { "Next step" } } }
                        tbody {
                            @for c in rows.iter().skip(start).take(PAGE_SIZE) {
                                @let action = next(c, refunds.get(&c.id), now);
                                @let created = c.created_at.to_offset(time::UtcOffset::UTC);
                                @let payout = payout_progress.as_ref().ok().and_then(|p| p.get(&c.id));
                                tr {
                                    td { a href=(format!("/admin/operations/{}", c.id)) { (c.event_submission.locations.join(" · ")) }
                                        p.note { code { (c.id) } " · " (c.kind.as_str()) }
                                        a href=(format!("/admin/funds?competition={}", c.id)) { "Trace funds" }
                                    }
                                    td { time datetime=(created.format(&Rfc3339).unwrap_or_default()) {
                                        (created.date()) br; (format!("{:02}:{:02}:{:02}", created.hour(), created.minute(), created.second()))
                                    } }
                                    td { span.status { (phase_label(c, now)) } p.note { (c.get_state()) } }
                                    td { (c.total_paid_entries) " paid / " (c.total_entries) " entered" }
                                    td {
                                        @if finished(c, now) && c.kind != CompetitionKind::Queued {
                                            a href=(format!("/admin/funds?competition={}", c.id)) {
                                                @if let Some(payout) = payout { (payout.paid) " / " (payout.expected) " LN paid" }
                                                @else { "Coverage unknown" }
                                            }
                                            p.note { @if c.completed_at.is_some() { "Contract cleanup complete" } @else { "Contract cleanup pending" } }
                                        } @else { "—" }
                                    }
                                    td { strong class=[action.2.then_some("attention")] { (action.0) } p.note { (action.1) } }
                                }
                            }
                        }
                    } }
                    @if pages > 1 {
                        nav.discovery-filters aria-label="Competition pages" {
                            @if page > 0 { a href=(queue_url(&filter, page - 1)) { "← Previous" } }
                            span { "Page " (page + 1) " of " (pages) " · " (start + 1) "–" ((start + PAGE_SIZE).min(rows.len())) " of " (rows.len()) }
                            @if page + 1 < pages { a href=(queue_url(&filter, page + 1)) { "Next →" } }
                        }
                    }
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
    fn creation_sort_keeps_recent_progress_ahead_of_older_refunds() {
        let now = OffsetDateTime::now_utc();
        let mut old_refund = competition(now);
        old_refund.created_at = now - time::Duration::DAY;
        old_refund.cancelled_at = Some(now);
        let mut recent = competition(now);
        recent.created_at = now;
        let old_id = old_refund.id;
        let recent_id = recent.id;
        let mut rows = vec![old_refund, recent];
        sort_competitions(&mut rows, QueueFilter::default().sort);
        assert_eq!(rows[0].id, recent_id);
        sort_competitions(&mut rows, QueueSort::CreatedAsc);
        assert_eq!(rows[0].id, old_id);
        assert!(QueueShow::Attention.includes(&rows[0], None, now, None));
        assert!(!QueueShow::Attention.includes(&rows[1], None, now, None));
    }

    #[test]
    fn old_review_links_fall_back_to_creation_order_and_keep_the_review_filter() {
        let Query(filter) = Query::<QueueFilter>::try_from_uri(
            &"/admin/operations?show=attention&sort=review"
                .parse()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(filter.sort, QueueSort::CreatedDesc);
        assert_eq!(filter.show, QueueShow::Attention);
        let Query(default) =
            Query::<QueueFilter>::try_from_uri(&"/admin/operations".parse().unwrap()).unwrap();
        assert_eq!(default.sort, QueueSort::CreatedDesc);
        assert_eq!(default.show, QueueShow::All);
    }

    #[test]
    fn pending_and_live_filters_use_the_weather_window_not_contract_state() {
        let now = OffsetDateTime::now_utc();
        let mut open = competition(now);
        open.kind = CompetitionKind::Queued;
        open.event_submission.unlisted = true;
        let mut live = competition(now);
        live.event_submission.start_observation_date = now - time::Duration::HOUR;
        live.awaiting_attestation_at = Some(now - time::Duration::minutes(30));
        live.funding_confirmed_at = Some(now - time::Duration::minutes(30));
        live.kind = CompetitionKind::Pool;
        live.event_submission.unlisted = true;
        assert_eq!(live.get_state().to_string(), "awaiting_attestation");
        assert!(QueueShow::Pending.includes(&open, None, now, None));
        assert!(!QueueShow::Live.includes(&open, None, now, None));
        assert!(QueueShow::Live.includes(&live, None, now, None));
        assert_eq!(phase_label(&live, now), "Live weather");
        assert!(!QueueShow::AwaitingResult.includes(&live, None, now, None));
        assert!(!QueueShow::Attention.includes(&live, None, now, None));

        let after_window = live.event_submission.end_observation_date;
        assert!(!QueueShow::Live.includes(&live, None, after_window, None));
        assert!(QueueShow::AwaitingResult.includes(&live, None, after_window, None));
        assert_eq!(phase_label(&live, after_window), "Awaiting result");

        // Parent queues still remain in All, without counting the same active games twice.
        open.pools_formed_at = Some(now);
        open.event_submission.start_observation_date = now - time::Duration::HOUR;
        assert_eq!(phase_label(&open, now), "Pools formed");
        assert!(QueueShow::All.includes(&open, None, now, None));
        assert!(!QueueShow::Live.includes(&open, None, now, None));

        let mut cancelled = competition(now);
        cancelled.cancelled_at = Some(now);
        assert!(!QueueShow::Pending.includes(&cancelled, None, now, None));
        assert!(!QueueShow::Live.includes(&cancelled, None, now, None));
        assert!(QueueShow::Attention.includes(&cancelled, None, now, None));
    }

    #[test]
    fn pages_reach_every_match_beyond_the_old_two_hundred_row_limit() {
        let now = OffsetDateTime::now_utc();
        let mut rows: Vec<_> = (0..205)
            .map(|index| {
                let mut c = competition(now);
                c.created_at = now - time::Duration::minutes(index);
                c
            })
            .collect();
        sort_competitions(&mut rows, QueueSort::CreatedDesc);
        let mut shown = Vec::new();
        for page in 0..3 {
            let (actual, start, pages) = page_bounds(rows.len(), page);
            assert_eq!((actual, pages), (page, 3));
            shown.extend(rows.iter().skip(start).take(PAGE_SIZE).map(|c| c.id));
        }
        assert_eq!(shown, rows.iter().map(|c| c.id).collect::<Vec<_>>());
        assert_eq!(page_bounds(rows.len(), usize::MAX), (2, 200, 3));
        assert_eq!(page_bounds(0, usize::MAX), (0, 0, 1));

        let filter = QueueFilter {
            q: "KSEA & KPDX".into(),
            show: QueueShow::Live,
            sort: QueueSort::CreatedAsc,
            page: 0,
        };
        let url = queue_url(&filter, 1);
        let Query(parsed) = Query::<QueueFilter>::try_from_uri(&url.parse().unwrap()).unwrap();
        assert_eq!(parsed.q, filter.q);
        assert_eq!(parsed.show, filter.show);
        assert_eq!(parsed.sort, filter.sort);
        assert_eq!(parsed.page, 1);
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
    fn finished_and_paid_views_distinguish_results_cleanup_and_receipts() {
        let now = OffsetDateTime::now_utc();
        let mut c = competition(now);
        c.completed_at = Some(now);
        c.total_paid_out_entries = 1;
        assert!(QueueShow::Finished.includes(&c, None, now, None));
        assert!(!QueueShow::PaidOut.includes(&c, None, now, None));
        let partial = OperatorPayoutProgress {
            paid: 1,
            expected: 3,
        };
        assert!(!QueueShow::PaidOut.includes(&c, None, now, Some(&partial)));

        c.completed_at = None;
        c.attestation = Some(dlctix::secp::Scalar::one().into());
        assert!(QueueShow::Finished.includes(&c, None, now, None));
        let paid = OperatorPayoutProgress {
            paid: 3,
            expected: 3,
        };
        assert!(QueueShow::PaidOut.includes(&c, None, now, Some(&paid)));
        assert!(!QueueShow::Live.includes(&c, None, now, Some(&paid)));

        for (query, expected) in [
            ("finished", QueueShow::Finished),
            ("paid_out", QueueShow::PaidOut),
        ] {
            let url = format!("/admin/operations?show={query}");
            let Query(filter) = Query::<QueueFilter>::try_from_uri(&url.parse().unwrap()).unwrap();
            assert_eq!(filter.show, expected);
            assert_eq!(filter.sort, QueueSort::CreatedDesc);
        }
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
