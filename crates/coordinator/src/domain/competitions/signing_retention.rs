//! Retention of each competition's Keymeld signing state: its checkpoint row in
//! `keymeld_protocol_state`, that row's parts and manifest nodes in `keymeld_protocol_parts`,
//! and its refund invoice authorizations. See docs/protocol-checkpoints.md, "Retention".
//!
//! The state is what the coordinator needs to drive Keymeld for a competition: to register its
//! players, sign its contract or an escrow spend, sign a refund, or prepare and release a
//! winner's payout. Once the competition is fully paid out ([`PAID_OUT`]) none of that can be
//! asked for again. Its contract's transactions were signed and are stored with the
//! competition, the coordinator signs its closes and sweeps with its own key and the released
//! entry keys stored with each entry, and the recovery tool needs only the published records,
//! the player's wallet and the chain.
//!
//! The state is kept for `[signing_state].paid_out_retention_days` (3 by default) after the
//! competition was last paid out: after the latest time any of its money moved
//! (`Coordinator::retire_signing_state`). Money that moves again starts the period over. Then
//! the cleanup, which one coordinator runs at a time, deletes it, a few competitions a pass and
//! a batch of rows a write. Nothing new is stored for this: every input is a column the
//! competition, its tickets and its payouts already have.
//!
//! The competition's stored session (`competitions.keymeld_session`) stays. It is small, the
//! contract API reads it, and while it names the session no refund makes a new one.

use super::*;
use crate::{
    config::SigningStateSettings,
    infra::keymeld::DeletedSigningState,
    metrics::{checkpoint_retention, competitions_db_free_bytes},
};
use keymeld_sdk::prelude::SessionId;
use sqlx::Row;
use std::collections::BTreeSet;
use time::format_description::well_known::Rfc3339;

/// Whether competition `c` is fully paid out: nothing it holds can still need a Keymeld
/// signature, and none of its money can still move.
///
/// - Its lifecycle is over. It completed, which `publish_delta2_transactions` does only once
///   every winner's output is settled; or it was cancelled before its contract's funding was
///   ever broadcast; or it is a queued competition whose pools all finished. A queued
///   competition with a pool still running is not.
/// - No contract holds its pot unless it completed. A cancelled competition whose funding was
///   broadcast, or that an Arkade batch funded, may be resumed (`resume_stranded_settlement`,
///   `resume_funded_kickoff`), and a funded pool's kickoff signs its contract again with Keymeld
///   (`resume_ark_kickoff`).
/// - None of its tickets holds money: no held Lightning payment, no on-chain escrow left
///   unreclaimed, no funded Arkade escrow that neither a batch spent into the contract nor a
///   settled refund returned (a written-off refund counts as not returned), and no refund that
///   has not settled. A refund registers every paid player of the competition again, so one
///   open refund keeps the whole session.
/// - None of its entries has a payout job or a Lightning payout under way, a payout an operator
///   holds, or a winner it still owes. A paid payout job releases the winner's key through
///   Keymeld, even after the competition completed.
///
/// So it never holds for a competition that `active_competition_ids` or
/// `get_competitions_pending_cleanup` lists, or one whose recovery records hold money
/// (`recovery_ticket_rows`).
pub(crate) const PAID_OUT: &str = "((c.completed_at IS NOT NULL
       OR (c.cancelled_at IS NOT NULL AND c.funding_broadcasted_at IS NULL
           AND c.funding_confirmed_at IS NULL)
       OR (c.kind = 'queued' AND c.pools_finished_at IS NOT NULL))
     AND NOT (c.kind = 'queued' AND c.pools_formed_at IS NOT NULL
              AND c.pools_finished_at IS NULL)
     AND (c.completed_at IS NOT NULL
          OR NOT EXISTS (SELECT 1 FROM ark_funded_competitions af
                         WHERE af.event_id = c.id AND af.commitment_tx IS NOT NULL))
     AND NOT EXISTS (
         SELECT 1 FROM tickets t
         LEFT JOIN ticket_ark_escrows a ON a.ticket_id = t.id AND a.ticket_hash = t.hash
         LEFT JOIN ticket_ark_refunds r ON r.ticket_id = t.id
         WHERE t.event_id = c.id
           AND ((t.paid_at IS NOT NULL AND t.settled_at IS NULL
                 AND t.invoice_cancelled_at IS NULL)
                OR (t.escrow_transaction IS NOT NULL AND t.escrow_reclaimed_at IS NULL
                    AND (c.funding_broadcasted_at IS NULL OR c.failed_at IS NOT NULL
                         OR c.cancelled_at IS NOT NULL OR c.kind = 'queued'))
                OR (a.funded_at IS NOT NULL AND (r.state IS NULL OR r.state != 'settled')
                    AND NOT EXISTS (SELECT 1 FROM ark_funded_competitions af
                                    WHERE af.event_id = c.id AND af.commitment_tx IS NOT NULL))
                OR (r.state IS NOT NULL AND r.state != 'settled')))
     AND c.id NOT IN (SELECT o.competition_id FROM owed_winners o
                      WHERE o.settled_at IS NULL AND o.claimed_on_chain_at IS NULL
                        AND o.competition_id IS NOT NULL)
     AND c.id NOT IN (SELECT e.event_id FROM payout_jobs j JOIN entries e ON e.id = j.entry_id
                      WHERE j.completed_at IS NULL AND j.failed_at IS NULL
                        AND e.event_id IS NOT NULL)
     AND c.id NOT IN (SELECT e.event_id FROM payouts p JOIN entries e ON e.id = p.entry_id
                      WHERE p.succeed_at IS NULL AND p.failed_at IS NULL
                        AND e.event_id IS NOT NULL)
     AND c.id NOT IN (SELECT e.event_id FROM payout_holds h JOIN entries e ON e.id = h.entry_id
                      WHERE h.released_at IS NULL AND e.event_id IS NOT NULL))";

