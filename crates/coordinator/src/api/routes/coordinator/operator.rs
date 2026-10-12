//! Competition views for the operator listener's scripts and command line
//! (`coordinator admin`): what state each competition is in, how far it has settled, the errors
//! it kept, and how far its escrow refunds have got; writing off escrow refunds that can never
//! finish; the Lightning payouts held after a restore; the winners owed because their
//! Lightning payout window closed unpaid; and the recovery records on the Nostr relays.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use log::warn;
use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    api::routes::ApiError,
    domain::{
        Competition, CompetitionError, CompetitionKind, CreateEvent, Error, OwedWinner, PayoutHold,
        QueueSummary, RefundProgress, RefundWriteOff, WriteOffReport, WriteOffTarget,
    },
    infra::refresh_cache::{RefreshCache, RETRY_AFTER},
    metrics,
    startup::AppState,
};

/// How old the inventory may get before a request refreshes it.
const INVENTORY_REFRESH_AFTER: Duration = Duration::from_secs(5);

/// How long a request waits for the refresh it started or joined. Loading every competition with
/// its refunds can take longer than a page would wait, and the callers poll minutes apart, so
/// each poll needs a fresh load: a shorter wait failed polls whose load then succeeded.
const INVENTORY_WAIT: Duration = Duration::from_secs(2);

/// The oldest inventory a request is given, so refund progress is never minutes behind.
const INVENTORY_MAX_AGE: Duration = Duration::from_secs(30);

/// When to ask again after a 503 whose load is still running; that request shares the load.
const INVENTORY_RETRY_SECS: u64 = 2;

/// A competition as the operator sees it: its terms, its state, and its money's progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorCompetition {
    pub id: Uuid,
    /// The state the coordinator derives from the milestones, as in its logs.
    pub state: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// The terms it was created with.
    pub event_submission: CreateEvent,
    pub total_entries: u64,
    pub total_paid_entries: u64,
    pub total_paid_out_entries: u64,
    /// The milestones it has reached on its way to settling, oldest first.
    pub milestones: Vec<Milestone>,
    /// The errors kept on it, oldest first.
    pub errors: Vec<CompetitionError>,
    /// Its funded Arkade escrows and how many have been refunded; None when it has none.
    pub refunds: Option<RefundProgress>,
    /// The escrow refunds an operator wrote off, with their reasons.
    #[serde(default)]
    pub refund_write_offs: Vec<RefundWriteOff>,
    /// `single`, `queued` or `pool`.
    #[serde(default)]
    pub kind: CompetitionKind,
    /// A pool's queued competition.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// A queued competition's settings, entries and pools.
    #[serde(default)]
    pub queue: Option<QueueSummary>,
}

/// A step a competition has reached, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

impl OperatorCompetition {
    pub fn new(
        competition: &Competition,
        refunds: Option<RefundProgress>,
        refund_write_offs: Vec<RefundWriteOff>,
    ) -> Self {
        let c = competition;
        let mut milestones: Vec<Milestone> = [
            ("created", Some(c.created_at)),
            ("pools_formed", c.pools_formed_at),
            ("pools_finished", c.pools_finished_at),
            ("event_created", c.event_created_at),
            ("entries_submitted", c.entries_submitted_at),
            ("escrow_funds_confirmed", c.escrow_funds_confirmed_at),
            ("keymeld_keygen_completed", c.keymeld_keygen_completed_at),
            ("contracted", c.contracted_at),
            ("signed", c.signed_at),
            ("funding_broadcasted", c.funding_broadcasted_at),
            ("funding_confirmed", c.funding_confirmed_at),
            ("invoices_settled", c.invoices_settled_at),
            ("funding_settled", c.funding_settled_at),
            ("awaiting_attestation", c.awaiting_attestation_at),
            ("expiry_broadcasted", c.expiry_broadcasted_at),
            ("outcome_broadcasted", c.outcome_broadcasted_at),
            ("delta_broadcasted", c.delta_broadcasted_at),
            ("completed", c.completed_at),
            ("failed", c.failed_at),
            ("cancelled", c.cancelled_at),
        ]
        .into_iter()
        .filter_map(|(name, at)| {
            at.map(|at| Milestone {
                name: name.to_string(),
                at,
            })
        })
        .collect();
        // Stable, so milestones reached at the same moment keep the order they happen in.
        milestones.sort_by_key(|milestone| milestone.at);
        Self {
            id: c.id,
            state: c.get_state().to_string(),
            created_at: c.created_at,
            event_submission: c.event_submission.clone(),
            total_entries: c.total_entries,
            total_paid_entries: c.total_paid_entries,
            total_paid_out_entries: c.total_paid_out_entries,
            milestones,
            errors: c.errors.clone(),
            refunds,
            refund_write_offs,
            kind: c.kind,
            parent_id: c.parent_id,
            queue: c.queue.clone(),
        }
    }
}

