//! Arkade escrows for tickets, and the batch that funded each Arkade competition.
//!
//! A ticket's escrow is fixed with its payout policy, keyed by the ticket's hash like the policy.
//! Its swap, not the Lightning HTLC, decides when the ticket is paid: the escrow must hold the
//! buy-in first.

use super::CompetitionStore;
use crate::infra::db::DatabaseWriteError;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

/// A ticket's escrow and the swap that funds it.
#[derive(Debug, Clone)]
pub struct TicketArkEscrow {
    pub ticket_id: Uuid,
    pub ticket_hash: String,
    /// The escrow's PSBT `TapTree` field, hex.
    pub escrow_tap_tree: String,
    pub escrow_address: String,
    pub swap_id: Option<Uuid>,
    /// The funded escrow VTXO, `txid:vout`.
    pub vtxo_outpoint: Option<String>,
    pub vtxo_sats: Option<u64>,
}

/// A competition's funded escrows, and how many of their players have been refunded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RefundProgress {
    /// Paid tickets whose escrow is funded and not written off: the entry fees to return.
    pub escrowed: u64,
    pub refunded: u64,
    /// Funded escrows an operator wrote off: no longer owed, so not in `escrowed`.
    #[serde(default)]
    pub written_off: u64,
    /// When the first escrow not refunded yet can be: its refund locktime. Nothing can be
    /// refunded before. Only the pages ask for it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub opens_at: Option<OffsetDateTime>,
}

/// Where a funded escrow's refund has got to.
///
/// `Minted` → `Submitted` → `Paid` → `Settled`. Each step is recorded before the next begins, so
/// an outage resumes rather than repeats: a refund never pays a player twice, and never signs a
/// second spend of one escrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArkRefundState {
    /// ark-swapd minted the swap this refund pays, for an invoice from the player's address.
    Minted,
    /// The refund's transactions are built and its Ark transaction is going to Arkade. The
    /// escrow may or may not be spent yet, so a resume asks the server before rebuilding.
    Submitting,
    /// The escrow was spent into that swap on Arkade.
    Submitted,
    /// The player's invoice was paid, and its preimage given to ark-swapd.
    Paid,
    /// ark-swapd claimed the swap, so the refund is done.
    Settled,
}

impl ArkRefundState {
    pub fn as_str(self) -> &'static str {
        match self {
            ArkRefundState::Minted => "minted",
            ArkRefundState::Submitting => "submitting",
            ArkRefundState::Submitted => "submitted",
            ArkRefundState::Paid => "paid",
            ArkRefundState::Settled => "settled",
        }
    }
}

impl std::str::FromStr for ArkRefundState {
    type Err = sqlx::Error;

    fn from_str(state: &str) -> Result<Self, Self::Err> {
        Ok(match state {
            "minted" => ArkRefundState::Minted,
            "submitting" => ArkRefundState::Submitting,
            "submitted" => ArkRefundState::Submitted,
            "paid" => ArkRefundState::Paid,
            "settled" => ArkRefundState::Settled,
            other => {
                return Err(sqlx::Error::Decode(
                    format!("unknown Arkade refund state {other}").into(),
                ))
            }
        })
    }
}

/// An escrow whose refund an operator wrote off, and why. Cleanup no longer tries to refund
/// it, and the pages no longer count it as owed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RefundWriteOff {
    pub ticket_id: Uuid,
    pub competition_id: Uuid,
    /// The escrow's VTXO, `txid:vout`, and what it holds.
    pub vtxo_outpoint: Option<String>,
    pub vtxo_sats: Option<u64>,
    pub reason: String,
    /// The refund's state when it was written off; None when none was ever minted.
    pub refund_state: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub written_off_at: OffsetDateTime,
}

/// A funded escrow of a paid ticket that has not been refunded, as a write-off sees it.
#[derive(Debug, Clone)]
pub struct UnrefundedArkEscrow {
    pub ticket_id: Uuid,
    pub ticket_hash: String,
    pub competition_id: Uuid,
    pub vtxo_outpoint: Option<String>,
    pub vtxo_sats: Option<u64>,
    /// Its competition was cancelled or failed, or is a queue that formed its pools without it:
    /// its escrow is owed back.
    pub ended: bool,
    /// A batch spent the competition's escrows into its pool, so nothing is owed back.
    pub pooled: bool,
    /// Its player sent a registration, with an entry or before paying, that can sign the refund.
    pub registered: bool,
    pub refund_state: Option<ArkRefundState>,
    /// The reason it was written off, if it was.
    pub written_off: Option<String>,
}