/// Unix seconds that stand for a time SQLite cannot read: later than any cutoff, so a
/// competition with one is never found paid out long enough.
const UNREADABLE_TIME: i64 = i64::MAX;

/// `column` in Unix seconds, however it is stored: Unix seconds, or text SQLite reads as a time
/// (RFC 3339, or `YYYY-MM-DD HH:MM:SS` with or without fractions and offset). NULL is 0, never
/// the latest; text that is not a time is [`UNREADABLE_TIME`].
fn unix_seconds(column: &str) -> String {
    format!(
        "(CASE WHEN {column} IS NULL THEN 0
               WHEN typeof({column}) IN ('integer', 'real') THEN CAST({column} AS INTEGER)
               ELSE COALESCE(unixepoch({column}), {UNREADABLE_TIME}) END)"
    )
}

/// Competitions whose signing state one pass deletes, at most.
const DELETED_PER_PASS: usize = 2;

/// Parts and manifest nodes one pass deletes, at most. Deleting a part rewrites the page it
/// was on, so the write-ahead log, and what replication uploads, grows by about as much as is
/// deleted. A kickoff's thousands of parts go over a few passes, about a minute apart.
const DELETED_ROWS_PER_PASS: u64 = 4096;

/// Topics of conditions reported once rather than on every pass.
const SESSION_IN_USE: &str = "signing state in use";
const SESSION_UNOWNED: &str = "signing state unowned session";

/// A session with a stored checkpoint, and the competition it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredSigningState {
    pub competition_id: Uuid,
    pub session: SessionId,
}

/// What a retention pass found.
#[derive(Debug, Default)]
pub(crate) struct RetentionSurvey {
    /// Fully paid-out competitions whose signing state waits out the retention period.
    pub waiting: BTreeSet<Uuid>,
    /// Stored signing state due for deletion, with when its competition was last paid out,
    /// longest paid out first.
    pub due: Vec<(i64, StoredSigningState)>,
    /// Competitions whose stored session names a session another competition names too, or
    /// one that is not a session id. Their signing state is kept.
    pub unowned: Vec<Uuid>,
}

impl RetentionSurvey {
    /// The competitions due for deletion.
    pub fn due_competitions(&self) -> BTreeSet<Uuid> {
        self.due
            .iter()
            .map(|(_, state)| state.competition_id)
            .collect()
    }
}