/// A missing competition is a 404, not a database failure.
fn found(id: Uuid, error: Error) -> ApiError {
    match error {
        Error::DbError(sqlx::Error::RowNotFound) => {
            Error::NotFound(format!("competition {id}")).into()
        }
        error => error.into(),
    }
}

/// Every competition, newest first.
pub async fn operator_competitions(State(state): State<Arc<AppState>>) -> Response {
    let coordinator = state.coordinator.clone();
    inventory_response(&state.operator_inventory, move || async move {
        load_operator_inventory(&coordinator).await
    })
    .await
}

/// The cached inventory, refreshed with `load` when due. A request waits at most
/// [`INVENTORY_WAIT`] and is never given an inventory older than [`INVENTORY_MAX_AGE`]; without
/// one it gets a 503 whose `Retry-After` says when to ask again.
async fn inventory_response<F, Fut>(cache: &Arc<RefreshCache<(), Bytes>>, load: F) -> Response
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<Bytes>> + Send + 'static,
{
    let cached = cache
        .get_fresh((), INVENTORY_REFRESH_AFTER, INVENTORY_WAIT, load)
        .await;
    let Some(latest) = cached
        .latest
        .filter(|value| value.age() <= INVENTORY_MAX_AGE)
    else {
        // Still loading, the next request shares the load; otherwise the last load failed, and
        // the next starts once the cache's backoff ends.
        let (reason, retry_after, error) = if cached.refreshing {
            (
                "refreshing",
                INVENTORY_RETRY_SECS,
                "the competition inventory is still loading",
            )
        } else {
            (
                "failed",
                RETRY_AFTER.as_secs(),
                "the competition inventory could not be loaded",
            )
        };
        metrics::OPERATOR_INVENTORY_UNAVAILABLE
            .with_label_values(&[reason])
            .inc();
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                ("retry-after", retry_after.to_string()),
                ("cache-control", "private, no-store".to_owned()),
            ],
            Json(serde_json::json!({ "error": error })),
        )
            .into_response();
    };
    (
        [
            ("content-type", "application/json".to_owned()),
            ("cache-control", "private, no-store".to_owned()),
            (
                "x-inventory-age-seconds",
                latest.age().as_secs().to_string(),
            ),
        ],
        latest.value.clone(),
    )
        .into_response()
}

async fn load_operator_inventory(
    coordinator: &crate::domain::Coordinator,
) -> anyhow::Result<Bytes> {
    let mut timer = LoadTimer::new();
    let loaded = async {
        let mut competitions = coordinator
            .competition_store
            .list_operator_competitions()
            .await?;
        timer.stage("competitions");
        coordinator.attach_queue_details(&mut competitions).await?;
        timer.stage("queues");
        let ids: Vec<_> = competitions
            .iter()
            .map(|competition| competition.id)
            .collect();
        let refunds = coordinator.refund_status(&ids).await?;
        timer.stage("refunds");
        let write_offs = coordinator.refund_write_offs(None).await?;
        timer.stage("write_offs");
        let mut competitions: Vec<_> = competitions
            .iter()
            .map(|c| {
                let written_off = write_offs
                    .iter()
                    .filter(|write_off| write_off.competition_id == c.id)
                    .cloned()
                    .collect();
                OperatorCompetition::new(c, refunds.get(&c.id).copied(), written_off)
            })
            .collect();
        competitions.sort_by_key(|c| std::cmp::Reverse(c.created_at));
        let encoded: Bytes = serde_json::to_vec(&competitions)?.into();
        timer.stage("encode");
        anyhow::Ok(encoded)
    }
    .await;
    timer.finish(loaded.is_ok());
    loaded
}

