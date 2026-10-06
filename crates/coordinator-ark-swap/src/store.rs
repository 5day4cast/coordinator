//! Swaps, persisted in SQLite so a restart resumes every swap where it stopped.

use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use uuid::Uuid;

use crate::preimages::{PreimageKey, Row as PreimageRow};

/// Where a swap is.
///
/// `AwaitingPayment` → `PayingEscrow` → `EscrowPaid` → `Settled`.
/// A swap ends `Expired` if nobody pays, or `Failed` if the escrow could not be paid and the invoice was cancelled.
/// `Unsettled` means the escrow was paid but the invoice could not be settled; it needs an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapState {
    AwaitingPayment,
    PayingEscrow,
    EscrowPaid,
    Settled,
    Expired,
    Failed,
    Unsettled,
}

impl SwapState {
    pub fn is_final(self) -> bool {
        matches!(
            self,
            SwapState::Settled | SwapState::Expired | SwapState::Failed | SwapState::Unsettled
        )
    }

    fn as_str(self) -> &'static str {
        match self {
            SwapState::AwaitingPayment => "awaiting_payment",
            SwapState::PayingEscrow => "paying_escrow",
            SwapState::EscrowPaid => "escrow_paid",
            SwapState::Settled => "settled",
            SwapState::Expired => "expired",
            SwapState::Failed => "failed",
            SwapState::Unsettled => "unsettled",
        }
    }
}

impl FromStr for SwapState {
    type Err = anyhow::Error;

