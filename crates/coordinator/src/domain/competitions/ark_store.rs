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
