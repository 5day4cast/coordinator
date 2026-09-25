//! The Keymeld registration a player sends before paying for their ticket.
//!
//! The browser seals the player's entry key to Keymeld when it gets the ticket, and sends that
//! registration before it shows the invoice. So a player who pays and never enters can still be
//! refunded: Keymeld has the key their escrow's refund is signed with.
//!
//! A registration belongs to one reservation of its ticket, named by the ticket's hash. It is
//! deleted when the reservation is released: the invoice expired or was cancelled, or someone else
//! took the ticket over. Only a paid ticket's registration is ever given to Keymeld, and all of a
//! competition's are deleted once it ends and has no refund left to sign.

use super::CompetitionStore;
use crate::infra::db::DatabaseWriteError;
use coordinator_escrow::escrow::SignedEscrowPolicy;
use keymeld_sdk::types::RegistrationContext;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

/// A player's Keymeld registration for their ticket, as their browser sealed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketRegistration {
    /// The entry key, whose private half the envelope holds.
    pub ephemeral_pubkey: String,
    /// The entry key, encrypted to the Keymeld enclave the ticket is assigned to.
    pub encrypted_keymeld_private_key: String,
    pub keymeld_auth_pubkey: String,
    pub keymeld_registration_context: RegistrationContext,
    /// The player's signed consent to what Keymeld may do with the key, refunds included.
    #[serde(default)]
    pub keymeld_escrow_policy: Option<SignedEscrowPolicy>,
}