    fn from_str(state: &str) -> anyhow::Result<Self> {
        Ok(match state {
            "awaiting_payment" => SwapState::AwaitingPayment,
            "paying_escrow" => SwapState::PayingEscrow,
            "escrow_paid" => SwapState::EscrowPaid,
            "settled" => SwapState::Settled,
            "expired" => SwapState::Expired,
            "failed" => SwapState::Failed,
            "unsettled" => SwapState::Unsettled,
            other => anyhow::bail!("unknown swap state {other}"),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Swap {
    pub id: Uuid,
    pub escrow_address: String,
    pub amount_sat: u64,
    pub payment_hash: String,
    /// Never leaves the service: revealing it lets anyone settle the invoice. Stored sealed; see
    /// `preimages.rs`.
    #[serde(skip)]
    pub preimage: String,
    pub invoice: String,
    pub state: SwapState,
    /// The escrow VTXO this swap paid.
    pub escrow_vtxo: Option<String>,
    pub ark_txid: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
    /// Lookups of the escrow VTXO after the swap settled without it, the time the next is due
    /// (UNIX seconds), and when the service gave up looking.
    pub vtxo_lookups: u32,
    pub vtxo_lookup_after: Option<i64>,
    pub vtxo_lookup_gave_up_at: Option<i64>,
    /// When the last escrow payment was sent (UNIX seconds), recorded before sending it, and
    /// how many were sent.
    pub pay_attempted_at: Option<i64>,
    pub pay_attempts: u32,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    preimages: PreimageKey,
}

impl Store {
    pub async fn open(path: &Path, preimages: PreimageKey) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        // A database a newer release migrated still opens, so this release can be rolled back to.
        let mut migrator = sqlx::migrate!("./migrations");
        migrator.set_ignore_missing(true);
        migrator.run(&pool).await?;
        Ok(Self { pool, preimages })
    }

    /// Take or keep the worker lease for `ttl`. False while another instance holds it.
    pub async fn take_lease(&self, holder: &str, ttl: std::time::Duration) -> anyhow::Result<bool> {
        let now = now_ms();
        let taken = sqlx::query(
            "INSERT INTO worker_lease (id, holder, expires_at) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at
             WHERE worker_lease.holder = excluded.holder OR worker_lease.expires_at <= ?3",
        )
        .bind(holder)
        .bind(now.saturating_add(ttl.as_millis() as i64))
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(taken == 1)
    }

    /// Whether `holder` holds the worker lease now.
    pub async fn holds_lease(&self, holder: &str) -> anyhow::Result<bool> {
        let row = sqlx::query("SELECT 1 FROM worker_lease WHERE holder = ? AND expires_at > ?")
            .bind(holder)
            .bind(now_ms())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// Hand the worker lease over at once, on shutdown.
    pub async fn release_lease(&self, holder: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE worker_lease SET expires_at = 0 WHERE holder = ?")
            .bind(holder)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Renew the worker lease every `every` until `stop` changes, then release it.
    ///
    /// This runs apart from the work the lease guards, so a tick longer than `ttl` does not let the
    /// lease lapse mid-tick: the API would refuse boards, and another instance could take over
    /// while this one still works. `holding` says whether the lease was held at the last renewal;
    /// a renewal that fails clears it, so the work pauses before the lease can expire.
    pub async fn keep_lease(
        &self,
        holder: &str,
        ttl: std::time::Duration,
        every: std::time::Duration,
        holding: &AtomicBool,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = stop.changed() => break,
            }
            match self.take_lease(holder, ttl).await {
                Ok(true) => {
                    if !holding.swap(true, Ordering::SeqCst) {
                        log::info!("{holder} runs the swaps");
                    }
                }
                Ok(false) => {
                    if holding.swap(false, Ordering::SeqCst) {
                        log::warn!("another ark-swapd instance took over the swaps");
                    }
                }
                Err(error) => {
                    holding.store(false, Ordering::SeqCst);
                    log::warn!("cannot take the worker lease: {error:#}");
                }
            }
        }
        holding.store(false, Ordering::SeqCst);
        if let Err(error) = self.release_lease(holder).await {
            log::warn!("cannot release the worker lease: {error:#}");
        }
    }

    pub async fn insert(&self, swap: &Swap) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO swaps (id, escrow_address, amount_sat, payment_hash, preimage,
                preimage_ciphertext, invoice, state, escrow_vtxo, ark_txid, error, created_at,
                updated_at, expires_at, vtxo_lookups, vtxo_lookup_after, vtxo_lookup_gave_up_at,
                pay_attempted_at, pay_attempts)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(swap.id.to_string())
        .bind(&swap.escrow_address)
        .bind(swap.amount_sat as i64)
        .bind(&swap.payment_hash)
        // Only the sealed copy is kept. The column predates sealing and cannot be null.
        .bind("")
        .bind(self.seal(
            PreimageRow::Swap,
            swap.id,
            &swap.payment_hash,
            &swap.preimage,
        )?)
        .bind(&swap.invoice)
        .bind(swap.state.as_str())
        .bind(&swap.escrow_vtxo)
        .bind(&swap.ark_txid)
        .bind(&swap.error)
        .bind(swap.created_at)
        .bind(swap.updated_at)
        .bind(swap.expires_at)
        .bind(swap.vtxo_lookups)
        .bind(swap.vtxo_lookup_after)
        .bind(swap.vtxo_lookup_gave_up_at)
        .bind(swap.pay_attempted_at)
        .bind(swap.pay_attempts)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, swap: &Swap) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE swaps SET state = ?, escrow_vtxo = ?, ark_txid = ?, error = ?, updated_at = ?,
                vtxo_lookups = ?, vtxo_lookup_after = ?, vtxo_lookup_gave_up_at = ?,
                pay_attempted_at = ?, pay_attempts = ?
             WHERE id = ?",
        )
        .bind(swap.state.as_str())
        .bind(&swap.escrow_vtxo)
        .bind(&swap.ark_txid)
        .bind(&swap.error)
        .bind(swap.updated_at)
        .bind(swap.vtxo_lookups)
        .bind(swap.vtxo_lookup_after)
        .bind(swap.vtxo_lookup_gave_up_at)
        .bind(swap.pay_attempted_at)
        .bind(swap.pay_attempts)
        .bind(swap.id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: Uuid) -> anyhow::Result<Option<Swap>> {
        let row = sqlx::query("SELECT * FROM swaps WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| self.swap_row(&row)).transpose()
    }

    /// The newest swap for `escrow_address` that has not ended.
    pub async fn open_for_escrow(&self, escrow_address: &str) -> anyhow::Result<Option<Swap>> {
        let rows =
            sqlx::query("SELECT * FROM swaps WHERE escrow_address = ? ORDER BY created_at DESC")
                .bind(escrow_address)
                .fetch_all(&self.pool)
                .await?;
        for row in rows {
            let swap = self.swap_row(&row)?;
            if !swap.state.is_final() {
                return Ok(Some(swap));
            }
        }
        Ok(None)
    }

    /// The swap whose invoice pays to `payment_hash`, open or finished. There is at most one.
    pub async fn for_payment_hash(&self, payment_hash: &str) -> anyhow::Result<Option<Swap>> {
        let row = sqlx::query("SELECT * FROM swaps WHERE payment_hash = ?")
            .bind(payment_hash)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| self.swap_row(&row)).transpose()
    }

    /// Swaps that paid an escrow, or say they did, without recording the escrow output they
    /// paid, oldest first: money a debugger has to find by hand.
    pub async fn without_escrow_vtxo(&self) -> anyhow::Result<Vec<Swap>> {
        let rows = sqlx::query(
            "SELECT * FROM swaps WHERE escrow_vtxo IS NULL
             AND state IN ('escrow_paid', 'settled', 'unsettled') ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| self.swap_row(row)).collect()
    }

    /// Whether any swap, open or finished, used `payment_hash`. LND never reuses one.
    pub async fn payment_hash_used(&self, payment_hash: &str) -> anyhow::Result<bool> {
        let row = sqlx::query("SELECT 1 FROM swaps WHERE payment_hash = ?")
            .bind(payment_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// What the swaps not yet paid promise to pay into escrows, in sats: those awaiting a payment
    /// and those holding one while the escrow is paid.
    pub async fn unpaid_swap_sat(&self) -> anyhow::Result<u64> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(amount_sat), 0) AS promised FROM swaps
             WHERE state IN ('awaiting_payment', 'paying_escrow')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get::<i64, _>("promised")? as u64)
    }

    /// Swaps that have not ended. Those holding a payer's HTLC while the escrow is paid or
    /// settled come first, then those awaiting payment, oldest first within each.
    pub async fn unfinished(&self) -> anyhow::Result<Vec<Swap>> {
        let rows = sqlx::query(
            "SELECT * FROM swaps
             WHERE state IN ('awaiting_payment', 'paying_escrow', 'escrow_paid')
             ORDER BY CASE state WHEN 'escrow_paid' THEN 0 WHEN 'paying_escrow' THEN 1 ELSE 2 END,
                      created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| self.swap_row(row)).collect()
    }

