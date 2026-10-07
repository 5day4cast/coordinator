//! Winners the coordinator owes because their Lightning payout window closed with them unpaid.
//!
//! A winner is paid over Lightning while that is safe: until 13 blocks before they could claim
//! their split output on chain (`payout_watcher::classify_output`). A winner still unpaid then is
//! **owed**, and the settlement step records them here, once.
//!
//! Their split output stays where it is: the coordinator sweeps it to its own key only once an
//! operator approves (`coordinator admin owed-winners approve-sweep`) and the reclaim delay has
//! passed. A swept winner is still owed until an operator records paying them
//! (`coordinator admin owed-winners settle`), which also approves the sweep. A winner who claims
//! the output on chain themselves is no longer owed. The `coordinator_winners_owed` gauge counts
//! the winners still owed. See `docs/ops/owed-winners.md`.

use super::*;
use serde::Deserialize;
use sqlx::Row;
use std::collections::HashSet;
use time::format_description::well_known::Rfc3339;

/// The longest note an operator may keep with a settled winner.
pub const MAX_SETTLED_NOTE_CHARS: usize = 500;

/// A winner the coordinator owes, or owed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwedWinner {
    pub entry_id: Uuid,
    pub competition_id: Uuid,
    /// The winner's share of the pot, in sats, before any sweep's fee.
    pub amount_sats: u64,
    /// When the coordinator found the winner's Lightning payout window closed with them unpaid.
    pub owed_since: String,
    /// The first block at which the coordinator's reclaim path on the winner's output opens.
    pub sweepable_at_height: Option<u32>,
    /// When an operator approved sweeping the winner's output to the coordinator's key.
    pub sweep_approved_at: Option<String>,
    /// When the coordinator broadcast that sweep.
    pub swept_at: Option<String>,
    /// When the winner's output was found spent by the winner's own claim, and in which
    /// transaction.
    pub claimed_on_chain_at: Option<String>,
    pub claim_txid: Option<String>,
    /// When an operator recorded paying the winner, and how.
    pub settled_at: Option<String>,
    pub settled_note: Option<String>,
}

impl OwedWinner {
    /// Whether the coordinator still owes the winner: no operator recorded paying them, and they
    /// did not claim their output on chain.
    pub fn is_owed(&self) -> bool {
        self.settled_at.is_none() && self.claimed_on_chain_at.is_none()
    }

    /// Where the winner's output stands, for the operator.
    pub fn output_status(&self) -> String {
        if let Some(at) = &self.claimed_on_chain_at {
            return format!(
                "Claimed on chain by the winner at {at}{}",
                self.claim_txid
                    .as_deref()
                    .map(|txid| format!(" in {txid}"))
                    .unwrap_or_default()
            );
        }
        if let Some(at) = &self.swept_at {
            return format!("Swept to the coordinator at {at}");
        }
        let from = self
            .sweepable_at_height
            .map(|height| format!(" from block {height}"))
            .unwrap_or_default();
        if self.sweep_approved_at.is_some() {
            format!("Sweep approved; it is broadcast{from}")
        } else {
            format!("Held: sweeping it needs an operator's approval (possible{from})")
        }
    }
}

/// What the metrics report about owed winners.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OwedWinnerCounts {
    /// Winners still owed.
    pub owed: i64,
    /// What they are owed, in sats.
    pub owed_sats: i64,
    /// Owed winners whose output is not swept and whose sweep no operator has approved.
    pub sweeps_held: i64,
}

