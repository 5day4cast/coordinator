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