    /// Settled swaps whose escrow VTXO is still to be looked up at `now` (UNIX seconds): they
    /// paid the escrow before the indexer listed it, and the coordinator counts the entry only
    /// once it knows the VTXO. Swaps the service gave up on are left to an operator.
    pub async fn vtxo_lookups_due(&self, now: i64) -> anyhow::Result<Vec<Swap>> {
        let rows = sqlx::query(
            "SELECT * FROM swaps
             WHERE state = 'settled' AND escrow_vtxo IS NULL AND ark_txid IS NOT NULL
               AND vtxo_lookup_gave_up_at IS NULL
               AND (vtxo_lookup_after IS NULL OR vtxo_lookup_after <= ?)
             ORDER BY created_at",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| self.swap_row(row)).collect()
    }

    pub async fn insert_refund(&self, refund: &Refund) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO refunds (id, payment_hash, amount_sat, player_key, deadline,
                swap_tap_tree, swap_address, state, preimage, preimage_ciphertext, swap_vtxo,
                claim_txid, error, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(refund.id.to_string())
        .bind(&refund.payment_hash)
        .bind(refund.amount_sat as i64)
        .bind(&refund.player_key)
        .bind(refund.deadline)
        .bind(&refund.swap_tap_tree)
        .bind(&refund.swap_address)
        .bind(refund.state.as_str())
        .bind(None::<String>)
        .bind(self.seal_refund(refund)?)
        .bind(&refund.swap_vtxo)
        .bind(&refund.claim_txid)
        .bind(&refund.error)
        .bind(refund.created_at)
        .bind(refund.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_refund(&self, refund: &Refund) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE refunds SET state = ?, preimage = ?, preimage_ciphertext = ?, swap_vtxo = ?,
                claim_txid = ?, error = ?, updated_at = ? WHERE id = ?",
        )
        .bind(refund.state.as_str())
        // Only the sealed copy is kept.
        .bind(None::<String>)
        .bind(self.seal_refund(refund)?)
        .bind(&refund.swap_vtxo)
        .bind(&refund.claim_txid)
        .bind(&refund.error)
        .bind(refund.updated_at)
        .bind(refund.id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn refund(&self, id: Uuid) -> anyhow::Result<Option<Refund>> {
        let row = sqlx::query("SELECT * FROM refunds WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(|row| self.refund_row(row)).transpose()
    }

    /// The refund minted for `payment_hash`, if this service already minted one.
    ///
    /// A repeated request returns the same swap, so a coordinator that retries never mints a
    /// second swap for one invoice and strands the first.
    pub async fn refund_for_hash(&self, payment_hash: &str) -> anyhow::Result<Option<Refund>> {
        let row = sqlx::query("SELECT * FROM refunds WHERE payment_hash = ?")
            .bind(payment_hash)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(|row| self.refund_row(row)).transpose()
    }

    /// Refunds still to claim, oldest first: with those past their deadline that paid the
    /// player, whose swap the claim leaf can still take until the player takes it back.
    pub async fn unclaimed_refunds(&self) -> anyhow::Result<Vec<Refund>> {
        let rows = sqlx::query(
            "SELECT * FROM refunds
             WHERE state IN ('minted', 'paid')
                OR (state = 'reclaimable' AND claim_txid IS NULL
                    AND (preimage IS NOT NULL OR preimage_ciphertext IS NOT NULL))
             ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| self.refund_row(row)).collect()
    }

    /// Seal the preimages still stored in plaintext, `batch` rows at a time, and clear the
    /// plaintext; return how many rows were cleared. Releases before this one wrote both, so a
    /// rollback and a redeploy leave such rows. The plaintext of a row already sealed is cleared
    /// only once its sealed copy opens to a preimage that pays to the row's hash. A row that
    /// fails either check is left as it is, and refused when it is read. This can run any number
    /// of times.
    pub async fn seal_plaintext_preimages(&self, batch: u32) -> anyhow::Result<u64> {
        let mut cleared = 0;
        for (row_kind, select, update) in [
            (
                PreimageRow::Swap,
                "SELECT id, payment_hash, preimage, preimage_ciphertext FROM swaps
                 WHERE preimage <> '' AND id > ?
                 ORDER BY id LIMIT ?",
                "UPDATE swaps SET preimage_ciphertext = ?, preimage = '' WHERE id = ?",
            ),
            (
                PreimageRow::Refund,
                "SELECT id, payment_hash, preimage, preimage_ciphertext FROM refunds
                 WHERE preimage IS NOT NULL AND preimage <> '' AND id > ?
                 ORDER BY id LIMIT ?",
                "UPDATE refunds SET preimage_ciphertext = ?, preimage = NULL WHERE id = ?",
            ),
        ] {
            let mut after = String::new();
            loop {
                let rows = sqlx::query(select)
                    .bind(&after)
                    .bind(batch)
                    .fetch_all(&self.pool)
                    .await?;
                let Some(last) = rows.last() else {
                    break;
                };
                after = last.try_get("id")?;
                for row in &rows {
                    let id: &str = row.try_get("id")?;
                    let id = Uuid::parse_str(id)?;
                    let payment_hash: String = row.try_get("payment_hash")?;
                    let preimage: String = row.try_get("preimage")?;
                    let sealed: Option<Vec<u8>> = row.try_get("preimage_ciphertext")?;
                    let ciphertext = match sealed {
                        Some(sealed) => self
                            .preimages
                            .stored(row_kind, id, &payment_hash, Some(sealed.clone()), None)
                            .map(|_| sealed),
                        None => self.seal(row_kind, id, &payment_hash, &preimage),
                    };
                    let ciphertext = match ciphertext {
                        Ok(ciphertext) => ciphertext,
                        Err(error) => {
                            log::error!("cannot seal the stored preimage of {id}: {error:#}");
                            continue;
                        }
                    };
                    cleared += sqlx::query(update)
                        .bind(ciphertext)
                        .bind(id.to_string())
                        .execute(&self.pool)
                        .await?
                        .rows_affected();
                }
                tokio::task::yield_now().await;
            }
        }
        Ok(cleared)
    }

    /// Seal `preimage` (hex) for a row, refusing one that does not pay to its hash.
    fn seal(
        &self,
        row: PreimageRow,
        id: Uuid,
        payment_hash: &str,
        preimage: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let preimage = crate::preimages::from_hex(id, preimage)?;
        crate::preimages::checked(id, payment_hash, &preimage)?;
        self.preimages.seal(row, id, payment_hash, &preimage)
    }

    fn seal_refund(&self, refund: &Refund) -> anyhow::Result<Option<Vec<u8>>> {
        refund
            .preimage
            .as_deref()
            .map(|preimage| {
                self.seal(
                    PreimageRow::Refund,
                    refund.id,
                    &refund.payment_hash,
                    preimage,
                )
            })
            .transpose()
    }

    fn swap_row(&self, row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Swap> {
        swap(row, &self.preimages)
    }

    fn refund_row(&self, row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Refund> {
        refund(row, &self.preimages)
    }
}

/// Where a refund swap is.
///
/// `Minted` → `Paid` → `Claimed`. It ends `Reclaimable` if the service never claimed it before
/// the player's deadline, which needs an operator: by then the player may have taken it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefundState {
    /// Minted and waiting for the refund to pay it.
    Minted,
    /// The coordinator paid the player's invoice and gave up the preimage.
    Paid,
    /// Claimed into this service's wallet.
    Claimed,
    Reclaimable,
}

impl RefundState {
    fn as_str(self) -> &'static str {
        match self {
            RefundState::Minted => "minted",
            RefundState::Paid => "paid",
            RefundState::Claimed => "claimed",
            RefundState::Reclaimable => "reclaimable",
        }
    }
}

impl FromStr for RefundState {
    type Err = anyhow::Error;

    fn from_str(state: &str) -> anyhow::Result<Self> {
        Ok(match state {
            "minted" => RefundState::Minted,
            "paid" => RefundState::Paid,
            "claimed" => RefundState::Claimed,
            "reclaimable" => RefundState::Reclaimable,
            other => anyhow::bail!("unknown refund state {other}"),
        })
    }
}

/// One escrow's refund, on its way to the player's Lightning Address.
#[derive(Debug, Clone, Serialize)]
pub struct Refund {
    pub id: Uuid,
    pub payment_hash: String,
    pub amount_sat: u64,
    pub player_key: String,
    pub deadline: i64,
    pub swap_tap_tree: String,
    pub swap_address: String,
    pub state: RefundState,
    /// Never leaves the service: it is what claims the swap.
    #[serde(skip)]
    pub preimage: Option<String>,
    pub swap_vtxo: Option<String>,
    pub claim_txid: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn swap(row: &sqlx::sqlite::SqliteRow, preimages: &PreimageKey) -> anyhow::Result<Swap> {
    let id = Uuid::parse_str(row.try_get("id")?)?;
    let payment_hash: String = row.try_get("payment_hash")?;
    let preimage = preimages
        .stored(
            PreimageRow::Swap,
            id,
            &payment_hash,
            row.try_get("preimage_ciphertext")?,
            row.try_get("preimage")?,
        )?
        .with_context(|| format!("swap {id} has no preimage"))?;
    Ok(Swap {
        id,
        escrow_address: row.try_get("escrow_address")?,
        amount_sat: row.try_get::<i64, _>("amount_sat")? as u64,
        payment_hash,
        preimage,
        invoice: row.try_get("invoice")?,
        state: row.try_get::<&str, _>("state")?.parse()?,
        escrow_vtxo: row.try_get("escrow_vtxo")?,
        ark_txid: row.try_get("ark_txid")?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        expires_at: row.try_get("expires_at")?,
        vtxo_lookups: row.try_get::<i64, _>("vtxo_lookups")? as u32,
        vtxo_lookup_after: row.try_get("vtxo_lookup_after")?,
        vtxo_lookup_gave_up_at: row.try_get("vtxo_lookup_gave_up_at")?,
        pay_attempted_at: row.try_get("pay_attempted_at")?,
        pay_attempts: row.try_get::<i64, _>("pay_attempts")? as u32,
    })
}

fn refund(row: &sqlx::sqlite::SqliteRow, preimages: &PreimageKey) -> anyhow::Result<Refund> {
    let id = Uuid::parse_str(row.try_get("id")?)?;
    let payment_hash: String = row.try_get("payment_hash")?;
    let preimage = preimages.stored(
        PreimageRow::Refund,
        id,
        &payment_hash,
        row.try_get("preimage_ciphertext")?,
        row.try_get("preimage")?,
    )?;
    Ok(Refund {
        id,
        payment_hash,
        amount_sat: row.try_get::<i64, _>("amount_sat")? as u64,
        player_key: row.try_get("player_key")?,
        deadline: row.try_get("deadline")?,
        swap_tap_tree: row.try_get("swap_tap_tree")?,
        swap_address: row.try_get("swap_address")?,
        state: row.try_get::<&str, _>("state")?.parse()?,
        preimage,
        swap_vtxo: row.try_get("swap_vtxo")?,
        claim_txid: row.try_get("claim_txid")?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::time::Duration;

    fn test_key() -> PreimageKey {
        PreimageKey::from_wallet_secret(&[1u8; 32])
    }

    async fn open(path: &Path) -> Store {
        Store::open(path, test_key()).await.unwrap()
    }

    /// The preimage `byte` repeated, and its payment hash, both hex.
    fn preimage_and_hash(byte: u8) -> (String, String) {
        let preimage = [byte; 32];
        (hex::encode(preimage), hex::encode(Sha256::digest(preimage)))
    }

    fn refund_for(payment_hash: &str) -> Refund {
        Refund {
            id: Uuid::now_v7(),
            payment_hash: payment_hash.into(),
            amount_sat: 20_000,
            player_key: "14".repeat(32),
            deadline: 1_790_003_600,
            swap_tap_tree: "aa".into(),
            swap_address: "tark1refund".into(),
            state: RefundState::Minted,
            preimage: None,
            swap_vtxo: None,
            claim_txid: None,
            error: None,
            created_at: 1_790_000_000,
            updated_at: 1_790_000_000,
        }
    }

    #[tokio::test]
    async fn a_refund_is_claimed_once_and_kept_per_invoice() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(&directory.path().join("swaps.sqlite")).await;
        let (preimage, payment_hash) = preimage_and_hash(0xef);
        let mut refund = refund_for(&payment_hash);
        store.insert_refund(&refund).await.unwrap();

        // One swap per invoice: a retried request finds the one already minted.
        assert_eq!(
            store
                .refund_for_hash(&refund.payment_hash)
                .await
                .unwrap()
                .map(|found| found.id),
            Some(refund.id)
        );
        assert!(store
            .insert_refund(&refund_for(&refund.payment_hash))
            .await
            .is_err());
        assert!(store
            .refund_for_hash(&"cd".repeat(32))
            .await
            .unwrap()
            .is_none());

        // It waits to be claimed while it is minted or paid, and not afterwards.
        let unclaimed = |store: Store| async move {
            store
                .unclaimed_refunds()
                .await
                .unwrap()
                .into_iter()
                .map(|refund| refund.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(unclaimed(store.clone()).await, vec![refund.id]);
        refund.preimage = Some(preimage);
        refund.state = RefundState::Paid;
        store.update_refund(&refund).await.unwrap();
        assert_eq!(unclaimed(store.clone()).await, vec![refund.id]);

        refund.state = RefundState::Claimed;
        refund.claim_txid = Some("00".repeat(32));
        store.update_refund(&refund).await.unwrap();
        assert!(unclaimed(store.clone()).await.is_empty());

        // The preimage is kept for the operator, but never served.
        let stored = store.refund(refund.id).await.unwrap().unwrap();
        assert_eq!(stored.preimage, refund.preimage);
        assert_eq!(stored.state, RefundState::Claimed);
        let served = serde_json::to_value(&stored).unwrap();
        assert!(served.get("preimage").is_none(), "{served}");
    }

    /// A swap whose preimage is `byte` repeated.
    fn swap_for(byte: u8, created_at: i64) -> Swap {
        let (preimage, payment_hash) = preimage_and_hash(byte);
        Swap {
            id: Uuid::now_v7(),
            escrow_address: "tark1escrow".into(),
            amount_sat: 6_300,
            payment_hash,
            preimage,
            invoice: "lntb1".into(),
            state: SwapState::AwaitingPayment,
            escrow_vtxo: None,
            ark_txid: None,
            error: None,
            created_at,
            updated_at: created_at,
            expires_at: created_at + 600,
            vtxo_lookups: 0,
            vtxo_lookup_after: None,
            vtxo_lookup_gave_up_at: None,
            pay_attempted_at: None,
            pay_attempts: 0,
        }
    }

    #[tokio::test]
    async fn swaps_holding_a_payment_come_before_those_awaiting_one() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(&directory.path().join("swaps.sqlite")).await;
        let awaiting = swap_for(0xa1, 1_790_000_000);
        let mut paying = swap_for(0xa2, 1_790_000_100);
        paying.state = SwapState::PayingEscrow;
        let mut paid = swap_for(0xa3, 1_790_000_200);
        paid.state = SwapState::EscrowPaid;
        let mut settled = swap_for(0xa4, 1_789_999_000);
        settled.state = SwapState::Settled;
        settled.ark_txid = Some("ef".repeat(32));
        for swap in [&awaiting, &paying, &paid, &settled] {
            store.insert(swap).await.unwrap();
        }
        let order: Vec<Uuid> = store
            .unfinished()
            .await
            .unwrap()
            .into_iter()
            .map(|swap| swap.id)
            .collect();
        assert_eq!(
            order,
            vec![paid.id, paying.id, awaiting.id],
            "a settled swap is not live, even without its VTXO"
        );
    }

    #[tokio::test]
    async fn unpaid_swaps_promise_their_amounts_until_they_pay() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(&directory.path().join("swaps.sqlite")).await;
        assert_eq!(store.unpaid_swap_sat().await.unwrap(), 0);
        let awaiting = swap_for(0xb1, 1_790_000_000);
        let mut paying = swap_for(0xb2, 1_790_000_100);
        paying.state = SwapState::PayingEscrow;
        paying.amount_sat = 7_000;
        let mut paid = swap_for(0xb3, 1_790_000_200);
        paid.state = SwapState::EscrowPaid;
        let mut expired = swap_for(0xb4, 1_789_999_000);
        expired.state = SwapState::Expired;
        for swap in [&awaiting, &paying, &paid, &expired] {
            store.insert(swap).await.unwrap();
        }
        assert_eq!(store.unpaid_swap_sat().await.unwrap(), 6_300 + 7_000);
    }

    #[tokio::test]
    async fn an_escrow_payment_is_recorded_before_it_is_sent() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(&directory.path().join("swaps.sqlite")).await;
        let mut swap = swap_for(0xef, 1_790_000_000);
        swap.state = SwapState::PayingEscrow;
        store.insert(&swap).await.unwrap();
        swap.pay_attempted_at = Some(1_790_000_005);
        swap.pay_attempts = 1;
        store.update(&swap).await.unwrap();
        let stored = store.get(swap.id).await.unwrap().unwrap();
        assert_eq!(
            (stored.pay_attempted_at, stored.pay_attempts),
            (Some(1_790_000_005), 1),
            "a restart sees the payment that may have been sent"
        );
    }

    #[tokio::test]
    async fn a_settled_swap_is_looked_up_on_its_backoff_until_found_or_given_up() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(&directory.path().join("swaps.sqlite")).await;
        let now = 1_790_000_000;
        let due = |store: Store, at: i64| async move {
            store
                .vtxo_lookups_due(at)
                .await
                .unwrap()
                .into_iter()
                .map(|swap| swap.id)
                .collect::<Vec<_>>()
        };
        let mut swap = swap_for(0xab, now - 60);
        store.insert(&swap).await.unwrap();
        assert!(due(store.clone(), now).await.is_empty(), "not settled yet");

        // Paid and settled before the indexer listed the VTXO: looked up at once.
        swap.state = SwapState::Settled;
        swap.ark_txid = Some("ef".repeat(32));
        store.update(&swap).await.unwrap();
        assert_eq!(due(store.clone(), now).await, vec![swap.id]);

        // A lookup that missed waits out its backoff.
        swap.vtxo_lookups = 1;
        swap.vtxo_lookup_after = Some(now + 10);
        store.update(&swap).await.unwrap();
        assert!(due(store.clone(), now).await.is_empty());
        assert_eq!(due(store.clone(), now + 10).await, vec![swap.id]);

        let stored = store.get(swap.id).await.unwrap().unwrap();
        assert_eq!(
            (stored.vtxo_lookups, stored.vtxo_lookup_after),
            (1, Some(now + 10))
        );

        // Found: nothing left to look up.
        swap.escrow_vtxo = Some(format!("{}:0", "ef".repeat(32)));
        store.update(&swap).await.unwrap();
        assert!(due(store.clone(), now + 10).await.is_empty());

        // Given up: left to an operator, never looked up again.
        let mut abandoned = swap_for(0xcd, now);
        abandoned.state = SwapState::Settled;
        abandoned.ark_txid = Some("12".repeat(32));
        abandoned.vtxo_lookup_gave_up_at = Some(now);
        store.insert(&abandoned).await.unwrap();
        assert!(due(store.clone(), now + 1_000_000).await.is_empty());

        // A swap that ended without paying an escrow has nothing to look up.
        let mut expired = swap_for(0x34, now);
        expired.state = SwapState::Expired;
        store.insert(&expired).await.unwrap();
        assert!(due(store, now + 1_000_000).await.is_empty());
    }

    /// Whoever paid an invoice can find the swap it funded, and the escrow it paid, from the
    /// payment hash alone.
    #[tokio::test]
    async fn a_swap_is_found_by_its_invoice_payment_hash() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(&directory.path().join("swaps.sqlite")).await;
        let mut swap = swap_for(0xab, 1_790_000_000);
        swap.amount_sat = 1_100;
        swap.state = SwapState::Settled;
        swap.escrow_vtxo = Some(format!("{}:0", "ef".repeat(32)));
        swap.ark_txid = Some("ef".repeat(32));
        store.insert(&swap).await.unwrap();

        let found = store
            .for_payment_hash(&swap.payment_hash)
            .await
            .unwrap()
            .expect("the swap is found by its hash");
        assert_eq!(found.id, swap.id);
        assert_eq!(found.escrow_vtxo, swap.escrow_vtxo);
        assert!(store
            .for_payment_hash(&"00".repeat(32))
            .await
            .unwrap()
            .is_none());
        let served = serde_json::to_value(&found).unwrap();
        assert!(served.get("preimage").is_none(), "{served}");

        // A swap that paid its escrow without recording the output is listed; one that
        // recorded it, or never paid, is not.
        assert!(store.without_escrow_vtxo().await.unwrap().is_empty());
        let (preimage, payment_hash) = preimage_and_hash(0x12);
        let unrecorded = Swap {
            id: Uuid::now_v7(),
            payment_hash,
            preimage,
            escrow_vtxo: None,
            ..swap.clone()
        };
        store.insert(&unrecorded).await.unwrap();
        let (preimage, payment_hash) = preimage_and_hash(0x34);
        let waiting = Swap {
            id: Uuid::now_v7(),
            payment_hash,
            preimage,
            escrow_vtxo: None,
            state: SwapState::AwaitingPayment,
            ..swap.clone()
        };
        store.insert(&waiting).await.unwrap();
        let listed = store.without_escrow_vtxo().await.unwrap();
        assert_eq!(
            listed.iter().map(|swap| swap.id).collect::<Vec<_>>(),
            [unrecorded.id]
        );
    }

    async fn stored_preimages(
        store: &Store,
        table: &str,
        id: Uuid,
    ) -> (Option<String>, Option<Vec<u8>>) {
        let row = sqlx::query(&format!(
            "SELECT preimage, preimage_ciphertext FROM {table} WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        (
            row.try_get("preimage").unwrap(),
            row.try_get("preimage_ciphertext").unwrap(),
        )
    }

    #[tokio::test]
    async fn preimages_are_stored_only_sealed() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(&directory.path().join("swaps.sqlite")).await;
        let swap = swap_for(0x51, 1_790_000_000);
        store.insert(&swap).await.unwrap();
        let (plaintext, sealed) = stored_preimages(&store, "swaps", swap.id).await;
        assert_eq!(plaintext.as_deref(), Some(""));
        let sealed = sealed.expect("sealed on insert");
        assert!(!hex::encode(sealed).contains(&swap.preimage));
        assert_eq!(
            store.get(swap.id).await.unwrap().unwrap().preimage,
            swap.preimage
        );

        let (preimage, payment_hash) = preimage_and_hash(0x52);
        let mut refund = refund_for(&payment_hash);
        store.insert_refund(&refund).await.unwrap();
        assert_eq!(
            stored_preimages(&store, "refunds", refund.id).await,
            (None, None)
        );
        refund.preimage = Some(preimage.clone());
        store.update_refund(&refund).await.unwrap();
        let (plaintext, sealed) = stored_preimages(&store, "refunds", refund.id).await;
        assert_eq!(plaintext, None);
        assert!(sealed.is_some());
        assert_eq!(
            store.refund(refund.id).await.unwrap().unwrap().preimage,
            Some(preimage)
        );

        // A preimage that does not pay to the row's hash is never written.
        let mut wrong = swap_for(0x53, 1_790_000_000);
        wrong.preimage = "54".repeat(32);
        assert!(store.insert(&wrong).await.is_err());
        refund.preimage = Some("54".repeat(32));
        assert!(store.update_refund(&refund).await.is_err());
    }

    #[tokio::test]
    async fn plaintext_preimages_are_read_until_sealed_then_cleared_once() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(&directory.path().join("swaps.sqlite")).await;
        let mut swaps = Vec::new();
        for byte in 0x60..0x65 {
            let swap = swap_for(byte, 1_790_000_000);
            store.insert(&swap).await.unwrap();
            swaps.push(swap);
        }
        let (preimage, payment_hash) = preimage_and_hash(0x66);
        let mut refund = refund_for(&payment_hash);
        refund.preimage = Some(preimage.clone());
        refund.state = RefundState::Paid;
        store.insert_refund(&refund).await.unwrap();
        let unpaid = refund_for(&preimage_and_hash(0x67).1);
        store.insert_refund(&unpaid).await.unwrap();

        // As earlier releases left them: plaintext only, or plaintext and a sealed copy.
        for (index, swap) in swaps.iter().enumerate() {
            let clear_sealed = if index < 3 {
                ", preimage_ciphertext = NULL"
            } else {
                ""
            };
            sqlx::query(&format!(
                "UPDATE swaps SET preimage = ?{clear_sealed} WHERE id = ?"
            ))
            .bind(&swap.preimage)
            .bind(swap.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE refunds SET preimage = ?, preimage_ciphertext = NULL WHERE id = ?")
            .bind(&preimage)
            .bind(refund.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        for swap in &swaps {
            assert_eq!(
                store.get(swap.id).await.unwrap().unwrap().preimage,
                swap.preimage
            );
        }
        assert_eq!(
            store.refund(refund.id).await.unwrap().unwrap().preimage,
            Some(preimage.clone())
        );

        // Sealed and cleared in batches smaller than the table, then nothing left to do.
        assert_eq!(store.seal_plaintext_preimages(2).await.unwrap(), 6);
        assert_eq!(store.seal_plaintext_preimages(2).await.unwrap(), 0);
        for swap in &swaps {
            let (plaintext, sealed) = stored_preimages(&store, "swaps", swap.id).await;
            assert_eq!(plaintext.as_deref(), Some(""));
            assert!(sealed.is_some());
            assert_eq!(
                store.get(swap.id).await.unwrap().unwrap().preimage,
                swap.preimage
            );
        }
        let (plaintext, sealed) = stored_preimages(&store, "refunds", refund.id).await;
        assert_eq!(plaintext, None);
        assert!(sealed.is_some());
        assert_eq!(
            stored_preimages(&store, "refunds", unpaid.id).await,
            (None, None)
        );
        assert_eq!(
            store.refund(refund.id).await.unwrap().unwrap().preimage,
            Some(preimage)
        );
    }

    #[tokio::test]
    async fn a_preimage_that_does_not_match_its_row_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("swaps.sqlite");
        let store = open(&path).await;
        let first = swap_for(0x71, 1_790_000_000);
        let second = swap_for(0x72, 1_790_000_000);
        store.insert(&first).await.unwrap();
        store.insert(&second).await.unwrap();

        // Another wallet's key opens none of them.
        let other = Store::open(&path, PreimageKey::from_wallet_secret(&[2u8; 32]))
            .await
            .unwrap();
        assert!(other.get(first.id).await.is_err());

        // A sealed preimage moved to another row does not open there.
        sqlx::query(
            "UPDATE swaps SET preimage_ciphertext =
                (SELECT preimage_ciphertext FROM swaps WHERE id = ?1) WHERE id = ?2",
        )
        .bind(first.id.to_string())
        .bind(second.id.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(store.get(second.id).await.is_err());
        assert!(store.get(first.id).await.is_ok());

        // A plaintext preimage that does not pay to the hash is refused, and left unsealed.
        sqlx::query("UPDATE swaps SET preimage = ?, preimage_ciphertext = NULL WHERE id = ?")
            .bind("73".repeat(32))
            .bind(first.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.get(first.id).await.is_err());

        // The plaintext of a row whose sealed copy does not open is kept.
        sqlx::query("UPDATE swaps SET preimage = ? WHERE id = ?")
            .bind(&second.preimage)
            .bind(second.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.get(second.id).await.is_err());

        // Neither is sealed or cleared.
        assert_eq!(store.seal_plaintext_preimages(10).await.unwrap(), 0);
        assert_eq!(
            stored_preimages(&store, "swaps", first.id).await,
            (Some("73".repeat(32)), None)
        );
        assert_eq!(
            stored_preimages(&store, "swaps", second.id).await.0,
            Some(second.preimage.clone())
        );
    }

    #[tokio::test]
    async fn a_database_a_later_release_migrated_still_opens() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("swaps.sqlite");
        let store = open(&path).await;
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
             VALUES (99999, 'a later release', 1, x'00', 0)",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        open(&path).await;
    }

    #[tokio::test]
    async fn one_instance_runs_the_swaps_until_it_hands_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("swaps.sqlite");
        let blue = open(&path).await;
        let green = open(&path).await;
        let ttl = Duration::from_secs(15);

        assert!(blue.take_lease("blue", ttl).await.unwrap());
        assert!(!green.take_lease("green", ttl).await.unwrap());
        assert!(
            blue.take_lease("blue", ttl).await.unwrap(),
            "the holder renews"
        );
        assert!(blue.holds_lease("blue").await.unwrap());
        assert!(!green.holds_lease("green").await.unwrap());

        blue.release_lease("blue").await.unwrap();
        assert!(green.take_lease("green", ttl).await.unwrap());
        assert!(!blue.take_lease("blue", ttl).await.unwrap());

        // A holder that stops renewing loses the lease when it expires.
        green.release_lease("green").await.unwrap();
        assert!(blue
            .take_lease("blue", Duration::from_millis(50))
            .await
            .unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(green.take_lease("green", ttl).await.unwrap());
    }

    #[tokio::test]
    async fn the_lease_is_kept_through_work_that_outlasts_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("swaps.sqlite");
        let blue = open(&path).await;
        let green = open(&path).await;
        let ttl = Duration::from_secs(1);
        let holding = std::sync::Arc::new(AtomicBool::new(false));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let keeper = {
            let blue = blue.clone();
            let holding = holding.clone();
            tokio::spawn(async move {
                blue.keep_lease("blue", ttl, Duration::from_millis(50), &holding, stopped)
                    .await
            })
        };

        // Work three times as long as the lease lives, as a slow tick is.
        tokio::time::sleep(ttl * 3).await;
        assert!(holding.load(Ordering::SeqCst));
        assert!(blue.holds_lease("blue").await.unwrap());
        assert!(!green.take_lease("green", ttl).await.unwrap());

        // Stopping hands the lease over at once.
        stop.send(true).unwrap();
        keeper.await.unwrap();
        assert!(!holding.load(Ordering::SeqCst));
        assert!(green.take_lease("green", ttl).await.unwrap());
    }
}
