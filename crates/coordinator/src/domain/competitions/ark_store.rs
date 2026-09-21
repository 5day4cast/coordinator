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

    /// Record the VTXO that funded a ticket's escrow.
    pub async fn mark_ticket_ark_funded(
        &self,
        ticket_id: Uuid,
        ticket_hash: String,
        vtxo_outpoint: String,
        vtxo_sats: u64,
    ) -> Result<(), DatabaseWriteError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE ticket_ark_escrows SET vtxo_outpoint = ?, vtxo_sats = ?, funded_at = ? WHERE ticket_id = ? AND ticket_hash = ? AND funded_at IS NULL",
                )
                .bind(vtxo_outpoint)
                .bind(vtxo_sats as i64)
                .bind(now)
                .bind(ticket_id.to_string())
                .bind(ticket_hash)
                .execute(&pool)
                .await?;
                Ok(())
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
