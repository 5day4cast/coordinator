//! What the recovery records read, and the outbox they wait in until the relays take them.
//!
//! The records are rebuilt from these rows whenever a competition changes; see
//! `domain::recovery` and docs/RECOVERY.md.

use super::{ticket_preimage::stored_preimage, CompetitionStore, TicketCipher};
use crate::infra::db::DatabaseWriteError;
use sqlx::{sqlite::SqliteRow, Row};
use std::collections::HashMap;
use uuid::Uuid;

/// Outbox rows holding something not every relay took yet: a record's current version, or the
/// deletion of a retired record.
const PENDING: &str = "((deleted_at IS NULL AND published_at IS NULL)
     OR (deleted_at IS NOT NULL AND deletion_published_at IS NULL))";

/// A competition, as `c`, that may still move a player's money: the test
/// `CompetitionStore::active_competition_ids` makes.
const COMPETITION_ACTIVE: &str = "(c.completed_at IS NULL
     AND (c.cancelled_at IS NULL OR c.funding_confirmed_at IS NOT NULL
          OR (c.funding_broadcasted_at IS NOT NULL
              AND EXISTS (SELECT 1 FROM ark_funded_competitions af
                          WHERE af.event_id = c.id AND af.commitment_tx IS NOT NULL)))
     AND NOT (c.kind = 'queued' AND c.pools_formed_at IS NOT NULL))";

/// One ticket of a competition, with what its player's recovery record needs.
///
/// Every ticket an entry used is listed. A ticket no entry used yet is listed only once its
/// player fixed a payout policy for it, which names the entry id the entry key comes from.
#[derive(Debug, Clone, Default)]
pub struct RecoveryTicketRow {
    pub ticket_id: Uuid,
    pub ticket_hash: String,
    /// The ticket's preimage, hex, once its payment settled and so reached the player.
    pub ticket_preimage: Option<String>,
    /// When the ticket was reserved, in Unix seconds.
    pub reserved_at: Option<i64>,
    /// Hex Nostr pubkey of the player holding the reservation.
    pub reserved_by: Option<String>,
    /// The ticket's payment is held, or for an Arkade ticket its escrow is funded.
    pub paid: bool,
    /// The payment settled, so its preimage reached the player.
    pub settled: bool,
    pub entry_id: Option<Uuid>,
    /// Hex Nostr pubkey of the entry's player.
    pub entry_user: Option<String>,
    /// Compressed hex entry key of the entry.
    pub entry_pubkey: Option<String>,
    /// Compressed hex entry key from the ticket's payout policy.
    pub policy_entry_pubkey: Option<String>,
    pub policy_json: Option<String>,
    /// The ticket's Arkade escrow: its PSBT `TapTree` field, hex.
    pub escrow_tap_tree: Option<String>,
    /// The funded escrow VTXO, `txid:vout`.
    pub vtxo_outpoint: Option<String>,
    pub vtxo_sats: Option<u64>,
    /// The escrow's refund state, when a refund was started.
    pub refund_state: Option<String>,
    /// A Lightning payout to the entry succeeded.
    pub paid_out: bool,
    /// The coordinator bought the entry's win back on chain, or reclaimed it.
    pub closed_on_chain: bool,
    /// Some of the player's money may still move: the competition is not over, a payment is
    /// held, an escrow is neither spent into the contract nor refunded (a written-off refund
    /// leaves the escrow to the player), the winner is still owed, or a payout is held for an
    /// operator. While it is, the record is kept on the relays.
    pub money_held: bool,
}

/// A competition's stored contract, read as the JSON it is kept in. Parsing the signed contract
/// into dlctix's type would rebuild every transaction of the contract, which recovery records
/// never need.
#[derive(Debug, Clone, Default)]
pub struct RecoveryCompetitionRow {
    pub id: Uuid,
    pub event_submission: Vec<u8>,
    pub event_announcement: Option<Vec<u8>>,
    pub contract_parameters: Option<Vec<u8>>,
    pub signed_contract: Option<Vec<u8>>,
    pub funding_outpoint: Option<Vec<u8>>,
    pub funding_transaction: Option<Vec<u8>>,
    pub attestation: Option<Vec<u8>>,
    pub expiry_broadcasted: bool,
    /// The competition may still move money: not completed, and not cancelled before its
    /// contract was funded.
    pub active: bool,
}