/// Times the stages of one inventory load, so a load slower than a request waits names the
/// stage that took the time.
struct LoadTimer {
    started: Instant,
    last: Instant,
    stages: Vec<(&'static str, Duration)>,
}

impl LoadTimer {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            stages: Vec::new(),
        }
    }

    fn stage(&mut self, name: &'static str) {
        let now = Instant::now();
        let took = now - self.last;
        self.last = now;
        metrics::OPERATOR_INVENTORY_LOAD_SECONDS
            .with_label_values(&[name])
            .observe(took.as_secs_f64());
        self.stages.push((name, took));
    }

    fn finish(self, ok: bool) {
        let total = self.started.elapsed();
        metrics::OPERATOR_INVENTORY_LOAD_SECONDS
            .with_label_values(&["total"])
            .observe(total.as_secs_f64());
        metrics::OPERATOR_INVENTORY_LOADS
            .with_label_values(&[if ok { "ok" } else { "failed" }])
            .inc();
        if ok {
            metrics::OPERATOR_INVENTORY_LOADED
                .set(OffsetDateTime::now_utc().unix_timestamp() as f64);
        }
        if total >= INVENTORY_WAIT {
            let stages: Vec<String> = self
                .stages
                .iter()
                .map(|(name, took)| format!("{name} {took:.2?}"))
                .collect();
            warn!(
                "Loading the operator's competition inventory took {total:.2?}, longer than a \
                 request waits ({})",
                stages.join(", ")
            );
        }
    }
}

pub async fn operator_competition(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<OperatorCompetition>, ApiError> {
    let competition = state
        .coordinator
        .get_competition(id)
        .await
        .map_err(|e| found(id, e))?;
    let refunds = state.coordinator.refund_status(&[id]).await?;
    let write_offs = state.coordinator.refund_write_offs(Some(id)).await?;
    Ok(Json(OperatorCompetition::new(
        &competition,
        refunds.get(&id).copied(),
        write_offs,
    )))
}

/// Write off escrow refunds that can never finish: one ticket's, or every stuck one of a
/// competition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteOffRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub competition_id: Option<Uuid>,
    /// Kept with each write-off, and shown with it.
    pub reason: String,
    /// Also write off refunds that are not stuck, such as one in progress.
    #[serde(default)]
    pub force: bool,
}

/// Lightning payouts held after a restore, as `coordinator admin payout-holds list` shows them:
/// those not released, or every one with `?all=true`.
pub async fn operator_payout_holds(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<PayoutHoldsQuery>,
) -> Result<Json<Vec<PayoutHold>>, ApiError> {
    Ok(Json(state.coordinator.payout_holds(query.all).await?))
}

#[derive(Debug, Default, Deserialize)]
pub struct PayoutHoldsQuery {
    #[serde(default)]
    pub all: bool,
}

/// Release the holds on an entry's Lightning payout, once an operator found the payment LND made
/// was not this entry's (`coordinator admin payout-holds release`).
pub async fn operator_release_payout_hold(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let released = state.coordinator.release_payout_hold(entry_id).await?;
    if released == 0 {
        return Err(Error::NotFound(format!("no payout hold on entry {entry_id}")).into());
    }
    Ok(Json(
        serde_json::json!({ "entry_id": entry_id, "released": released }),
    ))
}

/// Winners owed because their Lightning payout window closed unpaid, as
/// `coordinator admin owed-winners list` shows them: those still owed, or every one with
/// `?all=true`.
pub async fn operator_owed_winners(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<OwedWinnersQuery>,
) -> Result<Json<Vec<OwedWinner>>, ApiError> {
    Ok(Json(state.coordinator.owed_winners(query.all).await?))
}

#[derive(Debug, Default, Deserialize)]
pub struct OwedWinnersQuery {
    #[serde(default)]
    pub all: bool,
}

/// Approve sweeping an owed winner's split output to the coordinator's key
/// (`coordinator admin owed-winners approve-sweep`). The winner stays owed.
pub async fn operator_approve_owed_winner_sweep(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
) -> Result<Json<OwedWinner>, ApiError> {
    Ok(Json(
        state
            .coordinator
            .approve_owed_winner_sweep(entry_id)
            .await?,
    ))
}

/// How an operator paid an owed winner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettleOwedWinnerRequest {
    /// Kept with the record, such as the Lightning payment hash.
    pub note: String,
}

/// Record that an operator paid an owed winner (`coordinator admin owed-winners settle`), which
/// also approves sweeping their output.
pub async fn operator_settle_owed_winner(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
    Json(request): Json<SettleOwedWinnerRequest>,
) -> Result<Json<OwedWinner>, ApiError> {
    Ok(Json(
        state
            .coordinator
            .settle_owed_winner(entry_id, &request.note)
            .await?,
    ))
}

/// Where the recovery records stand (`coordinator admin recovery status`): each relay with the
/// live records it has not taken, the outbox, and the records by kind. `enabled` is false, and
/// nothing else is filled in, while recovery records are off.
pub async fn operator_recovery_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::domain::recovery::RecoveryStatus>, ApiError> {
    Ok(Json(
        crate::domain::recovery::recovery_status(
            state.recovery.as_deref(),
            &state.coordinator.competition_store,
        )
        .await?,
    ))
}

/// Which relays a republish offers the recovery records to again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepublishRecoveryRequest {
    /// Relays from `[recovery].relays`; none means every one of them.
    #[serde(default)]
    pub relays: Vec<String>,
}