fn owed_winner_row(row: &sqlx::sqlite::SqliteRow) -> Result<OwedWinner, sqlx::Error> {
    let uuid = |column: &str| -> Result<Uuid, sqlx::Error> {
        Uuid::parse_str(&row.try_get::<String, _>(column)?)
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))
    };
    Ok(OwedWinner {
        entry_id: uuid("entry_id")?,
        competition_id: uuid("competition_id")?,
        amount_sats: u64::try_from(row.try_get::<i64, _>("amount_sats")?).unwrap_or_default(),
        owed_since: row.try_get("owed_since")?,
        sweepable_at_height: row
            .try_get::<Option<i64>, _>("sweepable_at_height")?
            .and_then(|height| u32::try_from(height).ok()),
        sweep_approved_at: row.try_get("sweep_approved_at")?,
        swept_at: row.try_get("swept_at")?,
        claimed_on_chain_at: row.try_get("claimed_on_chain_at")?,
        claim_txid: row.try_get("claim_txid")?,
        settled_at: row.try_get("settled_at")?,
        settled_note: row.try_get("settled_note")?,
    })
}

const OWED_WINNER_COLUMNS: &str = "o.entry_id, o.competition_id, o.amount_sats, o.owed_since,
     o.sweepable_at_height, o.sweep_approved_at, e.reclaimed_broadcasted_at AS swept_at,
     o.claimed_on_chain_at, o.claim_txid, o.settled_at, o.settled_note";

fn now_text() -> Result<String, sqlx::Error> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| sqlx::Error::Encode(Box::new(e)))
}

impl CompetitionStore {
    /// Record that the winner of `entry_id` is owed `amount_sats`, their output sweepable from
    /// `sweepable_at_height`. Returns whether the record is new; an existing one is kept.
    pub async fn record_owed_winner(
        &self,
        entry_id: Uuid,
        competition_id: Uuid,
        amount_sats: u64,
        sweepable_at_height: u32,
    ) -> Result<bool, DatabaseWriteError> {
        let now = now_text()?;
        let amount = i64::try_from(amount_sats).unwrap_or(i64::MAX);
        self.db_connection
            .execute_write(move |pool| async move {
                let inserted = sqlx::query(
                    "INSERT OR IGNORE INTO owed_winners
                         (entry_id, competition_id, amount_sats, owed_since, sweepable_at_height)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(entry_id.to_string())
                .bind(competition_id.to_string())
                .bind(amount)
                .bind(now)
                .bind(i64::from(sweepable_at_height))
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(inserted == 1)
            })
            .await
    }

    /// Owed winners, oldest first: those still owed, or every one with `include_resolved`; of
    /// one competition, or of all.
    pub async fn owed_winners(
        &self,
        include_resolved: bool,
        competition_id: Option<Uuid>,
    ) -> Result<Vec<OwedWinner>, sqlx::Error> {
        let competition = competition_id.map(|id| id.to_string());
        let rows = sqlx::query(&format!(
            "SELECT {OWED_WINNER_COLUMNS}
             FROM owed_winners o LEFT JOIN entries e ON e.id = o.entry_id
             WHERE (? OR (o.settled_at IS NULL AND o.claimed_on_chain_at IS NULL))
               AND (? IS NULL OR o.competition_id = ?)
             ORDER BY o.owed_since, o.entry_id"
        ))
        .bind(include_resolved)
        .bind(competition.clone())
        .bind(competition)
        .fetch_all(self.db_connection.read())
        .await?;
        rows.iter().map(owed_winner_row).collect()
    }

    /// The owed winner of `entry_id`, resolved or not.
    pub async fn owed_winner(&self, entry_id: Uuid) -> Result<Option<OwedWinner>, sqlx::Error> {
        sqlx::query(&format!(
            "SELECT {OWED_WINNER_COLUMNS}
             FROM owed_winners o LEFT JOIN entries e ON e.id = o.entry_id
             WHERE o.entry_id = ?"
        ))
        .bind(entry_id.to_string())
        .fetch_optional(self.db_connection.read())
        .await?
        .as_ref()
        .map(owed_winner_row)
        .transpose()
    }

    /// Approve sweeping an owed winner's output to the coordinator's key. Returns whether this
    /// approved it; one already approved is left as it is.
    pub async fn approve_owed_winner_sweep(
        &self,
        entry_id: Uuid,
    ) -> Result<bool, DatabaseWriteError> {
        let now = now_text()?;
        self.db_connection
            .execute_write(move |pool| async move {
                let changed = sqlx::query(
                    "UPDATE owed_winners SET sweep_approved_at = ?
                     WHERE entry_id = ? AND sweep_approved_at IS NULL
                       AND claimed_on_chain_at IS NULL",
                )
                .bind(now)
                .bind(entry_id.to_string())
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed == 1)
            })
            .await
    }