/// What the outbox holds for one event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryOutboxState {
    /// `wallet`, `entry` or `competition`.
    pub kind: String,
    /// The digest of the record's plaintext.
    pub content_sha256: String,
    pub created_at: i64,
    /// When the publisher first found the record's money settled.
    pub settled_at: Option<i64>,
    /// When the record was retired by a deletion.
    pub deleted_at: Option<i64>,
}

impl RecoveryOutboxState {
    /// The latest `created_at` of anything published under this `d` tag, which a new version
    /// must follow: its last version, or the deletion that retired it.
    pub fn latest(&self) -> i64 {
        self.created_at.max(self.deleted_at.unwrap_or(i64::MIN))
    }
}

/// A record's new version, signed and ready to publish.
#[derive(Debug, Clone)]
pub struct RecoveryOutboxEvent {
    pub d_tag: String,
    /// `wallet`, `entry` or `competition`.
    pub kind: &'static str,
    /// Hex Nostr pubkey of the player, for wallet and entry records.
    pub user_pubkey: Option<String>,
    pub competition_id: Option<Uuid>,
    pub content_sha256: String,
    pub event_json: String,
    pub created_at: i64,
}

/// An outbox event due for publishing: a record's version, or a retired record's deletion.
#[derive(Debug, Clone)]
pub struct RecoveryDueEvent {
    pub d_tag: String,
    pub kind: String,
    pub content_sha256: String,
    pub event_json: String,
    pub attempts: u32,
    pub accepted_relays: Vec<String>,
    /// For a deletion, its `created_at`.
    pub deletion: Option<i64>,
}

/// What one publishing attempt of an event achieved.
#[derive(Debug, Clone)]
pub struct RecoveryAttempt {
    pub d_tag: String,
    /// The version attempted. A newer version written meanwhile is left due.
    pub content_sha256: String,
    /// The deletion attempted, by its `created_at`; `None` for a record's version.
    pub deletion: Option<i64>,
    /// The attempts the event had and the relays that had taken it when it was read. An event
    /// re-queued meanwhile is left as the re-queue left it.
    pub attempts: u32,
    pub accepted_before: Vec<String>,
    pub accepted_relays: Vec<String>,
    /// Set when no relay is left to try.
    pub published_at: Option<i64>,
    pub next_attempt_at: i64,
    pub error: Option<String>,
}

/// A settled record whose grace period is over.
#[derive(Debug, Clone)]
pub struct RecoveryRetirement {
    pub d_tag: String,
    pub kind: String,
    pub content_sha256: String,
    pub event_json: String,
    pub created_at: i64,
}

/// The deletion that retires a record.
#[derive(Debug, Clone)]
pub struct RecoveryDeletion {
    pub d_tag: String,
    /// The version retired, as read; a record changed meanwhile is not retired.
    pub content_sha256: String,
    pub created_at: i64,
    pub deletion_json: String,
    /// The deletion's own `created_at`.
    pub deleted_at: i64,
}

/// The outbox's records of one kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecoveryKindCount {
    /// `wallet`, `entry` or `competition`.
    pub kind: String,
    /// Records not retired: kept on the relays.
    pub live: i64,
    /// Live records whose money is settled, waiting out the grace period.
    pub settled: i64,
    /// Records retired by a deletion.
    pub retired: i64,
}

fn uuid(value: String) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(&value).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

