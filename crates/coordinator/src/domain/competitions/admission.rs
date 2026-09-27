//! Entry deadlines are independent of background lifecycle progress.

use super::{Competition, CompetitionState, CompetitionStore, Error, Lease};
use crate::infra::db::DatabaseWriteError;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

pub(super) const TICKETS_CLOSED: &str =
    "Ticket requests close one minute before observations start";
pub(super) const ENTRIES_CLOSED: &str = "Competition is no longer accepting entries";

pub(super) fn entry_write_error(error: DatabaseWriteError, ticket_has_entry: bool) -> Error {
    match error {
        // A concurrent retry can pass the earlier unused-ticket check. Only
        // classify a uniqueness failure as that retry after confirming the
        // ticket was consumed; other storage failures remain server errors.
        DatabaseWriteError::Sqlx(sqlx::Error::Database(error))
            if ticket_has_entry && error.is_unique_violation() =>
        {
            Error::BadRequest("Ticket has already been used".into())
        }
        DatabaseWriteError::Sqlx(sqlx::Error::RowNotFound) => Error::BadRequest(
            "Failed to claim ticket - may have expired or been claimed by another entry".into(),
        ),
        error => Error::from(error),
    }
}

pub(super) fn before_deadline(deadline: Option<OffsetDateTime>) -> bool {
    deadline.is_none_or(|deadline| OffsetDateTime::now_utc() < deadline)
}

impl Competition {
    pub(super) fn unfilled_admission_expired(&self, now: OffsetDateTime) -> bool {
        matches!(self.get_state(), CompetitionState::Created)
            && !self.has_full_entries()
            && now >= self.event_submission.start_observation_date
    }

    pub(super) fn ticket_deadline(&self) -> OffsetDateTime {
        self.event_submission.start_observation_date - Duration::minutes(1)
    }

    pub(super) fn require_ticket_admission(&self, now: OffsetDateTime) -> Result<(), Error> {
        self.require_entry_admission(now)?;
        if now >= self.ticket_deadline() {
            return Err(Error::BadRequest(TICKETS_CLOSED.into()));
        }
        Ok(())
    }

    pub(super) fn require_entry_admission(&self, now: OffsetDateTime) -> Result<(), Error> {
        if now >= self.event_submission.start_observation_date
            || !matches!(self.get_state(), CompetitionState::Created)
        {
            return Err(Error::BadRequest(ENTRIES_CLOSED.into()));
        }
        Ok(())
    }
}

impl CompetitionStore {
    /// Close an unfilled roster under the worker lease. Count entries in the
    /// serialized write, since the worker's snapshot can predate the final entry.
    /// A queued competition has no roster to fill: its kickoff decides at the start.
    pub(super) async fn cancel_unfilled_at_deadline(
        &self,
        competition: &Competition,
        lease: &Lease,
    ) -> Result<bool, DatabaseWriteError> {
        let id = competition.id.to_string();
        let capacity = competition.event_submission.total_allowed_entries as i64;
        let deadline = competition.event_submission.start_observation_date;
        let lease = lease.clone();
        self.db_connection
            .execute_write(move |pool| async move {
                let now = OffsetDateTime::now_utc();
                if now < deadline {
                    return Ok(false);
                }
                let cancelled_at = now.format(&Rfc3339)
                    .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
                let changed = sqlx::query(
                    "UPDATE competitions SET cancelled_at = ?
                     WHERE id = ? AND kind != 'queued'
                       AND cancelled_at IS NULL AND failed_at IS NULL AND completed_at IS NULL
                       AND escrow_funds_confirmed_at IS NULL AND event_created_at IS NULL
                       AND entries_submitted_at IS NULL AND contract_parameters IS NULL
                       AND keymeld_keygen_completed_at IS NULL AND signed_at IS NULL
                       AND funding_broadcasted_at IS NULL AND funding_confirmed_at IS NULL
                       AND funding_settled_at IS NULL AND awaiting_attestation_at IS NULL
                       AND attestation IS NULL AND expiry_broadcasted_at IS NULL
                       AND outcome_broadcasted_at IS NULL AND delta_broadcasted_at IS NULL
                       AND (SELECT count(*) FROM entries WHERE event_id = competitions.id) < ?
                       AND EXISTS (SELECT 1 FROM leases WHERE resource = ? AND holder = ? AND token = ?)",
                )
                .bind(cancelled_at)
                .bind(id)
                .bind(capacity)
                .bind(lease.resource)
                .bind(lease.holder)
                .bind(lease.token)
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed == 1)
            })
            .await
    }
}