impl CompetitionStore {
    /// Every session with a stored checkpoint row, with the competition it belongs to.
    ///
    /// A single competition's session and a pool's are named by the competition's id. A refund
    /// session that a queued competition, or a pool that never got its own session, made for
    /// its players' deposits has an id of its own, and the competition's stored session names
    /// it. Such a competition never signed a contract, so finding it reads only small rows.
    ///
    /// A session no competition names, or one more than one competition names, or a name that
    /// is not a session id, is left out: it is kept. The second list is the competitions that
    /// name those.
    pub(crate) async fn stored_signing_states(
        &self,
    ) -> Result<(Vec<StoredSigningState>, Vec<Uuid>), sqlx::Error> {
        let mut stored = Vec::new();
        for id in sqlx::query_scalar::<_, String>(
            "SELECT s.session_id FROM keymeld_protocol_state s
             WHERE EXISTS (SELECT 1 FROM competitions c WHERE c.id = s.session_id)",
        )
        .fetch_all(self.db_connection.read())
        .await?
        {
            let competition_id =
                Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            stored.push(StoredSigningState {
                competition_id,
                session: SessionId::from(competition_id),
            });
        }

        // typeof() needs only a row's header, not a signed competition's contract.
        let named = sqlx::query(
            "WITH named(competition_id, session_id) AS (
                 SELECT c.id,
                        CASE WHEN json_valid(CAST(c.keymeld_session AS TEXT))
                             THEN json_extract(CAST(c.keymeld_session AS TEXT), '$.session_id')
                        END
                 FROM competitions c
                 WHERE typeof(c.signed_contract) = 'null' AND typeof(c.keymeld_session) != 'null'
                   AND c.kind != 'single'
             )
             SELECT n.competition_id, n.session_id FROM named n
             JOIN keymeld_protocol_state s ON s.session_id = n.session_id
             WHERE n.session_id != n.competition_id
               AND NOT EXISTS (SELECT 1 FROM competitions o WHERE o.id = n.session_id)
             ORDER BY n.session_id, n.competition_id",
        )
        .fetch_all(self.db_connection.read())
        .await?;
        let mut owners: BTreeMap<String, Vec<Uuid>> = BTreeMap::new();
        for row in &named {
            let competition_id = Uuid::parse_str(&row.try_get::<String, _>("competition_id")?)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            owners
                .entry(row.try_get("session_id")?)
                .or_default()
                .push(competition_id);
        }
        let mut unowned = Vec::new();
        for (session, competitions) in owners {
            let only = match competitions.as_slice() {
                [competition_id] => Some(*competition_id),
                _ => None,
            };
            match (only, SessionId::parse(&session)) {
                (Some(competition_id), Ok(session)) => stored.push(StoredSigningState {
                    competition_id,
                    session,
                }),
                _ => unowned.extend(competitions),
            }
        }
        Ok((stored, unowned))
    }

    /// For each of `competitions` that is fully paid out ([`PAID_OUT`]), when it was last paid
    /// out, in Unix seconds: the latest time any of its money moved. That is the latest of when
    /// it completed, was cancelled or finished its pools, and when each of its tickets settled,
    /// was cancelled or reclaimed, each refund last changed, each owed winner was settled or
    /// claimed, each payout job and payout ended, and each payout hold was released.
    ///
    /// Every one of those is recorded with what it ends, so this holds across restarts, and a
    /// competition whose money moves again is paid out again only from then on.
    pub(crate) async fn paid_out_since(
        &self,
        competitions: &[Uuid],
    ) -> Result<HashMap<Uuid, i64>, sqlx::Error> {
        if competitions.is_empty() {
            return Ok(HashMap::new());
        }
        let ids =
            serde_json::to_string(&competitions.iter().map(Uuid::to_string).collect::<Vec<_>>())
                .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let at = unix_seconds;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "WITH listed(id) AS (SELECT value FROM json_each(?)),
             moved(id, at) AS (
                 SELECT t.event_id, MAX({ticket_settled}, {ticket_cancelled}, {ticket_reclaimed})
                 FROM tickets t WHERE t.event_id IN (SELECT id FROM listed)
                 UNION ALL
                 SELECT t.event_id, {refund}
                 FROM ticket_ark_refunds r JOIN tickets t ON t.id = r.ticket_id
                 WHERE t.event_id IN (SELECT id FROM listed)
                 UNION ALL
                 SELECT o.competition_id, MAX({owed_settled}, {owed_claimed})
                 FROM owed_winners o WHERE o.competition_id IN (SELECT id FROM listed)
                 UNION ALL
                 SELECT e.event_id, MAX({job_completed}, {job_failed})
                 FROM payout_jobs j JOIN entries e ON e.id = j.entry_id
                 WHERE e.event_id IN (SELECT id FROM listed)
                 UNION ALL
                 SELECT e.event_id, MAX({payout_succeeded}, {payout_failed})
                 FROM payouts p JOIN entries e ON e.id = p.entry_id
                 WHERE e.event_id IN (SELECT id FROM listed)
                 UNION ALL
                 SELECT e.event_id, {hold_released}
                 FROM payout_holds h JOIN entries e ON e.id = h.entry_id
                 WHERE e.event_id IN (SELECT id FROM listed)
             ),
             last_moved(id, at) AS MATERIALIZED (SELECT id, MAX(at) FROM moved GROUP BY id)
             SELECT c.id,
                    MAX({completed}, {cancelled}, {pools_finished}, COALESCE(l.at, 0)) AS since
             FROM competitions c LEFT JOIN last_moved l ON l.id = c.id
             WHERE c.id IN (SELECT id FROM listed) AND {PAID_OUT}",
            ticket_settled = at("t.settled_at"),
            ticket_cancelled = at("t.invoice_cancelled_at"),
            ticket_reclaimed = at("t.escrow_reclaimed_at"),
            refund = at("r.updated_at"),
            owed_settled = at("o.settled_at"),
            owed_claimed = at("o.claimed_on_chain_at"),
            job_completed = at("j.completed_at"),
            job_failed = at("j.failed_at"),
            payout_succeeded = at("p.succeed_at"),
            payout_failed = at("p.failed_at"),
            hold_released = at("h.released_at"),
            completed = at("c.completed_at"),
            cancelled = at("c.cancelled_at"),
            pools_finished = at("c.pools_finished_at"),
        )))
        .bind(ids)
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(|row| {
            let id = Uuid::parse_str(&row.try_get::<String, _>("id")?)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok((id, row.try_get::<i64, _>("since")?))
        })
        .collect()
    }

    /// The bytes of the database's free pages: what deletions freed and writes have not
    /// reused yet. SQLite reuses them before it grows the file.
    pub async fn database_free_bytes(&self) -> Result<i64, sqlx::Error> {
        let pages: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(self.db_connection.read())
            .await?;
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(self.db_connection.read())
            .await?;
        Ok(pages.saturating_mul(page_size))
    }
}

