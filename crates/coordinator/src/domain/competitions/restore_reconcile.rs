//! Reconciliation at start, for a database restored from a backup that may be behind what
//! already happened: payments LND sent and transactions already on chain.
//!
//! - **Chain.** Every settlement transaction is broadcast through `broadcast_or_known`: one the
//!   chain or the mempool already holds counts as broadcast, so a competition whose stored state
//!   is behind the chain records it and moves on instead of retrying forever. The runners' sweep
//!   steps every unfinished competition after a start.
//! - **Payouts the database marked failed but LND paid.** They are marked paid, with LND's proof,
//!   so the entry is not paid again.
//! - **Payments the database never recorded.** A payout recorded after the backup was taken is
//!   missing from the restored database, while LND paid it. Any payment LND sent after the newest
//!   write the database holds, whose hash no payout or refund knows, and whose amount is one an
//!   unpaid entry of an unsettled contract could be owed, holds that entry's Lightning payout
//!   until an operator releases it (`coordinator admin payout-holds`). The winner's on-chain claim
//!   is unaffected.
//!
//! The payout watcher and the automatic payouts wait for this to finish once, so nothing is paid
//! before it has looked. Running it again changes nothing: a payout is marked paid once, a hold is
//! kept per entry and payment, and a released hold stays released.

use super::*;
use crate::domain::{CompetitionKind, PaymentStatus};
use crate::infra::lightning::{extract_payment_hash_from_invoice, PaymentNotFound};
use serde::Deserialize;
use sqlx::Row;
use std::collections::HashSet;
use time::format_description::well_known::Rfc3339;

/// How far before the database's newest write LND's payments are compared, for the clocks of
/// LND and this host.
const CLOCK_MARGIN_SECS: i64 = 10 * 60;

/// What the reconciliation found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RestoreReconciliation {
    /// Payouts the database had failed that LND paid, now marked paid.
    pub payouts_marked_paid: usize,
    /// Entries whose Lightning payout is newly held.
    pub payouts_held: usize,
}

/// An entry's Lightning payout, held until an operator releases it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayoutHold {
    pub entry_id: Uuid,
    pub payment_hash: String,
    pub amount_sats: u64,
    pub reason: String,
    pub held_at: String,
    pub released_at: Option<String>,
}

/// A payout the database recorded as failed.
struct FailedPayout {
    id: Uuid,
    entry_id: Uuid,
    invoice: String,
    amount_sats: u64,
}

/// An entry of an unsettled contract that has no payout yet, with every amount it could be owed.
struct UnpaidEntry {
    entry_id: Uuid,
    amounts: HashSet<u64>,
}

