//! What the recovery records read, and the outbox they wait in until the relays take them.
//!
//! The records are rebuilt from these rows whenever a competition changes; see
//! `domain::recovery` and docs/RECOVERY.md.

use super::CompetitionStore;
use crate::infra::db::DatabaseWriteError;
use sqlx::{sqlite::SqliteRow, Row};
use std::collections::HashMap;
use uuid::Uuid;

/// One ticket of a competition, with what its player's recovery record needs.
///
/// Every ticket an entry used is listed. A ticket no entry used yet is listed only once its
/// player fixed a payout policy for it, which names the entry id the entry key comes from.
#[derive(Debug, Clone, Default)]
pub struct RecoveryTicketRow {
    pub ticket_id: Uuid,
    pub ticket_hash: String,
    /// Plaintext hex despite the column's name; see `Ticket::encrypted_preimage`.
    pub ticket_preimage: String,
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
}

/// What the outbox holds for one event: the digest of its plaintext and its `created_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryOutboxState {
    pub content_sha256: String,
    pub created_at: i64,
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

/// An outbox event due for publishing.
#[derive(Debug, Clone)]
pub struct RecoveryDueEvent {
    pub d_tag: String,
    pub content_sha256: String,
    pub event_json: String,
    pub attempts: u32,
    pub accepted_relays: Vec<String>,
}

/// What one publishing attempt of an event achieved.
#[derive(Debug, Clone)]
pub struct RecoveryAttempt {
    pub d_tag: String,
    /// The version attempted. A newer version written meanwhile is left due.
    pub content_sha256: String,
    pub accepted_relays: Vec<String>,
    /// Set when no relay is left to try.
    pub published_at: Option<i64>,
    pub next_attempt_at: i64,
    pub error: Option<String>,
}

fn uuid(value: String) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(&value).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

fn ticket_row(row: &SqliteRow) -> Result<RecoveryTicketRow, sqlx::Error> {
    Ok(RecoveryTicketRow {
        ticket_id: uuid(row.try_get("ticket_id")?)?,
        ticket_hash: row.try_get("ticket_hash")?,
        ticket_preimage: row.try_get("ticket_preimage")?,
        reserved_at: row.try_get("reserved_at")?,
        reserved_by: row.try_get("reserved_by")?,
        paid: row.try_get("paid")?,
        settled: row.try_get("settled")?,
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
    })
}

impl CompetitionStore {
    /// Every ticket of a competition that a recovery record describes.
    pub async fn recovery_ticket_rows(
        &self,
        competition_id: Uuid,
    ) -> Result<Vec<RecoveryTicketRow>, sqlx::Error> {
        sqlx::query(
            "SELECT t.id AS ticket_id, t.hash AS ticket_hash, t.encrypted_preimage AS ticket_preimage,
                    unixepoch(t.reserved_at) AS reserved_at, t.reserved_by,
                    t.paid_at IS NOT NULL AS paid, t.settled_at IS NOT NULL AS settled,
                    e.id AS entry_id, e.pubkey AS entry_user, e.ephemeral_pubkey AS entry_pubkey,
                    p.entry_pubkey AS policy_entry_pubkey, p.policy_json,
                    a.escrow_tap_tree, a.vtxo_outpoint, a.vtxo_sats, r.state AS refund_state,
                    EXISTS(SELECT 1 FROM payouts WHERE payouts.entry_id = e.id AND payouts.succeed_at IS NOT NULL) AS paid_out,
                    (e.sellback_broadcasted_at IS NOT NULL OR e.reclaimed_broadcasted_at IS NOT NULL) AS closed_on_chain
             FROM tickets t
             LEFT JOIN entries e ON e.ticket_id = t.id
             LEFT JOIN ticket_payout_policies p ON p.ticket_id = t.id AND p.ticket_hash = t.hash
             LEFT JOIN ticket_ark_escrows a ON a.ticket_id = t.id AND a.ticket_hash = t.hash
             LEFT JOIN ticket_ark_refunds r ON r.ticket_id = t.id
             WHERE t.event_id = ? AND (e.id IS NOT NULL OR p.ticket_id IS NOT NULL)
             ORDER BY t.id",
        )
        .bind(competition_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(ticket_row)
        .collect()
    }

    pub async fn recovery_competition(
        &self,
        competition_id: Uuid,
    ) -> Result<Option<RecoveryCompetitionRow>, sqlx::Error> {
        let Some(row) = sqlx::query(
            "SELECT id, event_submission, event_announcement, contract_parameters, signed_contract,
                    funding_outpoint, funding_transaction, attestation,
                    expiry_broadcasted_at IS NOT NULL AS expiry_broadcasted
             FROM competitions WHERE id = ?",
        )
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

    /// Competitions that may still hold a player's money: every one not completed.
    pub async fn recovery_open_competitions(&self) -> Result<Vec<Uuid>, sqlx::Error> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM competitions WHERE completed_at IS NULL ORDER BY created_at DESC",
        )
        .fetch_all(self.db_connection.read())
        .await?;
        ids.into_iter().map(uuid).collect()
    }

