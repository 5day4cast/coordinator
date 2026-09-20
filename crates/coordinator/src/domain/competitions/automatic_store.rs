//! Durable preparation, payment and escrow-release state.
//!
//! Payment success and escrow release are separate commits. A release retry
//! must consume the stored payment proof without initiating another payment.

use super::{verify_entry_key, verify_payout_preimage, CompetitionStore};
use crate::infra::{db::DatabaseWriteError, lightning::extract_payment_hash_from_invoice};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone)]
pub struct PayoutJob {
    pub id: Uuid,
    pub entry_id: Uuid,
    pub request_json: String,
    pub prepared_json: Option<String>,
    pub payout_id: Option<Uuid>,
    pub attempts: u32,
}

fn invalid(message: impl Into<String>) -> DatabaseWriteError {
    sqlx::Error::Protocol(message.into()).into()
}

impl CompetitionStore {
    pub async fn payout_window_is_closed(&self, event_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM automatic_payout_competitions WHERE event_id = ? AND payout_window_closed_at IS NOT NULL)")
            .bind(event_id.to_string()).fetch_one(self.db_connection.read()).await
    }

    /// Stop new Lightning sends before beginning the on-chain fallback. Claims
    /// with an outbox row remain locked until their payment is reconciled.
    pub async fn close_payout_window(&self, event_id: Uuid) -> Result<(), DatabaseWriteError> {
        self.db_connection.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            let now = OffsetDateTime::now_utc().unix_timestamp();
            sqlx::query("UPDATE automatic_payout_competitions SET payout_window_closed_at = COALESCE(payout_window_closed_at, ?) WHERE event_id = ?")
                .bind(now).bind(event_id.to_string()).execute(&mut *tx).await?;
            sqlx::query("UPDATE payout_jobs SET failed_at = ?, last_error = 'On-chain settlement window reached' WHERE entry_id IN (SELECT id FROM entries WHERE event_id = ?) AND completed_at IS NULL AND failed_at IS NULL AND payout_id IS NULL")
                .bind(now).bind(event_id.to_string()).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(())
        }).await
    }

    pub async fn payout_send_allowed(&self, payout_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM payouts p JOIN entries e ON e.id = p.entry_id JOIN automatic_payout_competitions a ON a.event_id = e.event_id WHERE p.id = ? AND a.payout_window_closed_at IS NOT NULL)")
            .bind(payout_id.to_string()).fetch_one(self.db_connection.read()).await
    }

    /// Backfill payment hashes from legacy rows before accepting automatic claims.
    pub async fn index_existing_payment_hashes(&self) -> Result<(), DatabaseWriteError> {
        let rows = sqlx::query("SELECT p.id, p.payout_payment_request FROM payouts p LEFT JOIN payout_payment_hashes h ON h.payout_id = p.id WHERE h.payout_id IS NULL")
            .fetch_all(self.db_connection.read()).await?;
        let pairs: Vec<(String, String)> = rows
            .into_iter()
            .filter_map(|row| {
                let invoice: String = row.try_get("payout_payment_request").ok()?;
                Some((
                    extract_payment_hash_from_invoice(&invoice).ok()?,
                    row.try_get("id").ok()?,
                ))
            })
            .collect();
        if pairs.is_empty() {
            return Ok(());
        }
        self.db_connection.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            for (hash, id) in pairs {
                sqlx::query("INSERT OR IGNORE INTO payout_payment_hashes(payment_hash, payout_id) VALUES (?, ?)")
                    .bind(hash).bind(id).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            Ok(())
        }).await
    }

    pub async fn ticket_ids(&self, event_id: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM tickets WHERE event_id = ? ORDER BY id")
                .bind(event_id.to_string())
                .fetch_all(self.db_connection.read())
                .await?;
        ids.into_iter()
            .map(|id| Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e))))
            .collect()
    }

    pub async fn store_ticket_payout_policy(
        &self,
        ticket_id: Uuid,
        ticket_hash: String,
        entry_pubkey: String,
        policy_json: String,
    ) -> Result<(), DatabaseWriteError> {
        self.db_connection.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            // A recycled unpaid ticket has a new hash and may accept a new policy.
            sqlx::query("DELETE FROM ticket_payout_policies WHERE ticket_id = ? AND ticket_hash != ?")
                .bind(ticket_id.to_string()).bind(&ticket_hash).execute(&mut *tx).await?;
            sqlx::query("INSERT OR IGNORE INTO ticket_payout_policies(ticket_id, ticket_hash, entry_pubkey, policy_json) VALUES (?, ?, ?, ?)")
                .bind(ticket_id.to_string()).bind(&ticket_hash).bind(&entry_pubkey).bind(&policy_json).execute(&mut *tx).await?;
            let row = sqlx::query("SELECT entry_pubkey, policy_json FROM ticket_payout_policies WHERE ticket_id = ? AND ticket_hash = ?")
                .bind(ticket_id.to_string()).bind(ticket_hash).fetch_one(&mut *tx).await?;
            if row.try_get::<String, _>("entry_pubkey")? != entry_pubkey || row.try_get::<String, _>("policy_json")? != policy_json {
                return Err(sqlx::Error::Protocol("Ticket payout authorization has already been fixed".into()));
            }
            tx.commit().await?;
            Ok(())
        }).await
    }

    pub async fn ticket_payout_public_key(
        &self,
        ticket_id: Uuid,
        ticket_hash: &str,
    ) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT entry_pubkey FROM ticket_payout_policies WHERE ticket_id = ? AND ticket_hash = ?")
            .bind(ticket_id.to_string()).bind(ticket_hash).fetch_one(self.db_connection.read()).await
    }

    pub async fn ticket_payout_policy(
        &self,
        ticket_id: Uuid,
        ticket_hash: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT policy_json FROM ticket_payout_policies WHERE ticket_id = ? AND ticket_hash = ?")
            .bind(ticket_id.to_string()).bind(ticket_hash).fetch_optional(self.db_connection.read()).await
    }

    pub async fn store_payout_contract_binding(
        &self,
        event_id: Uuid,
        binding_json: String,
    ) -> Result<(), DatabaseWriteError> {
        self.db_connection.execute_write(move |pool| async move {
            sqlx::query("INSERT INTO payout_contract_bindings(event_id, binding_json) VALUES (?, ?) ON CONFLICT(event_id) DO UPDATE SET binding_json = excluded.binding_json")
                .bind(event_id.to_string()).bind(binding_json).execute(&pool).await?;
            Ok(())
        }).await
    }

    pub async fn payout_contract_binding(
        &self,
        event_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT binding_json FROM payout_contract_bindings WHERE event_id = ?")
            .bind(event_id.to_string())
            .fetch_optional(self.db_connection.read())
            .await
    }

    pub async fn enable_automatic_payouts(&self, event_id: Uuid) -> Result<(), DatabaseWriteError> {
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT OR IGNORE INTO automatic_payout_competitions(event_id) VALUES (?)",
                )
                .bind(event_id.to_string())
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    pub async fn has_automatic_payouts(&self, event_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM automatic_payout_competitions WHERE event_id = ?)",
        )
        .bind(event_id.to_string())
        .fetch_one(self.db_connection.read())
        .await
    }

    pub async fn store_entry_payout_policy(
        &self,
        entry_id: Uuid,
        policy_json: String,
    ) -> Result<(), DatabaseWriteError> {
        self.db_connection.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            sqlx::query("INSERT OR IGNORE INTO entry_payout_policies(entry_id, policy_json) VALUES (?, ?)")
                .bind(entry_id.to_string()).bind(&policy_json).execute(&mut *tx).await?;
            let stored: String = sqlx::query_scalar("SELECT policy_json FROM entry_payout_policies WHERE entry_id = ?")
                .bind(entry_id.to_string()).fetch_one(&mut *tx).await?;
            if stored != policy_json {
                return Err(sqlx::Error::Protocol("Entry payout authorization is immutable".into()));
            }
            tx.commit().await?;
            Ok(())
        }).await
    }

    pub async fn entry_payout_policy(&self, entry_id: Uuid) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT policy_json FROM entry_payout_policies WHERE entry_id = ?")
            .bind(entry_id.to_string())
            .fetch_optional(self.db_connection.read())
            .await
    }

    /// The unique index arbitrates concurrent workers and manual fallback.
    pub async fn create_payout_job(
        &self,
        entry_id: Uuid,
        request_json: String,
    ) -> Result<Uuid, DatabaseWriteError> {
        let id = Uuid::now_v7();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.db_connection.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            let closed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM entries e JOIN automatic_payout_competitions a ON a.event_id = e.event_id WHERE e.id = ? AND a.payout_window_closed_at IS NOT NULL)")
                .bind(entry_id.to_string()).fetch_one(&mut *tx).await?;
            if closed { return Err(sqlx::Error::Protocol("Lightning payout window is closed".into())); }

            sqlx::query("INSERT INTO payout_jobs(id, entry_id, request_json, created_at, retry_at) VALUES (?, ?, ?, ?, ?)")
                .bind(id.to_string()).bind(entry_id.to_string()).bind(request_json)
                .bind(now).bind(now).execute(&mut *tx).await?;
            let paid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payouts WHERE entry_id = ? AND failed_at IS NULL)")
                .bind(entry_id.to_string()).fetch_one(&mut *tx).await?;
            if paid {
                return Err(sqlx::Error::Protocol("Entry already has a live payment".into()));
            }
            tx.commit().await?;
            Ok(id)
        }).await
    }

    /// A manual fallback can replace an unprepared job. Once an invoice enters
    /// the outbox, only a conclusive payment failure permits another claim.
    pub async fn queue_invoice_fallback(
        &self,
        id: Uuid,
        entry_id: Uuid,
        request_json: String,
    ) -> Result<Uuid, DatabaseWriteError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.db_connection.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            let closed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM entries e JOIN automatic_payout_competitions a ON a.event_id = e.event_id WHERE e.id = ? AND a.payout_window_closed_at IS NOT NULL)")
                .bind(entry_id.to_string()).fetch_one(&mut *tx).await?;
            if closed { return Err(sqlx::Error::Protocol("Lightning payout window is closed".into())); }

            if let Some(row) = sqlx::query("SELECT entry_id, request_json FROM payout_jobs WHERE id = ?")
                .bind(id.to_string()).fetch_optional(&mut *tx).await? {
                if row.try_get::<String, _>("entry_id")? == entry_id.to_string() && row.try_get::<String, _>("request_json")? == request_json {
                    return Ok(id);
                }
                return Err(sqlx::Error::Protocol("Claim ID was already used for another request".into()));
            }
            let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payouts WHERE entry_id = ? AND failed_at IS NULL)")
                .bind(entry_id.to_string()).fetch_one(&mut *tx).await?;
            if live { return Err(sqlx::Error::Protocol("Payment already in progress or complete".into())); }
            sqlx::query("UPDATE payout_jobs SET failed_at = ? WHERE entry_id = ? AND failed_at IS NULL AND completed_at IS NULL AND (payout_id IS NULL OR payout_id IN (SELECT id FROM payouts WHERE failed_at IS NOT NULL AND succeed_at IS NULL))")
                .bind(now).bind(entry_id.to_string()).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO payout_jobs(id, entry_id, request_json, created_at, retry_at) VALUES (?, ?, ?, ?, ?)")
                .bind(id.to_string()).bind(entry_id.to_string()).bind(request_json).bind(now).bind(now).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(id)
        }).await
    }

    pub async fn payout_job_status(&self, entry_id: Uuid) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT CASE WHEN j.completed_at IS NOT NULL THEN 'Paid' WHEN p.succeed_at IS NOT NULL THEN 'Paid; completing settlement' WHEN j.payout_id IS NOT NULL THEN 'Payment in progress' WHEN j.last_error IS NOT NULL THEN 'Retrying automatically' ELSE 'Queued automatically' END FROM payout_jobs j LEFT JOIN payouts p ON p.id = j.payout_id WHERE j.entry_id = ? AND j.failed_at IS NULL ORDER BY j.created_at DESC LIMIT 1")
            .bind(entry_id.to_string()).fetch_optional(self.db_connection.read()).await
    }

    pub async fn has_unsettled_payout_jobs(&self, event_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payout_jobs j JOIN entries e ON e.id = j.entry_id WHERE e.event_id = ? AND j.completed_at IS NULL AND j.failed_at IS NULL)")
            .bind(event_id.to_string()).fetch_one(self.db_connection.read()).await
    }

    pub async fn has_live_payout_job(&self, entry_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM payout_jobs WHERE entry_id = ? AND failed_at IS NULL)",
        )
        .bind(entry_id.to_string())
        .fetch_one(self.db_connection.read())
        .await
    }

    pub async fn due_payout_jobs(&self) -> Result<Vec<PayoutJob>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, entry_id, request_json, prepared_json, payout_id, attempts FROM payout_jobs WHERE completed_at IS NULL AND failed_at IS NULL AND retry_at <= ? ORDER BY retry_at, created_at, id LIMIT 100")
            .bind(OffsetDateTime::now_utc().unix_timestamp()).fetch_all(self.db_connection.read()).await?;
        rows.into_iter()
            .map(|row| {
                let uuid = |field| -> Result<Uuid, sqlx::Error> {
                    Uuid::parse_str(row.try_get(field)?)
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
                };
                let payout_id: Option<String> = row.try_get("payout_id")?;
                Ok(PayoutJob {
                    id: uuid("id")?,
                    entry_id: uuid("entry_id")?,
                    request_json: row.try_get("request_json")?,
                    prepared_json: row.try_get("prepared_json")?,
                    payout_id: payout_id
                        .map(|v| Uuid::parse_str(&v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                    attempts: row.try_get("attempts")?,
                })
            })
            .collect()
    }

    /// Rotate pending payments behind claims that have not been checked yet.
    /// Waiting for LND is normal polling, so it does not increase error backoff.
    pub async fn schedule_payout_poll(&self, id: Uuid) -> Result<(), DatabaseWriteError> {
        let retry_at = OffsetDateTime::now_utc().unix_timestamp() + 5;
        self.db_connection.execute_write(move |pool| async move {
            sqlx::query("UPDATE payout_jobs SET retry_at = ? WHERE id = ? AND completed_at IS NULL AND failed_at IS NULL")
                .bind(retry_at).bind(id.to_string()).execute(&pool).await?;
            Ok(())
        }).await
    }

    /// Commit the authorized invoice and its sealed receipt with the outbox.
    /// Entry secrets remain absent until payment proof authorizes release.
    pub async fn store_prepared_payout(
        &self,
        job_id: Uuid,
        invoice: String,
        amount_sats: u64,
        prepared_json: String,
    ) -> Result<Uuid, DatabaseWriteError> {
        let hash =
            extract_payment_hash_from_invoice(&invoice).map_err(|e| invalid(e.to_string()))?;
        let amount = i64::try_from(amount_sats).map_err(|_| invalid("Payout amount overflow"))?;
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| invalid(e.to_string()))?;
        self.db_connection.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            let row = sqlx::query("SELECT entry_id, payout_id, prepared_json FROM payout_jobs WHERE id = ? AND failed_at IS NULL")
                .bind(job_id.to_string()).fetch_one(&mut *tx).await?;
            if let Some(existing) = row.try_get::<Option<String>, _>("payout_id")? {
                let existing_prepared: Option<String> = row.try_get("prepared_json")?;
                if existing_prepared.as_deref() != Some(&prepared_json) {
                    return Err(sqlx::Error::Protocol("Prepared claim changed during retry".into()));
                }
                return Uuid::parse_str(&existing).map_err(|e| sqlx::Error::Decode(Box::new(e)));
            }
            let entry_id: String = row.try_get("entry_id")?;
            let payout_id = Uuid::now_v7();
            sqlx::query("INSERT INTO payouts(id, entry_id, payout_payment_request, payout_amount_sats, initiated_at) VALUES (?, ?, ?, ?, ?)")
                .bind(payout_id.to_string()).bind(entry_id).bind(invoice).bind(amount).bind(now)
                .execute(&mut *tx).await?;
            sqlx::query("INSERT INTO payout_payment_hashes(payment_hash, payout_id) VALUES (?, ?)")
                .bind(hash).bind(payout_id.to_string()).execute(&mut *tx).await?;
            sqlx::query("UPDATE payout_jobs SET payout_id = ?, prepared_json = ?, last_error = NULL WHERE id = ?")
                .bind(payout_id.to_string()).bind(prepared_json).bind(job_id.to_string()).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(payout_id)
        }).await
    }

    pub async fn retry_payout_job(
        &self,
        id: Uuid,
        attempts: u32,
        error: String,
    ) -> Result<(), DatabaseWriteError> {
        let delay = 5_i64.saturating_mul(1_i64 << attempts.min(7)).min(300);
        let retry_at = OffsetDateTime::now_utc().unix_timestamp() + delay;
        self.db_connection.execute_write(move |pool| async move {
            sqlx::query("UPDATE payout_jobs SET attempts = attempts + 1, retry_at = ?, last_error = ? WHERE id = ? AND completed_at IS NULL AND failed_at IS NULL")
                .bind(retry_at).bind(error).bind(id.to_string()).execute(&pool).await?;
            Ok(())
        }).await
    }

    /// Retire a claim only after LND has conclusively failed its payment.
    pub async fn fail_payout_job(&self, id: Uuid) -> Result<(), DatabaseWriteError> {
        self.db_connection.execute_write(move |pool| async move {
            sqlx::query("UPDATE payout_jobs SET failed_at = ? WHERE id = ? AND completed_at IS NULL AND payout_id IN (SELECT id FROM payouts WHERE failed_at IS NOT NULL AND succeed_at IS NULL)")
                .bind(OffsetDateTime::now_utc().unix_timestamp()).bind(id.to_string()).execute(&pool).await?;
            Ok(())
        }).await
    }

    /// Store released secrets and completion atomically. Never replace an
    /// existing entry key/preimage with different material on retry.
    pub async fn complete_payout_job(
        &self,
        id: Uuid,
        entry_private_key: String,
        payout_preimage: String,
    ) -> Result<(), DatabaseWriteError> {
        self.db_connection.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            let row = sqlx::query("SELECT e.id, e.ephemeral_pubkey, e.payout_hash, e.ephemeral_privatekey, e.payout_preimage FROM payout_jobs j JOIN entries e ON e.id = j.entry_id JOIN payouts p ON p.id = j.payout_id WHERE j.id = ? AND j.failed_at IS NULL AND p.succeed_at IS NOT NULL AND p.payment_preimage IS NOT NULL")
                .bind(id.to_string()).fetch_one(&mut *tx).await?;
            let pubkey: String = row.try_get("ephemeral_pubkey")?;
            let hash: String = row.try_get("payout_hash")?;
            verify_entry_key(&entry_private_key, &pubkey.parse().map_err(|e| sqlx::Error::Protocol(format!("Invalid stored entry public key: {e}")))?).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            verify_payout_preimage(&payout_preimage, &hash).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            let old_key: Option<String> = row.try_get("ephemeral_privatekey")?;
            let old_preimage: Option<String> = row.try_get("payout_preimage")?;
            if old_key.as_ref().is_some_and(|v| v != &entry_private_key) || old_preimage.as_ref().is_some_and(|v| v != &payout_preimage) {
                return Err(sqlx::Error::Protocol("Escrow release changed existing secrets".into()));
            }
            sqlx::query("UPDATE entries SET ephemeral_privatekey = ?, payout_preimage = ? WHERE id = ?")
                .bind(entry_private_key).bind(payout_preimage).bind(row.try_get::<String, _>("id")?).execute(&mut *tx).await?;
            sqlx::query("UPDATE payout_jobs SET completed_at = COALESCE(completed_at, ?), last_error = NULL WHERE id = ?")
                .bind(OffsetDateTime::now_utc().unix_timestamp()).bind(id.to_string()).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(())
        }).await
    }
}