fn ticket_row(
    row: &SqliteRow,
    cipher: Option<&TicketCipher>,
) -> Result<RecoveryTicketRow, sqlx::Error> {
    let ticket_id = uuid(row.try_get("ticket_id")?)?;
    let ticket_hash: String = row.try_get("ticket_hash")?;
    let settled: bool = row.try_get("settled")?;
    let ticket_preimage = if settled {
        let ciphertext: Option<Vec<u8>> = row.try_get("ticket_preimage_ciphertext")?;
        let legacy: String = row.try_get("ticket_preimage")?;
        let preimage = stored_preimage(
            cipher,
            ticket_id,
            &ticket_hash,
            ciphertext.as_deref(),
            &legacy,
        )
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        Some(hex::encode(preimage))
    } else {
        None
    };
    Ok(RecoveryTicketRow {
        ticket_id,
        ticket_hash,
        ticket_preimage,
        reserved_at: row.try_get("reserved_at")?,
        reserved_by: row.try_get("reserved_by")?,
        paid: row.try_get("paid")?,
        settled,
        entry_id: row
            .try_get::<Option<String>, _>("entry_id")?
            .map(uuid)
            .transpose()?,
        entry_user: row.try_get("entry_user")?,
        entry_pubkey: row.try_get("entry_pubkey")?,
        policy_entry_pubkey: row.try_get("policy_entry_pubkey")?,
        policy_json: row.try_get("policy_json")?,
        escrow_tap_tree: row.try_get("escrow_tap_tree")?,
        vtxo_outpoint: row.try_get("vtxo_outpoint")?,
        vtxo_sats: row
            .try_get::<Option<i64>, _>("vtxo_sats")?
            .map(|sats| sats as u64),
        refund_state: row.try_get("refund_state")?,
        paid_out: row.try_get("paid_out")?,
        closed_on_chain: row.try_get("closed_on_chain")?,
        money_held: row.try_get("money_held")?,
    })
}

fn outbox_state(row: &SqliteRow) -> Result<(String, RecoveryOutboxState), sqlx::Error> {
    Ok((
        row.try_get("d_tag")?,
        RecoveryOutboxState {
            kind: row.try_get("kind")?,
            content_sha256: row.try_get("content_sha256")?,
            created_at: row.try_get("created_at")?,
            settled_at: row.try_get("settled_at")?,
            deleted_at: row.try_get("deleted_at")?,
        },
    ))
}

