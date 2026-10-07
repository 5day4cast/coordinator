//! Competition views for the operator listener's scripts and command line
//! (`coordinator admin`): what state each competition is in, how far it has settled, the errors
//! it kept, and how far its escrow refunds have got; writing off escrow refunds that can never
//! finish; the Lightning payouts held after a restore; and the winners owed because their
//! Lightning payout window closed unpaid.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    api::routes::ApiError,
    domain::{
        Competition, CompetitionError, CompetitionKind, CreateEvent, Error, OwedWinner, PayoutHold,
        QueueSummary, RefundProgress, RefundWriteOff, WriteOffReport, WriteOffTarget,
    },
    startup::AppState,
};

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
pub async fn operator_competitions(
    State(state): State<Arc<AppState>>,
) -> Result<Response, ApiError> {
    let coordinator = state.coordinator.clone();
    let cached = state
        .operator_inventory
        .get_fresh(
            (),
            Duration::from_secs(5),
            Duration::from_millis(250),
            move || async move { load_operator_inventory(&coordinator).await },
        )
        .await;
    let latest = cached
        .latest
        .filter(|value| value.age() <= Duration::from_secs(30))
        .ok_or(ApiError::Status(StatusCode::SERVICE_UNAVAILABLE))?;
    Ok((
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
        .into_response())
}

async fn load_operator_inventory(
    coordinator: &crate::domain::Coordinator,
) -> anyhow::Result<Bytes> {
    let competitions = coordinator.list_operator_competitions().await?;
    let ids: Vec<_> = competitions
        .iter()
        .map(|competition| competition.id)
        .collect();
    let refunds = coordinator.refund_status(&ids).await?;
    let write_offs = coordinator.refund_write_offs(None).await?;
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
    Ok(serde_json::to_vec(&competitions)?.into())
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