    /// Record that an operator paid an owed winner, with how; this approves the sweep of their
    /// output too. Returns whether this settled it; one already settled is left as it is.
    pub async fn settle_owed_winner(
        &self,
        entry_id: Uuid,
        note: String,
    ) -> Result<bool, DatabaseWriteError> {
        let now = now_text()?;
        self.db_connection
            .execute_write(move |pool| async move {
                let changed = sqlx::query(
                    "UPDATE owed_winners
                     SET settled_at = ?, settled_note = ?,
                         sweep_approved_at = CASE WHEN claimed_on_chain_at IS NULL
                                                  THEN COALESCE(sweep_approved_at, ?)
                                                  ELSE sweep_approved_at END
                     WHERE entry_id = ? AND settled_at IS NULL",
                )
                .bind(now.clone())
                .bind(note)
                .bind(now)
                .bind(entry_id.to_string())
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed == 1)
            })
            .await
    }

    /// Record that an owed winner claimed their output on chain in `txid`.
    pub(super) async fn mark_owed_winner_claimed(
        &self,
        entry_id: Uuid,
        txid: String,
    ) -> Result<bool, DatabaseWriteError> {
        let now = now_text()?;
        self.db_connection
            .execute_write(move |pool| async move {
                let changed = sqlx::query(
                    "UPDATE owed_winners SET claimed_on_chain_at = ?, claim_txid = ?
                     WHERE entry_id = ? AND claimed_on_chain_at IS NULL",
                )
                .bind(now)
                .bind(txid)
                .bind(entry_id.to_string())
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed == 1)
            })
            .await
    }

    /// Record that the entry's split output was found spent by a transaction the coordinator has
    /// no record of making. The first time is kept.
    pub async fn mark_entry_split_output_spent(
        &self,
        entry_id: Uuid,
        spent_at: OffsetDateTime,
    ) -> Result<bool, DatabaseWriteError> {
        let spent_at = spent_at
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        self.db_connection
            .execute_write(move |pool| async move {
                let changed = sqlx::query(
                    "UPDATE entries SET split_output_spent_at = ?
                     WHERE id = ? AND split_output_spent_at IS NULL",
                )
                .bind(spent_at)
                .bind(entry_id.to_string())
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed > 0)
            })
            .await
    }

    /// Owed winners for the metrics.
    pub async fn owed_winner_counts(&self) -> Result<OwedWinnerCounts, sqlx::Error> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS owed, COALESCE(SUM(o.amount_sats), 0) AS owed_sats,
                    COALESCE(SUM(o.sweep_approved_at IS NULL
                                 AND e.reclaimed_broadcasted_at IS NULL), 0) AS held
             FROM owed_winners o LEFT JOIN entries e ON e.id = o.entry_id
             WHERE o.settled_at IS NULL AND o.claimed_on_chain_at IS NULL",
        )
        .fetch_one(self.db_connection.read())
        .await?;
        Ok(OwedWinnerCounts {
            owed: row.try_get("owed")?,
            owed_sats: row.try_get("owed_sats")?,
            sweeps_held: row.try_get("held")?,
        })
    }

    /// The entries of a competition whose Lightning payout succeeded.
    pub async fn paid_out_entries(&self, event_id: Uuid) -> Result<HashSet<Uuid>, sqlx::Error> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT p.entry_id FROM payouts p JOIN entries e ON e.id = p.entry_id
             WHERE e.event_id = ? AND p.succeed_at IS NOT NULL",
        )
        .bind(event_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?;
        ids.iter()
            .map(|id| Uuid::parse_str(id).map_err(|e| sqlx::Error::Decode(Box::new(e))))
            .collect()
    }

    /// Whether the competition has a Lightning payout window that is still open. A competition
    /// without automatic payouts has none.
    pub async fn payout_window_is_open(&self, event_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM automatic_payout_competitions
                           WHERE event_id = ? AND payout_window_closed_at IS NULL)",
        )
        .bind(event_id.to_string())
        .fetch_one(self.db_connection.read())
        .await
    }

    /// Open a closed Lightning payout window again. Returns whether it was closed. Each payout
    /// is still checked against the winners' earliest on-chain claim before it is sent.
    pub async fn reopen_payout_window(&self, event_id: Uuid) -> Result<bool, DatabaseWriteError> {
        if !self.payout_window_is_closed(event_id).await? {
            return Ok(false);
        }
        self.db_connection
            .execute_write(move |pool| async move {
                let changed = sqlx::query(
                    "UPDATE automatic_payout_competitions SET payout_window_closed_at = NULL
                     WHERE event_id = ? AND payout_window_closed_at IS NOT NULL",
                )
                .bind(event_id.to_string())
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed > 0)
            })
            .await
    }
}

