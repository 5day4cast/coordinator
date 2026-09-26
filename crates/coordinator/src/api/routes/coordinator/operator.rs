//! Competition views for the operator listener's scripts and command line
//! (`coordinator admin`): what state each competition is in, how far it has settled, the errors
//! it kept, and how far its escrow refunds have got.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    api::routes::ApiError,
    domain::{Competition, CompetitionError, CreateEvent, Error, RefundProgress},
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
}

/// A step a competition has reached, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

impl OperatorCompetition {
    pub fn new(competition: &Competition, refunds: Option<RefundProgress>) -> Self {
        let c = competition;
        let mut milestones: Vec<Milestone> = [
            ("created", Some(c.created_at)),
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
) -> Result<Json<Vec<OperatorCompetition>>, ApiError> {
    let competitions = state.coordinator.get_competitions().await?;
    let refunds = state.coordinator.refund_progress(None).await?;
    let mut competitions: Vec<_> = competitions
        .iter()
        .map(|c| OperatorCompetition::new(c, refunds.get(&c.id).copied()))
        .collect();
    competitions.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    Ok(Json(competitions))
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
    let refunds = state.coordinator.refund_progress(Some(id)).await?;
    Ok(Json(OperatorCompetition::new(
        &competition,
        refunds.get(&id).copied(),
    )))
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