/// Offer every live recovery record again (`coordinator admin recovery republish`), to the
/// relays named or to all of them, at a limited pace: how a relay added to `[recovery].relays`
/// gets the records published before it was.
pub async fn operator_republish_recovery(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RepublishRecoveryRequest>,
) -> Result<Json<crate::domain::recovery::RepublishReport>, ApiError> {
    Ok(Json(
        crate::domain::recovery::republish(
            state.recovery.as_deref(),
            &state.coordinator.competition_store,
            &request.relays,
            OffsetDateTime::now_utc().unix_timestamp(),
        )
        .await?,
    ))
}

/// Write off escrow refunds, as `coordinator admin write-off-refund` does. A ticket's refund that
/// is not stuck is refused unless forced; a competition's are written off where they are stuck,
/// and the rest listed as refused.
pub async fn operator_write_off_refunds(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WriteOffRequest>,
) -> Result<Json<WriteOffReport>, ApiError> {
    let target = match (request.ticket_id, request.competition_id) {
        (Some(ticket_id), None) => WriteOffTarget::Ticket(ticket_id),
        (None, Some(competition_id)) => {
            state
                .coordinator
                .get_competition(competition_id)
                .await
                .map_err(|e| found(competition_id, e))?;
            WriteOffTarget::Competition(competition_id)
        }
        _ => {
            return Err(
                Error::BadRequest("give either a ticket_id or a competition_id".into()).into(),
            )
        }
    };
    Ok(Json(
        state
            .coordinator
            .write_off_refunds(target, &request.reason, request.force)
            .await?,
    ))
}

/// Delete a competition nobody has paid into, as the admin page's delete button does.
pub async fn operator_delete_competition(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state
        .coordinator
        .delete_competition(id)
        .await
        .map_err(|e| found(id, e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;

    /// Ask for the inventory, loading it, when due, in `delay` with the given outcome.
    async fn ask(
        cache: &Arc<RefreshCache<(), Bytes>>,
        loads: &Arc<AtomicUsize>,
        delay: Duration,
        fail: bool,
    ) -> Response {
        let loads = loads.clone();
        inventory_response(cache, move || async move {
            loads.fetch_add(1, Ordering::SeqCst);
            sleep(delay).await;
            anyhow::ensure!(!fail, "the database is unavailable");
            Ok(Bytes::from_static(b"[]"))
        })
        .await
    }

    fn retry_after(response: &Response) -> &str {
        response.headers()["retry-after"].to_str().unwrap()
    }

    /// Synth polls every five minutes, so every poll needs a fresh load. One slower than the old
    /// 250 ms wait was answered 503 at every poll, although each load then succeeded.
    #[tokio::test(start_paused = true)]
    async fn polls_minutes_apart_get_a_load_slower_than_a_page_would_wait() {
        let cache = Arc::new(RefreshCache::new());
        let loads = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let response = ask(&cache, &loads, Duration::from_millis(300), false).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-inventory-age-seconds"], "0");
            sleep(Duration::from_secs(300)).await;
        }
        assert_eq!(loads.load(Ordering::SeqCst), 4);
    }

    /// A load longer than the wait is a bounded 503 that says when to ask again, and the request
    /// that does shares the load rather than starting another.
    #[tokio::test(start_paused = true)]
    async fn a_load_past_the_wait_is_a_503_with_a_retry_hint_the_retry_shares() {
        let cache = Arc::new(RefreshCache::new());
        let loads = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let response = ask(&cache, &loads, Duration::from_secs(3), false).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() >= INVENTORY_WAIT);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(retry_after(&response), INVENTORY_RETRY_SECS.to_string());

        sleep(Duration::from_secs(INVENTORY_RETRY_SECS)).await;
        let response = ask(&cache, &loads, Duration::from_secs(3), false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    /// Refund progress is never served from an inventory older than the limit: a failed refresh
    /// is a 503 until the cache's backoff ends, without another load before then.
    #[tokio::test(start_paused = true)]
    async fn a_failed_refresh_never_serves_an_expired_inventory() {
        let cache = Arc::new(RefreshCache::new());
        let loads = Arc::new(AtomicUsize::new(0));
        let quick = Duration::from_millis(1);
        assert_eq!(
            ask(&cache, &loads, quick, false).await.status(),
            StatusCode::OK
        );
        sleep(Duration::from_secs(300)).await;
        let response = ask(&cache, &loads, quick, true).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry_after(&response), RETRY_AFTER.as_secs().to_string());
        let response = ask(&cache, &loads, quick, true).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }
}