impl Coordinator {
    /// Owed winners, oldest first: those still owed, or every one with `include_resolved`.
    pub async fn owed_winners(&self, include_resolved: bool) -> Result<Vec<OwedWinner>, Error> {
        Ok(self
            .competition_store
            .owed_winners(include_resolved, None)
            .await?)
    }

    /// Every owed winner of a competition, resolved or not.
    pub async fn competition_owed_winners(
        &self,
        competition_id: Uuid,
    ) -> Result<Vec<OwedWinner>, Error> {
        Ok(self
            .competition_store
            .owed_winners(true, Some(competition_id))
            .await?)
    }

    async fn existing_owed_winner(&self, entry_id: Uuid) -> Result<OwedWinner, Error> {
        self.competition_store
            .owed_winner(entry_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("no owed winner for entry {entry_id}")))
    }

    /// Approve sweeping an owed winner's split output to the coordinator's key, once its reclaim
    /// delay has passed. The winner stays owed until an operator records paying them.
    pub async fn approve_owed_winner_sweep(&self, entry_id: Uuid) -> Result<OwedWinner, Error> {
        let owed = self.existing_owed_winner(entry_id).await?;
        if owed.claimed_on_chain_at.is_some() {
            return Err(Error::BadRequest(format!(
                "the winner of entry {entry_id} claimed their output on chain; there is nothing to \
                 sweep"
            )));
        }
        if self
            .competition_store
            .approve_owed_winner_sweep(entry_id)
            .await?
        {
            warn!(
                "An operator approved sweeping the split output of owed winner entry {entry_id} \
                 to the coordinator"
            );
        }
        self.wake_competition(owed.competition_id);
        self.existing_owed_winner(entry_id).await
    }

    /// Record that an operator paid an owed winner, with `note` saying how, such as the payment
    /// hash. This also approves sweeping their output, which the coordinator then holds.
    pub async fn settle_owed_winner(
        &self,
        entry_id: Uuid,
        note: &str,
    ) -> Result<OwedWinner, Error> {
        let note = note.trim();
        if note.is_empty() || note.chars().count() > MAX_SETTLED_NOTE_CHARS {
            return Err(Error::BadRequest(format!(
                "say how the winner was paid, in 1 to {MAX_SETTLED_NOTE_CHARS} characters"
            )));
        }
        let owed = self.existing_owed_winner(entry_id).await?;
        if self
            .competition_store
            .settle_owed_winner(entry_id, note.to_owned())
            .await?
        {
            info!("An operator recorded paying owed winner entry {entry_id}: {note}");
        }
        self.wake_competition(owed.competition_id);
        self.existing_owed_winner(entry_id).await
    }
}
