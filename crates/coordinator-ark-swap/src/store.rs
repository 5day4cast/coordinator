//! Swaps, persisted in SQLite so a restart resumes every swap where it stopped.

use std::path::Path;
use std::str::FromStr;

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use uuid::Uuid;

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
    /// Never leaves the service: revealing it lets anyone settle the invoice.
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
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
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

    pub async fn insert(&self, swap: &Swap) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO swaps (id, escrow_address, amount_sat, payment_hash, preimage, invoice, state,
                escrow_vtxo, ark_txid, error, created_at, updated_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(swap.id.to_string())
        .bind(&swap.escrow_address)
        .bind(swap.amount_sat as i64)
        .bind(&swap.payment_hash)
        .bind(&swap.preimage)
        .bind(&swap.invoice)
        .bind(swap.state.as_str())
        .bind(&swap.escrow_vtxo)
        .bind(&swap.ark_txid)
        .bind(&swap.error)
        .bind(swap.created_at)
        .bind(swap.updated_at)
        .bind(swap.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, swap: &Swap) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE swaps SET state = ?, escrow_vtxo = ?, ark_txid = ?, error = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(swap.state.as_str())
        .bind(&swap.escrow_vtxo)
        .bind(&swap.ark_txid)
        .bind(&swap.error)
        .bind(swap.updated_at)
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
        row.map(|row| swap(&row)).transpose()
    }

    /// The newest swap for `escrow_address` that has not ended.
    pub async fn open_for_escrow(&self, escrow_address: &str) -> anyhow::Result<Option<Swap>> {
        let rows =
            sqlx::query("SELECT * FROM swaps WHERE escrow_address = ? ORDER BY created_at DESC")
                .bind(escrow_address)
                .fetch_all(&self.pool)
                .await?;
        for row in rows {
            let swap = swap(&row)?;
            if !swap.state.is_final() {
                return Ok(Some(swap));
            }
        }
        Ok(None)
    }

    /// Whether any swap, open or finished, used `payment_hash`. LND never reuses one.
    pub async fn payment_hash_used(&self, payment_hash: &str) -> anyhow::Result<bool> {
        let row = sqlx::query("SELECT 1 FROM swaps WHERE payment_hash = ?")
            .bind(payment_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    pub async fn unfinished(&self) -> anyhow::Result<Vec<Swap>> {
        let rows = sqlx::query(
            "SELECT * FROM swaps WHERE state IN ('awaiting_payment', 'paying_escrow', 'escrow_paid')
             ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(swap).collect()
    }
}

fn swap(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Swap> {
    Ok(Swap {
        id: Uuid::parse_str(row.try_get("id")?)?,
        escrow_address: row.try_get("escrow_address")?,
        amount_sat: row.try_get::<i64, _>("amount_sat")? as u64,
        payment_hash: row.try_get("payment_hash")?,
        preimage: row.try_get("preimage")?,
        invoice: row.try_get("invoice")?,
        state: row.try_get::<&str, _>("state")?.parse()?,
        escrow_vtxo: row.try_get("escrow_vtxo")?,
        ark_txid: row.try_get("ark_txid")?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        expires_at: row.try_get("expires_at")?,
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
    use std::time::Duration;

    #[tokio::test]
    async fn one_instance_runs_the_swaps_until_it_hands_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("swaps.sqlite");
        let blue = Store::open(&path).await.unwrap();
        let green = Store::open(&path).await.unwrap();
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
}