/// One ticket's refund, and the swap that pays its player.
#[derive(Debug, Clone)]
pub struct TicketArkRefund {
    pub ticket_id: Uuid,
    pub refund_id: Uuid,
    pub invoice: String,
    pub payment_hash: String,
    pub fee_sats: u64,
    pub state: ArkRefundState,
    pub ark_txid: Option<String>,
    /// The checkpoint Arkade signed, hex, kept so an interrupted refund finalizes rather than
    /// signing and spending the escrow again. Only its owner can sign it.
    pub checkpoint_psbt: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A swap still waiting to fund a reserved ticket's escrow.
#[derive(Debug, Clone)]
pub struct PendingArkSwap {
    pub ticket_id: Uuid,
    pub ticket_hash: String,
    pub competition_id: Uuid,
    pub swap_id: Uuid,
}

/// The Arkade batch that funded a competition's pool.
#[derive(Debug, Clone)]
pub struct ArkCommitment {
    pub batch_id: String,
    /// Consensus hex of the commitment transaction.
    pub commitment_tx: String,
    pub funding_vout: u32,
}

fn refund_row(row: &sqlx::sqlite::SqliteRow) -> Result<TicketArkRefund, sqlx::Error> {
    Ok(TicketArkRefund {
        ticket_id: Uuid::parse_str(row.try_get("ticket_id")?)
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
        refund_id: Uuid::parse_str(row.try_get("refund_id")?)
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
        invoice: row.try_get("invoice")?,
        payment_hash: row.try_get("payment_hash")?,
        fee_sats: row.try_get::<i64, _>("fee_sats")? as u64,
        state: row.try_get::<&str, _>("state")?.parse()?,
        ark_txid: row.try_get("ark_txid")?,
        checkpoint_psbt: row.try_get("checkpoint_psbt")?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn escrow_row(row: &sqlx::sqlite::SqliteRow) -> Result<TicketArkEscrow, sqlx::Error> {
    let uuid =
        |value: String| Uuid::parse_str(&value).map_err(|e| sqlx::Error::Decode(Box::new(e)));
    Ok(TicketArkEscrow {
        ticket_id: uuid(row.try_get("ticket_id")?)?,
        ticket_hash: row.try_get("ticket_hash")?,
        escrow_tap_tree: row.try_get("escrow_tap_tree")?,
        escrow_address: row.try_get("escrow_address")?,
        swap_id: row
            .try_get::<Option<String>, _>("swap_id")?
            .map(uuid)
            .transpose()?,
        vtxo_outpoint: row.try_get("vtxo_outpoint")?,
        vtxo_sats: row
            .try_get::<Option<i64>, _>("vtxo_sats")?
            .map(|sats| sats as u64),
    })
}

/// Competition ids as a JSON array, for `json_each` in a query.
fn id_list(ids: &[Uuid]) -> String {
    serde_json::Value::from(ids.iter().map(Uuid::to_string).collect::<Vec<_>>()).to_string()
}

impl CompetitionStore {
    pub async fn mark_ark_funded(&self, event_id: Uuid) -> Result<(), DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query("INSERT OR IGNORE INTO ark_funded_competitions(event_id) VALUES (?)")
                    .bind(event_id.to_string())
                    .execute(&pool)
                    .await?;
                Ok(())
            })
            .await
    }

    pub async fn is_ark_funded(&self, event_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ark_funded_competitions WHERE event_id = ?)",
        )
        .bind(event_id.to_string())
        .fetch_one(self.db_connection.read())
        .await
    }

    /// Fix a ticket's escrow for its current hash. A recycled ticket's old escrow is replaced.
    pub async fn store_ticket_ark_escrow(
        &self,
        ticket_id: Uuid,
        ticket_hash: String,
        escrow_tap_tree: String,
        escrow_address: String,
    ) -> Result<(), DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                sqlx::query(
                    "DELETE FROM ticket_ark_escrows WHERE ticket_id = ? AND ticket_hash != ?",
                )
                .bind(ticket_id.to_string())
                .bind(&ticket_hash)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT OR IGNORE INTO ticket_ark_escrows(ticket_id, ticket_hash, escrow_tap_tree, escrow_address) VALUES (?, ?, ?, ?)",
                )
                .bind(ticket_id.to_string())
                .bind(&ticket_hash)
                .bind(&escrow_tap_tree)
                .bind(&escrow_address)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    pub async fn ticket_ark_escrow(
        &self,
        ticket_id: Uuid,
        ticket_hash: &str,
    ) -> Result<Option<TicketArkEscrow>, sqlx::Error> {
        sqlx::query("SELECT * FROM ticket_ark_escrows WHERE ticket_id = ? AND ticket_hash = ?")
            .bind(ticket_id.to_string())
            .bind(ticket_hash)
            .fetch_optional(self.db_connection.read())
            .await?
            .as_ref()
            .map(escrow_row)
            .transpose()
    }

    pub async fn set_ticket_ark_swap(
        &self,
        ticket_id: Uuid,
        ticket_hash: String,
        swap_id: Uuid,
    ) -> Result<(), DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE ticket_ark_escrows SET swap_id = ? WHERE ticket_id = ? AND ticket_hash = ?",
                )
                .bind(swap_id.to_string())
                .bind(ticket_id.to_string())
                .bind(ticket_hash)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    /// Record the VTXO that funded a ticket's escrow, and the ticket as paid and settled.
    ///
    /// One transaction: a funded escrow is no longer a pending swap, so a ticket left unpaid
    /// beside it would never be marked paid. The swap service settles an Arkade ticket's
    /// invoice itself, so paid and settled are the same moment. Returns whether the ticket was
    /// still reserved for this hash and is now paid.
    pub async fn mark_ticket_ark_paid(
        &self,
        ticket_id: Uuid,
        ticket_hash: String,
        competition_id: Uuid,
        vtxo_outpoint: String,
        vtxo_sats: u64,
    ) -> Result<bool, DatabaseWriteError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                sqlx::query(
                    "UPDATE ticket_ark_escrows SET vtxo_outpoint = ?, vtxo_sats = ?, funded_at = ?
                     WHERE ticket_id = ? AND ticket_hash = ? AND funded_at IS NULL",
                )
                .bind(vtxo_outpoint)
                .bind(vtxo_sats as i64)
                .bind(now)
                .bind(ticket_id.to_string())
                .bind(&ticket_hash)
                .execute(&mut *tx)
                .await?;
                let paid = sqlx::query(
                    "UPDATE tickets SET paid_at = COALESCE(paid_at, datetime('now')),
                        settled_at = COALESCE(settled_at, datetime('now'))
                     WHERE id = ? AND hash = ? AND event_id = ? AND reserved_at IS NOT NULL",
                )
                .bind(ticket_id.to_string())
                .bind(&ticket_hash)
                .bind(competition_id.to_string())
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    > 0;
                tx.commit().await?;
                Ok(paid)
            })
            .await
    }

    /// Swaps for reserved tickets whose escrow is not funded yet.
    pub async fn pending_ark_swaps(&self) -> Result<Vec<PendingArkSwap>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT e.ticket_id, e.ticket_hash, e.swap_id, t.event_id
             FROM ticket_ark_escrows e JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
             WHERE e.swap_id IS NOT NULL AND e.funded_at IS NULL AND t.reserved_at IS NOT NULL",
        )
        .fetch_all(self.db_connection.read())
        .await?;
        rows.iter()
            .map(|row| {
                let uuid = |name: &str| -> Result<Uuid, sqlx::Error> {
                    Uuid::parse_str(&row.try_get::<String, _>(name)?)
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
                };
                Ok(PendingArkSwap {
                    ticket_id: uuid("ticket_id")?,
                    ticket_hash: row.try_get("ticket_hash")?,
                    competition_id: uuid("event_id")?,
                    swap_id: uuid("swap_id")?,
                })
            })
            .collect()
    }

    /// The funded escrows of a competition's paid tickets, in ticket order.
    pub async fn funded_ark_escrows(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<TicketArkEscrow>, sqlx::Error> {
        sqlx::query(
            "SELECT e.* FROM ticket_ark_escrows e JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
             WHERE t.event_id = ? AND t.paid_at IS NOT NULL AND e.funded_at IS NOT NULL
             ORDER BY e.ticket_id",
        )
        .bind(event_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(escrow_row)
        .collect()
    }

    /// How many of a competition's tickets are reserved with an invoice that can still be paid.
    pub async fn payable_ticket_count(&self, event_id: Uuid) -> Result<u64, sqlx::Error> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM tickets
             WHERE event_id = ? AND paid_at IS NULL AND reserved_at IS NOT NULL
               AND payment_request IS NOT NULL AND invoice_cancelled_at IS NULL
               AND invoice_expires_at > datetime('now')",
        )
        .bind(event_id.to_string())
        .fetch_one(self.db_connection.read())
        .await?;
        Ok(count as u64)
    }

    /// How far the refunds of each competition's funded escrows have got, for `event_ids` or for
    /// every competition. Competitions without funded escrows are left out.
    ///
    /// An escrow counts once its player has been paid; the escrows of a pool that a batch
    /// funded were spent into it, so they are not counted at all. A written-off escrow is no
    /// longer owed, so it is counted apart.
    pub async fn ark_refund_progress(
        &self,
        event_ids: Option<&[Uuid]>,
    ) -> Result<std::collections::HashMap<Uuid, RefundProgress>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT t.event_id AS event_id,
                    SUM(CASE WHEN w.ticket_id IS NULL THEN 1 ELSE 0 END) AS escrowed,
                    SUM(CASE WHEN w.ticket_id IS NULL AND r.state IN ('paid', 'settled')
                             THEN 1 ELSE 0 END) AS refunded,
                    SUM(CASE WHEN w.ticket_id IS NOT NULL THEN 1 ELSE 0 END) AS written_off
             FROM ticket_ark_escrows e
             JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
             LEFT JOIN ticket_ark_refunds r ON r.ticket_id = e.ticket_id
             LEFT JOIN ticket_ark_refund_write_offs w
                    ON w.ticket_id = e.ticket_id AND w.ticket_hash = e.ticket_hash
             WHERE e.funded_at IS NOT NULL AND t.paid_at IS NOT NULL
               AND (?1 IS NULL OR t.event_id IN (SELECT value FROM json_each(?1)))
               AND NOT EXISTS (SELECT 1 FROM ark_funded_competitions a
                               WHERE a.event_id = t.event_id AND a.commitment_tx IS NOT NULL)
             GROUP BY t.event_id",
        )
        .bind(event_ids.map(id_list))
        .fetch_all(self.db_connection.read())
        .await?;
        rows.iter()
            .map(|row| {
                let id: String = row.try_get("event_id")?;
                let id = Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                Ok((
                    id,
                    RefundProgress {
                        escrowed: row.try_get::<i64, _>("escrowed")? as u64,
                        refunded: row.try_get::<i64, _>("refunded")? as u64,
                        written_off: row.try_get::<i64, _>("written_off")? as u64,
                        opens_at: None,
                    },
                ))
            })
            .collect()
    }

    /// The tap trees of the funded escrows of `event_ids` whose players are not refunded yet,
    /// and were not written off, with their competitions. Each holds when its refund opens.
    pub async fn unrefunded_ark_escrow_trees(
        &self,
        event_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT t.event_id AS event_id, e.escrow_tap_tree AS escrow_tap_tree
             FROM ticket_ark_escrows e
             JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
             LEFT JOIN ticket_ark_refunds r ON r.ticket_id = e.ticket_id
             WHERE e.funded_at IS NOT NULL AND t.paid_at IS NOT NULL
               AND (r.state IS NULL OR r.state NOT IN ('paid', 'settled'))
               AND NOT EXISTS (SELECT 1 FROM ticket_ark_refund_write_offs w
                               WHERE w.ticket_id = e.ticket_id AND w.ticket_hash = e.ticket_hash)
               AND t.event_id IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (SELECT 1 FROM ark_funded_competitions a
                               WHERE a.event_id = t.event_id AND a.commitment_tx IS NOT NULL)",
        )
        .bind(id_list(event_ids))
        .fetch_all(self.db_connection.read())
        .await?;
        rows.iter()
            .map(|row| {
                let id: String = row.try_get("event_id")?;
                let id = Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                Ok((id, row.try_get("escrow_tap_tree")?))
            })
            .collect()
    }

    /// The funded escrows of a competition that still need refunding, in ticket order.
    ///
    /// An escrow is refunded once its refund settles. The escrows of a pool that a batch funded
    /// were spent into it, so they have nothing left to refund, and a written-off escrow is
    /// left alone.
    pub async fn refundable_ark_escrows(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<TicketArkEscrow>, sqlx::Error> {
        sqlx::query(
            "SELECT e.* FROM ticket_ark_escrows e
             JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
             LEFT JOIN ticket_ark_refunds r ON r.ticket_id = e.ticket_id
             WHERE t.event_id = ? AND e.funded_at IS NOT NULL
               AND (r.state IS NULL OR r.state != 'settled')
               AND NOT EXISTS (SELECT 1 FROM ticket_ark_refund_write_offs w
                               WHERE w.ticket_id = e.ticket_id AND w.ticket_hash = e.ticket_hash)
               AND NOT EXISTS (SELECT 1 FROM ark_funded_competitions a
                               WHERE a.event_id = t.event_id AND a.commitment_tx IS NOT NULL)
             ORDER BY e.ticket_id",
        )
        .bind(event_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(escrow_row)
        .collect()
    }

    /// The refund of one ticket's escrow, as far as it has got.
    pub async fn ticket_ark_refund(
        &self,
        ticket_id: Uuid,
    ) -> Result<Option<TicketArkRefund>, sqlx::Error> {
        let row = sqlx::query("SELECT * FROM ticket_ark_refunds WHERE ticket_id = ?")
            .bind(ticket_id.to_string())
            .fetch_optional(self.db_connection.read())
            .await?;
        row.as_ref().map(refund_row).transpose()
    }

    /// Record a refund's swap, before anything is signed or paid.
    pub async fn store_ticket_ark_refund(
        &self,
        refund: TicketArkRefund,
    ) -> Result<(), DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO ticket_ark_refunds (ticket_id, refund_id, invoice, payment_hash,
                        fee_sats, state, ark_txid, checkpoint_psbt, error, created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT (ticket_id) DO NOTHING",
                )
                .bind(refund.ticket_id.to_string())
                .bind(refund.refund_id.to_string())
                .bind(&refund.invoice)
                .bind(&refund.payment_hash)
                .bind(refund.fee_sats as i64)
                .bind(refund.state.as_str())
                .bind(&refund.ark_txid)
                .bind(&refund.checkpoint_psbt)
                .bind(&refund.error)
                .bind(refund.created_at)
                .bind(refund.updated_at)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    /// Replace a minted refund that went stale before anything was signed with a fresh one.
    ///
    /// Only a refund still `minted` as `stale_refund_id` is replaced: in that state no spend of
    /// the escrow was finalized and nothing was paid, so its swap and invoice can be dropped.
    /// Returns whether it was replaced.
    pub async fn replace_minted_ticket_ark_refund(
        &self,
        stale_refund_id: Uuid,
        refund: TicketArkRefund,
    ) -> Result<bool, DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                let replaced = sqlx::query(
                    "UPDATE ticket_ark_refunds SET refund_id = ?, invoice = ?, payment_hash = ?,
                        fee_sats = ?, state = ?, ark_txid = NULL, checkpoint_psbt = NULL,
                        error = NULL, created_at = ?, updated_at = ?
                     WHERE ticket_id = ? AND refund_id = ? AND state = 'minted'",
                )
                .bind(refund.refund_id.to_string())
                .bind(&refund.invoice)
                .bind(&refund.payment_hash)
                .bind(refund.fee_sats as i64)
                .bind(refund.state.as_str())
                .bind(refund.created_at)
                .bind(refund.updated_at)
                .bind(refund.ticket_id.to_string())
                .bind(stale_refund_id.to_string())
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(replaced > 0)
            })
            .await
    }

    /// Note why a minted refund is held back, or clear the note with `None`.
    ///
    /// Only a refund still `minted` is touched, and nothing else about it changes.
    pub async fn note_minted_ticket_ark_refund(
        &self,
        ticket_id: Uuid,
        error: Option<String>,
    ) -> Result<(), DatabaseWriteError> {
        let updated_at = OffsetDateTime::now_utc().unix_timestamp();
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE ticket_ark_refunds SET error = ?, updated_at = ?
                     WHERE ticket_id = ? AND state = 'minted'",
                )
                .bind(error)
                .bind(updated_at)
                .bind(ticket_id.to_string())
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    /// Move a refund on, once the step before it is done.
    ///
    /// What is already recorded is kept: a step that learns nothing new passes `None`.
    pub async fn advance_ticket_ark_refund(
        &self,
        ticket_id: Uuid,
        state: ArkRefundState,
        ark_txid: Option<String>,
        checkpoint_psbt: Option<String>,
        error: Option<String>,
    ) -> Result<(), DatabaseWriteError> {
        let updated_at = OffsetDateTime::now_utc().unix_timestamp();
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE ticket_ark_refunds SET state = ?, ark_txid = COALESCE(?, ark_txid),
                        checkpoint_psbt = COALESCE(?, checkpoint_psbt), error = ?, updated_at = ?
                     WHERE ticket_id = ?",
                )
                .bind(state.as_str())
                .bind(ark_txid)
                .bind(checkpoint_psbt)
                .bind(error)
                .bind(updated_at)
                .bind(ticket_id.to_string())
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    /// The funded escrows of paid tickets not refunded yet, of one ticket or one competition,
    /// in ticket order, with what a write-off needs to know about each.
    pub async fn unrefunded_ark_escrows(
        &self,
        ticket_id: Option<Uuid>,
        event_id: Option<Uuid>,
    ) -> Result<Vec<UnrefundedArkEscrow>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT e.ticket_id, e.ticket_hash, t.event_id, e.vtxo_outpoint, e.vtxo_sats,
                    (c.failed_at IS NOT NULL OR c.cancelled_at IS NOT NULL
                     OR (c.kind = 'queued' AND c.pools_formed_at IS NOT NULL)) AS ended,
                    EXISTS (SELECT 1 FROM ark_funded_competitions a
                            WHERE a.event_id = t.event_id AND a.commitment_tx IS NOT NULL) AS pooled,
                    (EXISTS (SELECT 1 FROM entries en WHERE en.ticket_id = t.id)
                     OR EXISTS (SELECT 1 FROM ticket_keymeld_registrations k
                                WHERE k.ticket_id = t.id AND k.ticket_hash = t.hash)) AS registered,
                    r.state AS refund_state, w.reason AS written_off
             FROM ticket_ark_escrows e
             JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
             JOIN competitions c ON c.id = t.event_id
             LEFT JOIN ticket_ark_refunds r ON r.ticket_id = e.ticket_id
             LEFT JOIN ticket_ark_refund_write_offs w
                    ON w.ticket_id = e.ticket_id AND w.ticket_hash = e.ticket_hash
             WHERE e.funded_at IS NOT NULL AND t.paid_at IS NOT NULL
               AND (r.state IS NULL OR r.state != 'settled')
               AND (?1 IS NULL OR e.ticket_id = ?1)
               AND (?2 IS NULL OR t.event_id = ?2)
             ORDER BY e.ticket_id",
        )
        .bind(ticket_id.map(|id| id.to_string()))
        .bind(event_id.map(|id| id.to_string()))
        .fetch_all(self.db_connection.read())
        .await?;
        rows.iter()
            .map(|row| {
                let uuid = |name: &str| -> Result<Uuid, sqlx::Error> {
                    Uuid::parse_str(&row.try_get::<String, _>(name)?)
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
                };
                Ok(UnrefundedArkEscrow {
                    ticket_id: uuid("ticket_id")?,
                    ticket_hash: row.try_get("ticket_hash")?,
                    competition_id: uuid("event_id")?,
                    vtxo_outpoint: row.try_get("vtxo_outpoint")?,
                    vtxo_sats: row
                        .try_get::<Option<i64>, _>("vtxo_sats")?
                        .map(|sats| sats as u64),
                    ended: row.try_get("ended")?,
                    pooled: row.try_get("pooled")?,
                    registered: row.try_get("registered")?,
                    refund_state: row
                        .try_get::<Option<&str>, _>("refund_state")?
                        .map(str::parse)
                        .transpose()?,
                    written_off: row.try_get("written_off")?,
                })
            })
            .collect()
    }

    /// Record that an escrow's refund is written off. Returns whether it was: an escrow
    /// already written off keeps its first reason.
    pub async fn write_off_ticket_ark_refund(
        &self,
        ticket_id: Uuid,
        ticket_hash: String,
        reason: String,
        refund_state: Option<ArkRefundState>,
    ) -> Result<bool, DatabaseWriteError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.db_connection
            .execute_write(move |pool| async move {
                let written = sqlx::query(
                    "INSERT INTO ticket_ark_refund_write_offs
                        (ticket_id, ticket_hash, reason, refund_state, written_off_at)
                     VALUES (?, ?, ?, ?, ?)
                     ON CONFLICT (ticket_id) DO NOTHING",
                )
                .bind(ticket_id.to_string())
                .bind(ticket_hash)
                .bind(reason)
                .bind(refund_state.map(ArkRefundState::as_str))
                .bind(now)
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(written > 0)
            })
            .await
    }

    /// The written-off refunds of `event_ids`, or of every competition, in ticket order.
    pub async fn ark_refund_write_offs(
        &self,
        event_ids: Option<&[Uuid]>,
    ) -> Result<Vec<RefundWriteOff>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT w.ticket_id, t.event_id, e.vtxo_outpoint, e.vtxo_sats, w.reason,
                    w.refund_state, w.written_off_at
             FROM ticket_ark_refund_write_offs w
             JOIN tickets t ON t.id = w.ticket_id AND t.hash = w.ticket_hash
             LEFT JOIN ticket_ark_escrows e
                    ON e.ticket_id = w.ticket_id AND e.ticket_hash = w.ticket_hash
             WHERE ?1 IS NULL OR t.event_id IN (SELECT value FROM json_each(?1))
             ORDER BY w.ticket_id",
        )
        .bind(event_ids.map(id_list))
        .fetch_all(self.db_connection.read())
        .await?;
        rows.iter()
            .map(|row| {
                let uuid = |name: &str| -> Result<Uuid, sqlx::Error> {
                    Uuid::parse_str(&row.try_get::<String, _>(name)?)
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
                };
                Ok(RefundWriteOff {
                    ticket_id: uuid("ticket_id")?,
                    competition_id: uuid("event_id")?,
                    vtxo_outpoint: row.try_get("vtxo_outpoint")?,
                    vtxo_sats: row
                        .try_get::<Option<i64>, _>("vtxo_sats")?
                        .map(|sats| sats as u64),
                    reason: row.try_get("reason")?,
                    refund_state: row.try_get("refund_state")?,
                    written_off_at: OffsetDateTime::from_unix_timestamp(
                        row.try_get("written_off_at")?,
                    )
                    .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                })
            })
            .collect()
    }

    pub async fn store_ark_commitment(
        &self,
        event_id: Uuid,
        commitment: ArkCommitment,
    ) -> Result<(), DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE ark_funded_competitions SET batch_id = ?, commitment_tx = ?, funding_vout = ? WHERE event_id = ?",
                )
                .bind(commitment.batch_id)
                .bind(commitment.commitment_tx)
                .bind(commitment.funding_vout as i64)
                .bind(event_id.to_string())
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    pub async fn ark_commitment(
        &self,
        event_id: Uuid,
    ) -> Result<Option<ArkCommitment>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT batch_id, commitment_tx, funding_vout FROM ark_funded_competitions
             WHERE event_id = ? AND commitment_tx IS NOT NULL",
        )
        .bind(event_id.to_string())
        .fetch_optional(self.db_connection.read())
        .await?;
        row.map(|row| {
            Ok(ArkCommitment {
                batch_id: row.try_get("batch_id")?,
                commitment_tx: row.try_get("commitment_tx")?,
                funding_vout: row.try_get::<i64, _>("funding_vout")? as u32,
            })
        })
        .transpose()
    }
}