impl CompetitionStore {
    /// Every ticket of a competition that a recovery record describes.
    pub async fn recovery_ticket_rows(
        &self,
        competition_id: Uuid,
    ) -> Result<Vec<RecoveryTicketRow>, sqlx::Error> {
        sqlx::query(&format!(
            "SELECT t.id AS ticket_id, t.hash AS ticket_hash, t.encrypted_preimage AS ticket_preimage,
                    t.preimage_ciphertext AS ticket_preimage_ciphertext,
                    unixepoch(t.reserved_at) AS reserved_at, t.reserved_by,
                    t.paid_at IS NOT NULL AS paid, t.settled_at IS NOT NULL AS settled,
                    e.id AS entry_id, e.pubkey AS entry_user, e.ephemeral_pubkey AS entry_pubkey,
                    p.entry_pubkey AS policy_entry_pubkey, p.policy_json,
                    a.escrow_tap_tree, a.vtxo_outpoint, a.vtxo_sats, r.state AS refund_state,
                    EXISTS(SELECT 1 FROM payouts WHERE payouts.entry_id = e.id AND payouts.succeed_at IS NOT NULL) AS paid_out,
                    (e.sellback_broadcasted_at IS NOT NULL OR e.reclaimed_broadcasted_at IS NOT NULL) AS closed_on_chain,
                    COALESCE({COMPETITION_ACTIVE}
                     OR (t.paid_at IS NOT NULL AND t.settled_at IS NULL AND t.invoice_cancelled_at IS NULL)
                     OR (t.escrow_transaction IS NOT NULL AND t.escrow_reclaimed_at IS NULL
                         AND c.funding_broadcasted_at IS NULL)
                     OR (a.funded_at IS NOT NULL AND (r.state IS NULL OR r.state != 'settled')
                         AND NOT EXISTS (SELECT 1 FROM ark_funded_competitions af
                                         WHERE af.event_id = c.id AND af.commitment_tx IS NOT NULL))
                     OR EXISTS (SELECT 1 FROM owed_winners o WHERE o.entry_id = e.id
                                AND o.settled_at IS NULL AND o.claimed_on_chain_at IS NULL)
                     OR EXISTS (SELECT 1 FROM payout_holds h WHERE h.entry_id = e.id
                                AND h.released_at IS NULL), TRUE) AS money_held
             FROM tickets t
             JOIN competitions c ON c.id = t.event_id
             LEFT JOIN entries e ON e.ticket_id = t.id
             LEFT JOIN ticket_payout_policies p ON p.ticket_id = t.id AND p.ticket_hash = t.hash
             LEFT JOIN ticket_ark_escrows a ON a.ticket_id = t.id AND a.ticket_hash = t.hash
             LEFT JOIN ticket_ark_refunds r ON r.ticket_id = t.id
             WHERE t.event_id = ? AND (e.id IS NOT NULL OR p.ticket_id IS NOT NULL)
             ORDER BY t.id"
        ))
        .bind(competition_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(|row| ticket_row(row, self.ticket_cipher.as_deref()))
        .collect()
    }

    pub async fn recovery_competition(
        &self,
        competition_id: Uuid,
    ) -> Result<Option<RecoveryCompetitionRow>, sqlx::Error> {
        let Some(row) = sqlx::query(&format!(
            "SELECT c.id, c.event_submission, c.event_announcement, c.contract_parameters,
                    c.signed_contract, c.funding_outpoint, c.funding_transaction, c.attestation,
                    c.expiry_broadcasted_at IS NOT NULL AS expiry_broadcasted,
                    COALESCE({COMPETITION_ACTIVE}, TRUE) AS active
             FROM competitions c WHERE c.id = ?"
        ))
        .bind(competition_id.to_string())
        .fetch_optional(self.db_connection.read())
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(RecoveryCompetitionRow {
            id: uuid(row.try_get("id")?)?,
            event_submission: row.try_get("event_submission")?,
            event_announcement: row.try_get("event_announcement")?,
            contract_parameters: row.try_get("contract_parameters")?,
            signed_contract: row.try_get("signed_contract")?,
            funding_outpoint: row.try_get("funding_outpoint")?,
            funding_transaction: row.try_get("funding_transaction")?,
            attestation: row.try_get("attestation")?,
            expiry_broadcasted: row.try_get("expiry_broadcasted")?,
            active: row.try_get("active")?,
        }))
    }