/// Wakes the payout workers once the first reconciliation has finished.
#[derive(Clone)]
pub struct Reconciled {
    done: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Default for Reconciled {
    fn default() -> Self {
        Self {
            done: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }
}

impl Reconciled {
    /// Record that the reconciliation finished.
    pub fn finish(&self) {
        self.done.send_replace(true);
    }

    /// Wait until the reconciliation has finished once.
    pub async fn wait(self) {
        let mut done = self.done.subscribe();
        // The sender lives as long as `self`, so this only returns once it is done.
        drop(done.wait_for(|done| *done).await);
    }

    pub fn is_finished(&self) -> bool {
        *self.done.borrow()
    }
}

impl Coordinator {
    /// The signal the payout workers wait on before their first send.
    pub fn reconciled(&self) -> Reconciled {
        self.reconciled.clone()
    }

    /// Compare the payouts the database knows with what LND sent; see the module documentation.
    /// The caller marks [`Reconciled`] finished once this succeeds.
    pub async fn reconcile_after_restore(&self) -> Result<RestoreReconciliation, anyhow::Error> {
        let store = &self.competition_store;
        store.index_existing_payment_hashes().await?;
        let mut found = RestoreReconciliation::default();

        for payout in store.failed_payouts_of_unsettled_competitions().await? {
            let Ok(hash) = extract_payment_hash_from_invoice(&payout.invoice) else {
                continue;
            };
            let payment = match self.ln.lookup_payment(&hash).await {
                Ok(payment) => payment,
                Err(error) if error.is::<PaymentNotFound>() => continue,
                Err(error) => {
                    return Err(error.context(format!("look up failed payout {}", payout.id)))
                }
            };
            if payment.status != PaymentStatus::Succeeded {
                continue;
            }
            let proof = payment
                .payment_preimage
                .filter(|proof| verify_payout_preimage(proof, &hash).is_ok());
            if let Some(proof) = proof {
                if store.revive_paid_payout(payout.id, proof).await? {
                    warn!(
                        "Payout {} of entry {} was recorded as failed, but LND paid it; it is \
                         marked paid",
                        payout.id, payout.entry_id
                    );
                    found.payouts_marked_paid += 1;
                    continue;
                }
            }
            let reason = format!(
                "LND paid payout {}, which the database recorded as failed, and it could not be \
                 marked paid",
                payout.id
            );
            if store
                .hold_payout(payout.entry_id, &hash, payout.amount_sats, &reason)
                .await?
            {
                error!("Entry {}: Lightning payout held: {reason}", payout.entry_id);
                found.payouts_held += 1;
            }
        }

        let Some(newest_write) = store.newest_write().await? else {
            return self.finish_reconciliation(found).await;
        };
        let unpaid = self.unpaid_entries().await?;
        if unpaid.is_empty() {
            return self.finish_reconciliation(found).await;
        }
        let known = store.known_payment_hashes().await?;
        let since = newest_write.unix_timestamp() - CLOCK_MARGIN_SECS;
        for payment in self.ln.payments_since(since).await? {
            if payment.status == PaymentStatus::Failed || known.contains(&payment.payment_hash) {
                continue;
            }
            for entry in unpaid
                .iter()
                .filter(|entry| entry.amounts.contains(&payment.value_sat))
            {
                let reason = format!(
                    "LND sent {} sats in payment {} after the database's newest write, and no \
                     payout or refund it holds made that payment",
                    payment.value_sat, payment.payment_hash
                );
                if store
                    .hold_payout(
                        entry.entry_id,
                        &payment.payment_hash,
                        payment.value_sat,
                        &reason,
                    )
                    .await?
                {
                    error!("Entry {}: Lightning payout held: {reason}", entry.entry_id);
                    found.payouts_held += 1;
                }
            }
        }
        self.finish_reconciliation(found).await
    }

    async fn finish_reconciliation(
        &self,
        found: RestoreReconciliation,
    ) -> Result<RestoreReconciliation, anyhow::Error> {
        self.record_payout_holds().await?;
        if found != RestoreReconciliation::default() {
            warn!("Restore reconciliation: {found:?}");
        } else {
            info!("Restore reconciliation found the database in step with LND");
        }
        Ok(found)
    }

    async fn record_payout_holds(&self) -> Result<(), anyhow::Error> {
        let held = self.competition_store.payout_holds(false).await?.len();
        crate::metrics::PAYOUT_HOLDS.set(i64::try_from(held).unwrap_or(i64::MAX));
        Ok(())
    }

    /// Every entry of a signed contract that has not settled on chain and that has no payout
    /// yet, with the amount each outcome would owe it. Whether or when the outcome is known does
    /// not matter: a restored database may be behind the attestation too.
    async fn unpaid_entries(&self) -> Result<Vec<UnpaidEntry>, anyhow::Error> {
        let mut unpaid = Vec::new();
        for competition in self.competition_store.get_competitions(false).await? {
            // Payouts go on after the split transaction, so a contract is unsettled until it
            // completes.
            if competition.kind == CompetitionKind::Queued || competition.completed_at.is_some() {
                continue;
            }
            let Some(contract) = competition.signed_contract.as_ref() else {
                continue;
            };
            let params = contract.params();
            for entry in self
                .competition_store
                .get_competition_entries(competition.id, vec![EntryStatus::Paid])
                .await?
            {
                if entry.paid_out_at.is_some() {
                    continue;
                }
                let Ok(entry_pubkey) = entry.ephemeral_pubkey.parse::<Point>() else {
                    continue;
                };
                let amounts: HashSet<u64> = params
                    .outcome_payouts
                    .keys()
                    .filter_map(|outcome| winner_payout_sats(params, outcome, &entry_pubkey).ok())
                    .filter(|amount| *amount > 0)
                    .collect();
                if !amounts.is_empty() {
                    unpaid.push(UnpaidEntry {
                        entry_id: entry.id,
                        amounts,
                    });
                }
            }
        }
        Ok(unpaid)
    }

    /// Every payout hold, or only those not released.
    pub async fn payout_holds(&self, include_released: bool) -> Result<Vec<PayoutHold>, Error> {
        Ok(self
            .competition_store
            .payout_holds(include_released)
            .await?)
    }

    /// Release the holds on an entry's Lightning payout: an operator found LND's payment was not
    /// this entry's. Returns how many were released.
    pub async fn release_payout_hold(&self, entry_id: Uuid) -> Result<u64, Error> {
        let released = self.competition_store.release_payout_hold(entry_id).await?;
        if released > 0 {
            info!("Released {released} holds on the Lightning payout of entry {entry_id}");
        }
        self.record_payout_holds().await?;
        Ok(released)
    }

    /// Broadcast a settlement transaction. One the chain or the mempool already holds, as after
    /// a restore from a database behind the chain, counts as broadcast.
    pub(super) async fn broadcast_or_known(
        &self,
        transaction: &Transaction,
    ) -> Result<(), anyhow::Error> {
        let Err(error) = self.bitcoin.broadcast(transaction).await else {
            return Ok(());
        };
        let txid = transaction.compute_txid();
        match self.bitcoin.get_raw_transaction(&txid).await {
            Ok(known) if known.compute_txid() == txid => {
                info!("Transaction {txid} is already on chain or in the mempool ({error:#})");
                Ok(())
            }
            _ => Err(error),
        }
    }
}

impl CompetitionStore {
    async fn failed_payouts_of_unsettled_competitions(
        &self,
    ) -> Result<Vec<FailedPayout>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT p.id, p.entry_id, p.payout_payment_request, p.payout_amount_sats
             FROM payouts p
             JOIN entries e ON e.id = p.entry_id
             JOIN competitions c ON c.id = e.event_id
             WHERE p.failed_at IS NOT NULL AND p.succeed_at IS NULL
               AND c.completed_at IS NULL",
        )
        .fetch_all(self.db_connection.read())
        .await?;
        rows.into_iter()
            .map(|row| {
                let uuid = |column: &str| -> Result<Uuid, sqlx::Error> {
                    Uuid::parse_str(&row.try_get::<String, _>(column)?)
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
                };
                Ok(FailedPayout {
                    id: uuid("id")?,
                    entry_id: uuid("entry_id")?,
                    invoice: row.try_get("payout_payment_request")?,
                    amount_sats: u64::try_from(row.try_get::<i64, _>("payout_amount_sats")?)
                        .unwrap_or_default(),
                })
            })
            .collect()
    }

