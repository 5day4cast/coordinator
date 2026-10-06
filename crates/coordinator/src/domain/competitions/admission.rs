//! Entry deadlines are independent of background lifecycle progress.

use super::{Competition, CompetitionKind, CompetitionState, CompetitionStore, Error, Lease};
use crate::config::KickoffCheckSettings;
use crate::infra::db::DatabaseWriteError;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

pub(super) const TICKETS_CLOSED: &str =
    "Ticket requests close one minute before observations start";
pub(super) const ENTRIES_CLOSED: &str = "Competition is no longer accepting entries";
/// Why an entry's picks can no longer change: its competition stopped taking entries. Entries
/// reach the oracle only then: a queued competition's once it forms its pools, a single
/// competition's at the start.
pub const PICKS_LOCKED_CLOSED: &str = "Entries have closed, so these picks are locked";
/// How long after the start a single competition's entries go to the oracle. A picks edit checks
/// the start inside its write, then commits; this gives one that passed the check at the last
/// moment time to commit before the entries are read.
pub(super) const ORACLE_ENTRIES_GRACE: Duration = Duration::seconds(5);
/// The fewest players a single competition's terms allow, as its kickoff check counts them.
pub(super) const SINGLE_COMPETITION_MIN_PLAYERS: u64 = 2;

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

    /// The fewest players it would start with at `sat_per_vb`, while it still takes entries: its
    /// terms' minimum (a queue's smallest pool, or two players for a single competition, as its
    /// kickoff check counts them), raised as the check raises it at that rate. None for a pool,
    /// whose players come from its queue, and once entries have closed.
    pub fn min_players_to_start(
        &self,
        settings: &KickoffCheckSettings,
        sat_per_vb: u64,
        now: OffsetDateTime,
    ) -> Option<u64> {
        if self.kind == CompetitionKind::Pool || self.require_entry_admission(now).is_err() {
            return None;
        }
        let template_min = self
            .queue
            .as_ref()
            .map_or(SINGLE_COMPETITION_MIN_PLAYERS, |queue| {
                queue.pool_rules.min_players() as u64
            });
        Some(settings.min_players_at(template_min, sat_per_vb))
    }

    /// Whether an entry's picks may still change at `now`, and why not when they may not. Picks
    /// are held by the coordinator alone until the entries reach the oracle, which takes them
    /// once and never again; nothing signed or committed names them (the contract, the Keymeld
    /// deposit and the payout policy name the entry id). Entries reach the oracle only once
    /// entries close, so picks change until then:
    ///
    /// - a queued competition sends them when it forms its pools, at or after the start;
    /// - a single competition sends them at the start (see [`Self::oracle_entries_due`]), though
    ///   its contract is built and signed as soon as its seats fill;
    /// - a pool's entries were its queue's, and moved to it after the start.
    pub fn picks_lock(&self, now: OffsetDateTime) -> Option<&'static str> {
        let closed = match self.kind {
            CompetitionKind::Queued => {
                self.require_entry_admission(now).is_err() || self.pools_formed_at.is_some()
            }
            CompetitionKind::Single => {
                now >= self.event_submission.start_observation_date
                    || self.entries_submitted_at.is_some()
                    || self.is_cancelled()
                    || self.is_failed()
            }
            CompetitionKind::Pool => true,
        };
        closed.then_some(PICKS_LOCKED_CLOSED)
    }

    /// When a single competition's entries are due at the oracle: once its entries close at the
    /// start, and its picks with them. None once they are there, and for other kinds: a queued
    /// competition has no oracle event, and a pool sends its entries as it is set up.
    ///
    /// The contract does not wait for this: it names the entries' ids and keys, never their
    /// picks, so it is built and signed as soon as the seats fill. The oracle takes entries
    /// until the end of the observation window, so they must go before then.
    pub fn oracle_entries_due(&self) -> Option<OffsetDateTime> {
        (self.kind == CompetitionKind::Single && self.entries_submitted_at.is_none())
            .then(|| self.event_submission.start_observation_date + ORACLE_ENTRIES_GRACE)
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