    /// Competitions whose entries, tickets or payouts changed after `since` (less a few seconds,
    /// for writes that committed late), with the latest change seen. The list triggers bump the
    /// competition for every change to its entries, tickets and payouts.
    pub async fn recovery_changed_competitions(
        &self,
        since: &str,
    ) -> Result<(Vec<Uuid>, Option<String>), sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, updated_at FROM list_updates
             WHERE kind = 'competition' AND updated_at > strftime('%Y-%m-%dT%H:%M:%fZ', ?, '-5 seconds')
             ORDER BY updated_at LIMIT 500",
        )
        .bind(since)
        .fetch_all(self.db_connection.read())
        .await?;
        let mut latest = None;
        let mut ids = Vec::with_capacity(rows.len());
        for row in &rows {
            ids.push(uuid(row.try_get("id")?)?);
            latest = Some(row.try_get::<String, _>("updated_at")?);
        }
        Ok((ids, latest))
    }

    /// The database's clock in the list triggers' format, where a change scan starts.
    pub async fn recovery_clock(&self) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')")
            .fetch_one(self.db_connection.read())
            .await
    }

    /// The competitions a full pass looks at: every one not completed, which may still hold a
    /// player's money, newest first; then completed ones with records on the relays whose money
    /// was not found settled yet (including competitions no longer in the database), and those
    /// with a winner still owed or a payout held, whose settled records come back. Competitions
    /// whose records are all settled are left to the change scan.
    pub async fn recovery_pass_competitions(&self) -> Result<Vec<Uuid>, sqlx::Error> {
        let mut ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM competitions WHERE completed_at IS NULL ORDER BY created_at DESC",
        )
        .fetch_all(self.db_connection.read())
        .await?;
        let rest: Vec<String> = sqlx::query_scalar(
            "SELECT competition_id FROM (
                 SELECT o.competition_id FROM recovery_outbox o
                 WHERE o.competition_id IS NOT NULL AND o.deleted_at IS NULL
                   AND o.settled_at IS NULL
                 UNION
                 SELECT ow.competition_id FROM owed_winners ow
                 WHERE ow.settled_at IS NULL AND ow.claimed_on_chain_at IS NULL
                 UNION
                 SELECT e.event_id FROM payout_holds h JOIN entries e ON e.id = h.entry_id
                 WHERE h.released_at IS NULL
             ) AS kept
             WHERE NOT EXISTS (SELECT 1 FROM competitions c
                               WHERE c.id = kept.competition_id AND c.completed_at IS NULL)
             ORDER BY competition_id",
        )
        .fetch_all(self.db_connection.read())
        .await?;
        ids.extend(rest);
        ids.into_iter().map(uuid).collect()
    }

    /// The outbox's current version of each event of one competition, or of every wallet
    /// record when `competition_id` is `None`.
    pub async fn recovery_outbox_states(
        &self,
        competition_id: Option<Uuid>,
    ) -> Result<HashMap<String, RecoveryOutboxState>, sqlx::Error> {
        const COLUMNS: &str = "d_tag, kind, content_sha256, created_at, settled_at, deleted_at";
        let sql = match competition_id {
            Some(_) => format!("SELECT {COLUMNS} FROM recovery_outbox WHERE competition_id = ?"),
            None => format!("SELECT {COLUMNS} FROM recovery_outbox WHERE kind = 'wallet'"),
        };
        let mut query = sqlx::query(&sql);
        if let Some(id) = competition_id {
            query = query.bind(id.to_string());
        }
        query
            .fetch_all(self.db_connection.read())
            .await?
            .iter()
            .map(outbox_state)
            .collect()
    }

    /// Store new versions of events. Each replaces the event's previous version, or the
    /// deletion that retired it, and is due at once; `published_at` marks it done already when
    /// there are no relays to publish to.
    pub async fn put_recovery_events(
        &self,
        events: Vec<RecoveryOutboxEvent>,
        now: i64,
        published_at: Option<i64>,
    ) -> Result<(), DatabaseWriteError> {
        if events.is_empty() {
            return Ok(());
        }
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                for event in &events {
                    sqlx::query(
                        "INSERT INTO recovery_outbox (d_tag, kind, user_pubkey, competition_id, content_sha256,
                                event_json, created_at, attempts, next_attempt_at, accepted_relays, published_at, last_error)
                         VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, '[]', ?, NULL)
                         ON CONFLICT(d_tag) DO UPDATE SET
                            kind = excluded.kind, user_pubkey = excluded.user_pubkey,
                            competition_id = excluded.competition_id, content_sha256 = excluded.content_sha256,
                            event_json = excluded.event_json, created_at = excluded.created_at, attempts = 0,
                            next_attempt_at = excluded.next_attempt_at, accepted_relays = '[]',
                            published_at = excluded.published_at, last_error = NULL,
                            deleted_at = NULL, deletion_json = NULL, deletion_published_at = NULL",
                    )
                    .bind(&event.d_tag)
                    .bind(event.kind)
                    .bind(&event.user_pubkey)
                    .bind(event.competition_id.map(|id| id.to_string()))
                    .bind(&event.content_sha256)
                    .bind(&event.event_json)
                    .bind(event.created_at)
                    .bind(now)
                    .bind(published_at)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    /// Events due for publishing at `now`: records' versions, and the deletions of retired
    /// records. Those never tried come first, so new versions do not wait behind retries to a
    /// relay that is down; then the oldest due.
    pub async fn due_recovery_events(
        &self,
        now: i64,
        limit: u32,
    ) -> Result<Vec<RecoveryDueEvent>, sqlx::Error> {
        sqlx::query(&format!(
            "SELECT d_tag, kind, content_sha256, attempts, accepted_relays, deleted_at,
                    CASE WHEN deleted_at IS NULL THEN event_json ELSE deletion_json END AS event_json
             FROM recovery_outbox
             WHERE next_attempt_at <= ? AND {PENDING}
             ORDER BY attempts > 0, next_attempt_at LIMIT ?"
        ))
        .bind(now)
        .bind(limit)
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(|row| {
            let accepted: String = row.try_get("accepted_relays")?;
            Ok(RecoveryDueEvent {
                d_tag: row.try_get("d_tag")?,
                kind: row.try_get("kind")?,
                content_sha256: row.try_get("content_sha256")?,
                event_json: row
                    .try_get::<Option<String>, _>("event_json")?
                    .unwrap_or_default(),
                attempts: row.try_get::<i64, _>("attempts")?.max(0) as u32,
                accepted_relays: serde_json::from_str(&accepted).unwrap_or_default(),
                deletion: row.try_get("deleted_at")?,
            })
        })
        .collect()
    }

    /// Record publishing attempts. An attempt on a version that was replaced, retired or
    /// re-queued meanwhile changes nothing, so what replaced it stays due.
    pub async fn record_recovery_attempts(
        &self,
        attempts: Vec<RecoveryAttempt>,
    ) -> Result<(), DatabaseWriteError> {
        if attempts.is_empty() {
            return Ok(());
        }
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                for attempt in &attempts {
                    let accepted = serde_json::to_string(&attempt.accepted_relays)
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
                    let before = serde_json::to_string(&attempt.accepted_before)
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
                    let deletion = attempt.deletion.is_some();
                    sqlx::query(
                        "UPDATE recovery_outbox SET attempts = attempts + 1, accepted_relays = ?,
                                published_at = CASE WHEN ? THEN published_at ELSE ? END,
                                deletion_published_at = CASE WHEN ? THEN ? ELSE deletion_published_at END,
                                next_attempt_at = ?, last_error = ?
                         WHERE d_tag = ? AND content_sha256 = ? AND deleted_at IS ? AND attempts = ?
                           AND accepted_relays = ?",
                    )
                    .bind(accepted)
                    .bind(deletion)
                    .bind(attempt.published_at)
                    .bind(deletion)
                    .bind(attempt.published_at)
                    .bind(attempt.next_attempt_at)
                    .bind(&attempt.error)
                    .bind(&attempt.d_tag)
                    .bind(&attempt.content_sha256)
                    .bind(attempt.deletion)
                    .bind(i64::from(attempt.attempts))
                    .bind(before)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    /// Events not yet taken by every relay: records' versions and deletions.
    pub async fn recovery_outbox_depth(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM recovery_outbox WHERE {PENDING}"
        ))
        .fetch_one(self.db_connection.read())
        .await
    }

    /// Make every event waiting out a retry due at `now`, as when a relay that was unreachable
    /// answers again. Events a republish spread out, not tried yet, keep their pace. Returns how
    /// many were waiting.
    pub async fn reset_recovery_backoff(&self, now: i64) -> Result<u64, DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                Ok(sqlx::query(&format!(
                    "UPDATE recovery_outbox SET next_attempt_at = ?
                     WHERE next_attempt_at > ? AND attempts > 0 AND {PENDING}"
                ))
                .bind(now)
                .bind(now)
                .execute(&pool)
                .await?
                .rows_affected())
            })
            .await
    }

    /// Offer every live record again: to the relays in `relays` only, or to every relay when it
    /// is empty. Each record forgets those relays took it, its attempts and its retry delay;
    /// they come due `per_tick` every `tick_secs` seconds from `now`, wallets first and then
    /// newest first, so a large backlog does not crowd out new versions. Retired records are
    /// left alone. Returns how many were queued.
    pub async fn requeue_recovery_records(
        &self,
        relays: Vec<String>,
        now: i64,
        per_tick: usize,
        tick_secs: i64,
    ) -> Result<u64, DatabaseWriteError> {
        let per_tick = per_tick.max(1);
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                let rows: Vec<(String, String)> = sqlx::query_as(
                    "SELECT d_tag, accepted_relays FROM recovery_outbox WHERE deleted_at IS NULL
                     ORDER BY kind = 'wallet' DESC, created_at DESC, d_tag",
                )
                .fetch_all(&mut *tx)
                .await?;
                for (index, (d_tag, accepted)) in rows.iter().enumerate() {
                    let kept: Vec<String> = if relays.is_empty() {
                        Vec::new()
                    } else {
                        serde_json::from_str::<Vec<String>>(accepted)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|relay| !relays.contains(relay))
                            .collect()
                    };
                    let kept =
                        serde_json::to_string(&kept).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
                    let due = now + (index / per_tick) as i64 * tick_secs;
                    sqlx::query(
                        "UPDATE recovery_outbox SET accepted_relays = ?, published_at = NULL, attempts = 0,
                                next_attempt_at = ?, last_error = NULL
                         WHERE d_tag = ?",
                    )
                    .bind(kept)
                    .bind(due)
                    .bind(d_tag)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                Ok(rows.len() as u64)
            })
            .await
    }

    /// Record which records' money is settled: `settled` from `now` on, unless they already
    /// were, and `unsettled` not at all.
    pub async fn set_recovery_settled(
        &self,
        settled: Vec<String>,
        unsettled: Vec<String>,
        now: i64,
    ) -> Result<(), DatabaseWriteError> {
        if settled.is_empty() && unsettled.is_empty() {
            return Ok(());
        }
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                for d_tag in &settled {
                    sqlx::query(
                        "UPDATE recovery_outbox SET settled_at = ? WHERE d_tag = ? AND settled_at IS NULL",
                    )
                    .bind(now)
                    .bind(d_tag)
                    .execute(&mut *tx)
                    .await?;
                }
                for d_tag in &unsettled {
                    sqlx::query(
                        "UPDATE recovery_outbox SET settled_at = NULL WHERE d_tag = ? AND settled_at IS NOT NULL",
                    )
                    .bind(d_tag)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    /// A wallet record is settled from when its player has no entry record left whose money is
    /// not, and unsettled again once they have one.
    pub async fn settle_recovery_wallets(&self, now: i64) -> Result<(), DatabaseWriteError> {
        const UNSETTLED_ENTRY: &str = "SELECT 1 FROM recovery_outbox e
             WHERE e.kind = 'entry' AND e.user_pubkey = recovery_outbox.user_pubkey
               AND e.settled_at IS NULL AND e.deleted_at IS NULL";
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                sqlx::query(&format!(
                    "UPDATE recovery_outbox SET settled_at = ?
                     WHERE kind = 'wallet' AND settled_at IS NULL AND deleted_at IS NULL
                       AND NOT EXISTS ({UNSETTLED_ENTRY})"
                ))
                .bind(now)
                .execute(&mut *tx)
                .await?;
                sqlx::query(&format!(
                    "UPDATE recovery_outbox SET settled_at = NULL
                     WHERE kind = 'wallet' AND settled_at IS NOT NULL AND EXISTS ({UNSETTLED_ENTRY})"
                ))
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    /// Hex pubkeys of players whose wallet record was retired but who have an entry record
    /// whose money is not settled: their wallet record is published again.
    pub async fn recovery_wallets_to_restore(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT w.user_pubkey FROM recovery_outbox w
             WHERE w.kind = 'wallet' AND w.deleted_at IS NOT NULL AND w.user_pubkey IS NOT NULL
               AND EXISTS (SELECT 1 FROM recovery_outbox e
                           WHERE e.kind = 'entry' AND e.user_pubkey = w.user_pubkey
                             AND e.settled_at IS NULL AND e.deleted_at IS NULL)
             LIMIT 100",
        )
        .fetch_all(self.db_connection.read())
        .await
    }

    /// Records whose money was settled at or before `cutoff`, oldest first, whose current
    /// version every relay took (or gave up on): the deletion then names the version the relays
    /// hold. A record a coordinator without retention rewrote after its deletion is listed again.
    pub async fn recovery_records_to_retire(
        &self,
        cutoff: i64,
        limit: u32,
    ) -> Result<Vec<RecoveryRetirement>, sqlx::Error> {
        sqlx::query(
            "SELECT d_tag, kind, content_sha256, event_json, created_at FROM recovery_outbox
             WHERE settled_at IS NOT NULL AND settled_at <= ? AND published_at IS NOT NULL
               AND (deleted_at IS NULL OR created_at > deleted_at)
             ORDER BY settled_at, d_tag LIMIT ?",
        )
        .bind(cutoff)
        .bind(limit)
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(|row| {
            Ok(RecoveryRetirement {
                d_tag: row.try_get("d_tag")?,
                kind: row.try_get("kind")?,
                content_sha256: row.try_get("content_sha256")?,
                event_json: row.try_get("event_json")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
    }

    /// Retire records: each keeps its last version and gets its deletion, due at once (or done
    /// already, `published_at`, when there are no relays). A record that changed since it was
    /// read is left live.
    pub async fn put_recovery_deletions(
        &self,
        deletions: Vec<RecoveryDeletion>,
        now: i64,
        published_at: Option<i64>,
    ) -> Result<u64, DatabaseWriteError> {
        if deletions.is_empty() {
            return Ok(0);
        }
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                let mut retired = 0;
                for deletion in &deletions {
                    retired += sqlx::query(
                        "UPDATE recovery_outbox SET deleted_at = ?, deletion_json = ?,
                                deletion_published_at = ?, attempts = 0, accepted_relays = '[]',
                                next_attempt_at = ?, published_at = COALESCE(published_at, ?),
                                last_error = NULL
                         WHERE d_tag = ? AND content_sha256 = ? AND created_at = ?
                           AND settled_at IS NOT NULL",
                    )
                    .bind(deletion.deleted_at)
                    .bind(&deletion.deletion_json)
                    .bind(published_at)
                    .bind(now)
                    .bind(now)
                    .bind(&deletion.d_tag)
                    .bind(&deletion.content_sha256)
                    .bind(deletion.created_at)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                }
                tx.commit().await?;
                Ok(retired)
            })
            .await
    }

    /// The outbox's records by kind: live, settled and waiting, and retired.
    pub async fn recovery_counts(&self) -> Result<Vec<RecoveryKindCount>, sqlx::Error> {
        sqlx::query(
            "SELECT kind,
                    COALESCE(SUM(deleted_at IS NULL), 0) AS live,
                    COALESCE(SUM(deleted_at IS NULL AND settled_at IS NOT NULL), 0) AS settled,
                    COALESCE(SUM(deleted_at IS NOT NULL), 0) AS retired
             FROM recovery_outbox GROUP BY kind ORDER BY kind",
        )
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(|row| {
            Ok(RecoveryKindCount {
                kind: row.try_get("kind")?,
                live: row.try_get("live")?,
                settled: row.try_get("settled")?,
                retired: row.try_get("retired")?,
            })
        })
        .collect()
    }

    /// Live records `relay` has not taken: what a republish to it still has to send.
    pub async fn recovery_relay_missing(&self, relay: &str) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM recovery_outbox
             WHERE deleted_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM json_each(recovery_outbox.accepted_relays) AS a
                               WHERE a.value = ?)",
        )
        .bind(relay)
        .fetch_one(self.db_connection.read())
        .await
    }

    /// A player's recovery file: their wallet and entry events, and the contract events of the
    /// competitions they entered, leaving out retired records. Returns `(kind, event_json)`
    /// pairs.
    pub async fn recovery_kit_events(
        &self,
        user_pubkey: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query(
            "SELECT kind, event_json FROM recovery_outbox
             WHERE deleted_at IS NULL
               AND (user_pubkey = ?
                    OR (kind = 'competition' AND competition_id IN (
                        SELECT competition_id FROM recovery_outbox
                        WHERE user_pubkey = ? AND kind = 'entry' AND deleted_at IS NULL)))
             ORDER BY kind, d_tag",
        )
        .bind(user_pubkey)
        .bind(user_pubkey)
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(|row| Ok((row.try_get("kind")?, row.try_get("event_json")?)))
        .collect()
    }
}