impl TicketRegistration {
    /// Whether two registrations are the same, field for field.
    pub fn same_as(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

/// What storing a ticket's registration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStored {
    /// It is now the ticket's registration.
    Stored,
    /// The ticket already had this exact registration.
    Unchanged,
    /// The ticket is no longer reserved under that hash by that player.
    ReservationChanged,
    /// The ticket is paid, so its registration is fixed, and this one differs.
    Fixed,
}

/// A paid ticket that was never used for an entry, with the registration its player sent.
#[derive(Debug, Clone)]
pub struct PaidTicketRegistration {
    pub ticket_id: Uuid,
    pub registration: String,
    /// The payout policy the player accepted before paying, if the competition has one.
    pub payout_policy: Option<String>,
}

impl CompetitionStore {
    /// Keep `registration` for the ticket while `player` holds it under `ticket_hash`.
    ///
    /// Until the ticket is paid its player may send it again. Once it is paid, only the same
    /// registration is accepted: it is the one the ticket is refunded with.
    pub async fn store_ticket_registration(
        &self,
        ticket_id: Uuid,
        ticket_hash: String,
        player: String,
        registration: String,
    ) -> Result<RegistrationStored, DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                let ticket = sqlx::query(
                    "SELECT paid_at IS NOT NULL AS paid FROM tickets
                     WHERE id = ? AND hash = ? AND reserved_by = ?",
                )
                .bind(ticket_id.to_string())
                .bind(&ticket_hash)
                .bind(&player)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(ticket) = ticket else {
                    return Ok(RegistrationStored::ReservationChanged);
                };
                let paid: bool = ticket.try_get("paid")?;
                sqlx::query(
                    "DELETE FROM ticket_keymeld_registrations WHERE ticket_id = ? AND ticket_hash != ?",
                )
                .bind(ticket_id.to_string())
                .bind(&ticket_hash)
                .execute(&mut *tx)
                .await?;
                let stored: Option<String> = sqlx::query_scalar(
                    "SELECT registration_json FROM ticket_keymeld_registrations WHERE ticket_id = ?",
                )
                .bind(ticket_id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
                let outcome = match stored {
                    Some(stored) if stored == registration => RegistrationStored::Unchanged,
                    Some(_) if paid => RegistrationStored::Fixed,
                    _ => {
                        sqlx::query(
                            "INSERT INTO ticket_keymeld_registrations(ticket_id, ticket_hash, registration_json)
                             VALUES (?, ?, ?)
                             ON CONFLICT(ticket_id) DO UPDATE SET
                                 ticket_hash = excluded.ticket_hash,
                                 registration_json = excluded.registration_json",
                        )
                        .bind(ticket_id.to_string())
                        .bind(&ticket_hash)
                        .bind(&registration)
                        .execute(&mut *tx)
                        .await?;
                        RegistrationStored::Stored
                    }
                };
                tx.commit().await?;
                Ok(outcome)
            })
            .await
    }

    /// The registration sent for the ticket's reservation under `ticket_hash`.
    pub async fn ticket_registration(
        &self,
        ticket_id: Uuid,
        ticket_hash: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT registration_json FROM ticket_keymeld_registrations
             WHERE ticket_id = ? AND ticket_hash = ?",
        )
        .bind(ticket_id.to_string())
        .bind(ticket_hash)
        .fetch_optional(self.db_connection.read())
        .await
    }

    /// The competition's paid tickets that were never used for an entry, and the registration
    /// each one's player sent before paying. In ticket order.
    pub async fn paid_ticket_registrations(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<PaidTicketRegistration>, sqlx::Error> {
        sqlx::query(
            "SELECT t.id, r.registration_json, p.policy_json
             FROM tickets t
             JOIN ticket_keymeld_registrations r ON r.ticket_id = t.id AND r.ticket_hash = t.hash
             LEFT JOIN ticket_payout_policies p ON p.ticket_id = t.id AND p.ticket_hash = t.hash
             WHERE t.event_id = ? AND t.paid_at IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM entries e WHERE e.ticket_id = t.id)
             ORDER BY t.id",
        )
        .bind(event_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?
        .iter()
        .map(|row| {
            Ok(PaidTicketRegistration {
                ticket_id: Uuid::parse_str(&row.try_get::<String, _>("id")?)
                    .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                registration: row.try_get("registration_json")?,
                payout_policy: row.try_get("policy_json")?,
            })
        })
        .collect()
    }

    /// Delete the registrations nothing will use, and return how many went:
    ///
    /// - an unpaid ticket's, once its reservation was released or taken over;
    /// - every registration of a competition that completed or expired;
    /// - in a competition that was cancelled or failed, a ticket's once its escrow has no refund
    ///   left to sign, or, while unpaid, once its invoice can no longer be paid.
    pub async fn purge_ticket_registrations(&self) -> Result<u64, DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                let deleted = sqlx::query(
                    "DELETE FROM ticket_keymeld_registrations WHERE ticket_id IN (
                        SELECT r.ticket_id
                        FROM ticket_keymeld_registrations r
                        JOIN tickets t ON t.id = r.ticket_id
                        JOIN competitions c ON c.id = t.event_id
                        WHERE (t.paid_at IS NULL
                               AND (t.hash != r.ticket_hash OR t.reserved_by IS NULL))
                           OR c.completed_at IS NOT NULL
                           OR c.expiry_broadcasted_at IS NOT NULL
                           OR ((c.cancelled_at IS NOT NULL OR c.failed_at IS NOT NULL)
                               AND NOT EXISTS (
                                   SELECT 1 FROM ticket_ark_escrows e
                                   LEFT JOIN ticket_ark_refunds f ON f.ticket_id = e.ticket_id
                                   WHERE e.ticket_id = t.id AND e.ticket_hash = t.hash
                                     AND e.funded_at IS NOT NULL
                                     AND (f.state IS NULL OR f.state != 'settled')
                                     AND NOT EXISTS (
                                         SELECT 1 FROM ark_funded_competitions a
                                         WHERE a.event_id = c.id AND a.commitment_tx IS NOT NULL))
                               AND NOT (t.paid_at IS NULL
                                        AND t.invoice_cancelled_at IS NULL
                                        AND t.invoice_expires_at > datetime('now'))))",
                )
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(deleted)
            })
            .await
    }
}