impl Coordinator {
    /// Delete the Keymeld signing state of paid-out competitions as `settings` says. Without
    /// this, nothing is deleted.
    pub fn with_signing_state(
        mut self,
        settings: SigningStateSettings,
    ) -> Result<Self, anyhow::Error> {
        settings.validate()?;
        self.signing_state = settings;
        Ok(self)
    }

    /// One retention pass, run with the cleanup, which one coordinator does at a time: find
    /// the stored signing state of fully paid-out competitions, count it, and delete that of
    /// the competitions paid out for longer than the retention period, the longest paid out
    /// first. With deletion off it only counts.
    pub(super) async fn retire_signing_state(&self) -> Result<(), anyhow::Error> {
        self.retire_signing_state_at(OffsetDateTime::now_utc().unix_timestamp())
            .await
    }

    /// As [`Self::retire_signing_state`], at `now` in Unix seconds.
    pub(super) async fn retire_signing_state_at(&self, now: i64) -> Result<(), anyhow::Error> {
        match self.competition_store.database_free_bytes().await {
            Ok(bytes) => competitions_db_free_bytes(bytes),
            Err(error) => debug!("Cannot read the database's free pages: {error}"),
        }
        let cutoff = now.saturating_sub(self.signing_state.grace_secs());
        let survey = self.survey_signing_state(cutoff).await?;
        for competition_id in &survey.unowned {
            if self
                .reported
                .is_new(SESSION_UNOWNED, *competition_id, "unowned")
            {
                warn!(
                    "Competition {competition_id}: its stored Keymeld session is named by another \
                     competition too, or is not a session id, so its signing state is kept"
                );
            }
        }
        checkpoint_retention(survey.waiting.len(), survey.due_competitions().len());
        if !self.signing_state.delete_paid_out {
            return Ok(());
        }
        let mut budget = DELETED_ROWS_PER_PASS;
        for (at, state) in survey.due.into_iter().take(DELETED_PER_PASS) {
            match self.delete_paid_out_state(&state, at, cutoff, budget).await {
                Ok(Some(deleted)) if !deleted.finished => break,
                Ok(Some(deleted)) => budget = budget.saturating_sub(deleted.parts),
                Ok(None) => {}
                Err(error) => warn!(
                    "Competition {}: its Keymeld signing state is not deleted yet: {error:#}",
                    state.competition_id
                ),
            }
            if budget == 0 {
                break;
            }
        }
        Ok(())
    }