    /// The outbox's current version of each event of one competition, or of every wallet
    /// record when `competition_id` is `None`.
    pub async fn recovery_outbox_states(
        &self,
        competition_id: Option<Uuid>,
    ) -> Result<HashMap<String, RecoveryOutboxState>, sqlx::Error> {
        let query = match competition_id {
            Some(id) => sqlx::query(
                "SELECT d_tag, content_sha256, created_at FROM recovery_outbox WHERE competition_id = ?",
            )
            .bind(id.to_string()),
            None => sqlx::query(
                "SELECT d_tag, content_sha256, created_at FROM recovery_outbox WHERE kind = 'wallet'",
            ),
        };
        query
            .fetch_all(self.db_connection.read())
            .await?
            .iter()
            .map(|row| {
                Ok((
                    row.try_get("d_tag")?,
                    RecoveryOutboxState {
                        content_sha256: row.try_get("content_sha256")?,
                        created_at: row.try_get("created_at")?,
                    },
                ))
            })
            .collect()
    }

    /// Store new versions of events. Each replaces the event's previous version and is due at
    /// once; `published_at` marks it done already when there are no relays to publish to.
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
                            published_at = excluded.published_at, last_error = NULL",
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

    /// Events due for publishing at `now`, oldest first.
    pub async fn due_recovery_events(
        &self,
        now: i64,
        limit: u32,
    ) -> Result<Vec<RecoveryDueEvent>, sqlx::Error> {
        sqlx::query(
            "SELECT d_tag, content_sha256, event_json, attempts, accepted_relays FROM recovery_outbox
             WHERE published_at IS NULL AND next_attempt_at <= ? ORDER BY next_attempt_at LIMIT ?",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(|row| {
            let accepted: String = row.try_get("accepted_relays")?;
            Ok(RecoveryDueEvent {
                d_tag: row.try_get("d_tag")?,
                content_sha256: row.try_get("content_sha256")?,
                event_json: row.try_get("event_json")?,
                attempts: row.try_get::<i64, _>("attempts")?.max(0) as u32,
                accepted_relays: serde_json::from_str(&accepted).unwrap_or_default(),
            })
        })
        .collect()
    }

    /// Record publishing attempts. An attempt on a version that was replaced meanwhile changes
    /// nothing, so the new version stays due.
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
                    sqlx::query(
                        "UPDATE recovery_outbox SET attempts = attempts + 1, accepted_relays = ?,
                                published_at = ?, next_attempt_at = ?, last_error = ?
                         WHERE d_tag = ? AND content_sha256 = ?",
                    )
                    .bind(accepted)
                    .bind(attempt.published_at)
                    .bind(attempt.next_attempt_at)
                    .bind(&attempt.error)
                    .bind(&attempt.d_tag)
                    .bind(&attempt.content_sha256)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    /// Events not yet taken by every relay.
    pub async fn recovery_outbox_depth(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT COUNT(*) FROM recovery_outbox WHERE published_at IS NULL")
            .fetch_one(self.db_connection.read())
            .await
    }

    /// A player's recovery file: their wallet and entry events, and the contract events of the
    /// competitions they entered. Returns `(kind, event_json)` pairs.
    pub async fn recovery_kit_events(
        &self,
        user_pubkey: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query(
            "SELECT kind, event_json FROM recovery_outbox
             WHERE user_pubkey = ?
                OR (kind = 'competition' AND competition_id IN (
                    SELECT competition_id FROM recovery_outbox WHERE user_pubkey = ? AND kind = 'entry'))
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