    /// Mark a failed payout paid with LND's proof, unless the entry has another live payout,
    /// which the one-live-payout rule forbids. Returns whether it was marked.
    async fn revive_paid_payout(
        &self,
        payout_id: Uuid,
        proof: String,
    ) -> Result<bool, DatabaseWriteError> {
        let id = payout_id.to_string();
        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        self.db_connection
            .execute_write(move |pool| async move {
                let changed = sqlx::query(
                    "UPDATE payouts
                     SET succeed_at = ?, failed_at = NULL, error = NULL,
                         payment_preimage = COALESCE(payment_preimage, ?)
                     WHERE id = ? AND failed_at IS NOT NULL AND succeed_at IS NULL
                       AND NOT EXISTS (SELECT 1 FROM payouts other
                                       WHERE other.entry_id = payouts.entry_id
                                         AND other.id != payouts.id
                                         AND other.failed_at IS NULL)",
                )
                .bind(now)
                .bind(proof)
                .bind(id)
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed == 1)
            })
            .await
    }

    /// When the database was last written, by the competitions, entries, tickets and payouts it
    /// lists (`list_updates`). None for an empty database.
    async fn newest_write(&self) -> Result<Option<OffsetDateTime>, sqlx::Error> {
        let newest: Option<String> = sqlx::query_scalar("SELECT max(updated_at) FROM list_updates")
            .fetch_one(self.db_connection.read())
            .await?;
        Ok(newest.and_then(|at| OffsetDateTime::parse(&at, &Rfc3339).ok()))
    }

    /// Every payment hash a payout or an escrow refund of this database pays.
    async fn known_payment_hashes(&self) -> Result<HashSet<String>, sqlx::Error> {
        let hashes: Vec<String> = sqlx::query_scalar(
            "SELECT payment_hash FROM payout_payment_hashes
             UNION SELECT payment_hash FROM ticket_ark_refunds",
        )
        .fetch_all(self.db_connection.read())
        .await?;
        Ok(hashes.into_iter().collect())
    }

    /// Hold an entry's Lightning payout for `payment_hash`. Returns whether the hold is new; one
    /// already kept, released or not, is left as it is.
    pub(super) async fn hold_payout(
        &self,
        entry_id: Uuid,
        payment_hash: &str,
        amount_sats: u64,
        reason: &str,
    ) -> Result<bool, DatabaseWriteError> {
        let (entry, hash, reason) = (
            entry_id.to_string(),
            payment_hash.to_owned(),
            reason.to_owned(),
        );
        let amount = i64::try_from(amount_sats).unwrap_or(i64::MAX);
        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        self.db_connection
            .execute_write(move |pool| async move {
                let inserted = sqlx::query(
                    "INSERT OR IGNORE INTO payout_holds
                         (entry_id, payment_hash, amount_sats, reason, held_at)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(entry)
                .bind(hash)
                .bind(amount)
                .bind(reason)
                .bind(now)
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(inserted == 1)
            })
            .await
    }

    /// Whether the entry's Lightning payout is held.
    pub async fn payout_held(&self, entry_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM payout_holds
                            WHERE entry_id = ? AND released_at IS NULL)",
        )
        .bind(entry_id.to_string())
        .fetch_one(self.db_connection.read())
        .await
    }

    async fn payout_holds(&self, include_released: bool) -> Result<Vec<PayoutHold>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT entry_id, payment_hash, amount_sats, reason, held_at, released_at
             FROM payout_holds WHERE ? OR released_at IS NULL
             ORDER BY held_at, entry_id",
        )
        .bind(include_released)
        .fetch_all(self.db_connection.read())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(PayoutHold {
                    entry_id: Uuid::parse_str(&row.try_get::<String, _>("entry_id")?)
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                    payment_hash: row.try_get("payment_hash")?,
                    amount_sats: u64::try_from(row.try_get::<i64, _>("amount_sats")?)
                        .unwrap_or_default(),
                    reason: row.try_get("reason")?,
                    held_at: row.try_get("held_at")?,
                    released_at: row.try_get("released_at")?,
                })
            })
            .collect()
    }

    async fn release_payout_hold(&self, entry_id: Uuid) -> Result<u64, DatabaseWriteError> {
        let entry = entry_id.to_string();
        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        self.db_connection
            .execute_write(move |pool| async move {
                Ok(sqlx::query(
                    "UPDATE payout_holds SET released_at = ?
                     WHERE entry_id = ? AND released_at IS NULL",
                )
                .bind(now)
                .bind(entry)
                .execute(&pool)
                .await?
                .rows_affected())
            })
            .await
    }
}