    /// The stored signing state of fully paid-out competitions: waiting out the retention
    /// period, or due for deletion, those last paid out at or before `cutoff`.
    pub(crate) async fn survey_signing_state(
        &self,
        cutoff: i64,
    ) -> Result<RetentionSurvey, anyhow::Error> {
        let (stored, unowned) = self.competition_store.stored_signing_states().await?;
        let competitions: Vec<Uuid> = stored
            .iter()
            .map(|state| state.competition_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let since = self.competition_store.paid_out_since(&competitions).await?;
        let mut survey = RetentionSurvey {
            unowned,
            ..RetentionSurvey::default()
        };
        for state in stored {
            let Some(&at) = since.get(&state.competition_id) else {
                continue;
            };
            if at <= cutoff {
                survey.due.push((at, state));
            } else {
                survey.waiting.insert(state.competition_id);
            }
        }
        survey
            .due
            .sort_by_key(|(at, state)| (*at, state.competition_id));
        Ok(survey)
    }

    /// Delete one competition's signing state, at most `budget` parts and nodes of it, holding
    /// its session for the whole deletion. What it deleted, or `None` for a session in use or a
    /// competition whose money moved again since the pass looked: those are left for a later
    /// pass. A deletion that stopped at its budget goes on at the next pass.
    async fn delete_paid_out_state(
        &self,
        state: &StoredSigningState,
        paid_out_since: i64,
        cutoff: i64,
        budget: u64,
    ) -> Result<Option<DeletedSigningState>, anyhow::Error> {
        let competition_id = state.competition_id;
        let Some(hold) = self.keymeld.try_hold_session(&state.session).await else {
            if self
                .reported
                .is_new(SESSION_IN_USE, competition_id, "in use")
            {
                info!(
                    "Competition {competition_id}: its Keymeld session {} is in use, so its \
                     signing state is deleted on a later pass",
                    state.session
                );
            }
            return Ok(None);
        };
        self.reported.clear(SESSION_IN_USE, competition_id);
        // Once held, no operation of this process can use the session; look again in case its
        // money moved since the pass read it.
        if !self
            .competition_store
            .paid_out_since(&[competition_id])
            .await?
            .get(&competition_id)
            .is_some_and(|at| *at <= cutoff)
        {
            debug!(
                "Competition {competition_id}: its signing state is kept, since its money moved \
                 again"
            );
            return Ok(None);
        }
        let deleted = self.keymeld.delete_signing_state(&hold, budget).await?;
        drop(hold);
        if deleted.finished {
            info!(
                "Competition {competition_id}: deleted its Keymeld signing state: {}",
                describe_deletion(&state.session, paid_out_since, &deleted)
            );
        } else {
            info!(
                "Competition {competition_id}: deleting its Keymeld signing state, the rest on \
                 the next pass: {}",
                describe_deletion(&state.session, paid_out_since, &deleted)
            );
        }
        Ok(Some(deleted))
    }
}

/// What a deletion removed, for its log line.
fn describe_deletion(
    session: &SessionId,
    paid_out_since: i64,
    deleted: &DeletedSigningState,
) -> String {
    let since = OffsetDateTime::from_unix_timestamp(paid_out_since)
        .ok()
        .and_then(|at| at.format(&Rfc3339).ok())
        .unwrap_or_else(|| paid_out_since.to_string());
    format!(
        "session {session}, paid out since {since}, {} rows and {} sealed bytes this pass \
         (checkpoint rows {}, parts and manifest nodes {}, refund invoice authorizations {})",
        deleted.total_rows(),
        deleted.total_bytes(),
        deleted.rows,
        deleted.parts,
        deleted.authorizations
    )
}

#[cfg(test)]
#[path = "signing_retention_tests.rs"]
mod tests;
