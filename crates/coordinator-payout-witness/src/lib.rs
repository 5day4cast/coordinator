//! Permanent uniqueness evidence. Only a completed SQLite commit permits a receipt.
use anyhow::{ensure, Context, Result};
use coordinator_escrow::payout_witness::{Occupancy, Reservation, WitnessError};
use serde::Deserialize;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    Row, Sqlite, SqlitePool, Transaction,
};
use std::{
    io::{BufRead, BufReader},
    path::Path,
    time::Duration,
};
use uuid::Uuid;

const SCHEMA: &str = "
CREATE TABLE metadata (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    ledger_id TEXT NOT NULL,
    history_checkpoint TEXT NOT NULL,
    payment_hashes INTEGER NOT NULL DEFAULT 0,
    released_entries INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE payment_hashes (
    payment_hash BLOB PRIMARY KEY CHECK(length(payment_hash) = 32),
    session_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    claim_id TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE released_entries (
    session_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    claim_id TEXT NOT NULL,
    PRIMARY KEY(session_id, user_id)
) WITHOUT ROWID;
";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapHeader {
    pub ledger_id: Uuid,
    /// Operator declaration identifying the complete, reconciled historical inventory.
    /// This is a trust assertion, not cryptographic proof of completeness.
    pub complete_history_checkpoint: String,
}

pub struct Ledger {
    pool: SqlitePool,
    pub id: Uuid,
}

async fn connect_existing(path: &Path) -> Result<SqlitePool> {
    ensure!(
        path.is_file(),
        "Witness database is missing; refusing automatic initialization"
    );
    Ok(SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(false)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Full)
                .busy_timeout(Duration::from_secs(2)),
        )
        .await?)
}

impl Ledger {
    pub async fn open(path: &Path) -> Result<Self> {
        let pool = connect_existing(path).await?;
        let id: String = sqlx::query_scalar("SELECT ledger_id FROM metadata WHERE singleton = 1")
            .fetch_one(&pool)
            .await
            .context("Witness bootstrap is incomplete")?;
        Ok(Self {
            pool,
            id: id.parse()?,
        })
    }

    pub async fn occupancy(&self) -> Result<Occupancy> {
        let row = sqlx::query(
            "SELECT payment_hashes, released_entries FROM metadata WHERE singleton = 1",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(Occupancy {
            payment_hashes: u64::try_from(row.try_get::<i64, _>("payment_hashes")?)?,
            released_entries: u64::try_from(row.try_get::<i64, _>("released_entries")?)?,
        })
    }

    pub async fn reserve(
        &self,
        ledger_id: Uuid,
        reservation: &Reservation,
    ) -> Result<Occupancy, WitnessError> {
        if ledger_id != self.id {
            return Err(WitnessError::WrongLedger);
        }
        self.commit_reservation(reservation)
            .await
            .map_err(storage_error)?
    }

    async fn commit_reservation(
        &self,
        reservation: &Reservation,
    ) -> Result<Result<Occupancy, WitnessError>, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        match reserve_in_transaction(&mut transaction, reservation, false).await {
            Ok(Ok(occupancy)) => {
                transaction.commit().await?;
                Ok(Ok(occupancy))
            }
            Ok(Err(conflict)) => {
                transaction.rollback().await?;
                Ok(Err(conflict))
            }
            Err(error) => Err(rollback_after_error(transaction, error).await),
        }
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

/// SQLite can automatically roll back on SQLITE_FULL. Retain that typed cause
/// even if a subsequent explicit rollback fails; report the secondary failure.
async fn rollback_after_error<E>(transaction: Transaction<'_, Sqlite>, error: E) -> E {
    if transaction.rollback().await.is_err() {
        eprintln!("Payout witness rollback failed; retaining the original storage error");
    }
    error
}

fn storage_error(error: sqlx::Error) -> WitnessError {
    if error
        .as_database_error()
        .and_then(|error| error.code())
        .as_deref()
        == Some("13")
    {
        WitnessError::Capacity
    } else {
        WitnessError::Unavailable
    }
}

async fn reserve_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    reservation: &Reservation,
    historical: bool,
) -> Result<Result<Occupancy, WitnessError>, sqlx::Error> {
    let session = reservation.claim.session_id.to_string();
    let user = reservation.claim.user_id.to_string();
    let claim = reservation.claim.claim_id.to_string();
    let prior = sqlx::query(
        "SELECT session_id, user_id, claim_id FROM payment_hashes WHERE payment_hash = ?",
    )
    .bind(reservation.payment_hash.as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(owner) = &prior {
        if owner.try_get::<String, _>("session_id")? != session
            || owner.try_get::<String, _>("user_id")? != user
            || owner.try_get::<String, _>("claim_id")? != claim
        {
            return Ok(Err(WitnessError::Conflict));
        }
    }
    let released: Option<String> = sqlx::query_scalar(
        "SELECT claim_id FROM released_entries WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session)
    .bind(&user)
    .fetch_optional(&mut **transaction)
    .await?;
    // A trusted bootstrap can include earlier unpaid attempts for an entry
    // later released under a different claim. Retain their hashes permanently.
    if (!historical || reservation.executing)
        && released.as_ref().is_some_and(|owner| *owner != claim)
    {
        return Ok(Err(WitnessError::Conflict));
    }
    if prior.is_none() {
        sqlx::query("INSERT INTO payment_hashes (payment_hash, session_id, user_id, claim_id) VALUES (?, ?, ?, ?)")
            .bind(reservation.payment_hash.as_slice()).bind(&session).bind(&user).bind(&claim)
            .execute(&mut **transaction).await?;
    }
    let newly_released = reservation.executing && released.is_none();
    if newly_released {
        sqlx::query(
            "INSERT INTO released_entries (session_id, user_id, claim_id) VALUES (?, ?, ?)",
        )
        .bind(&session)
        .bind(&user)
        .bind(&claim)
        .execute(&mut **transaction)
        .await?;
    }
    let row = sqlx::query("UPDATE metadata SET payment_hashes = payment_hashes + ?, released_entries = released_entries + ? WHERE singleton = 1 RETURNING payment_hashes, released_entries")
        .bind(i64::from(prior.is_none())).bind(i64::from(newly_released))
        .fetch_one(&mut **transaction).await?;
    Ok(Ok(Occupancy {
        payment_hashes: row.try_get::<i64, _>("payment_hashes")? as u64,
        released_entries: row.try_get::<i64, _>("released_entries")? as u64,
    }))
}

/// Initialization is an explicit trusted administration operation. Serving never
/// calls it. The complete import and checkpoint become visible in one commit.
pub async fn initialize(database: &Path, inventory: &Path) -> Result<Occupancy> {
    let mut source = BufReader::new(std::fs::File::open(inventory)?);
    let header: BootstrapHeader = serde_json::from_str(&inventory_line(&mut source)?)?;
    ensure!(
        !header.ledger_id.is_nil(),
        "Ledger identity must not be nil"
    );
    ensure!(
        (8..=512).contains(&header.complete_history_checkpoint.len()),
        "A complete-history checkpoint declaration is required"
    );
    create_database_file(database)?;
    let pool = connect_existing(database).await?;
    let result = import_inventory(&pool, header, &mut source).await;
    pool.close().await;
    result
}

fn create_database_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    Ok(())
}

fn inventory_line(reader: &mut impl BufRead) -> Result<String> {
    use std::io::Read;
    let mut line = String::new();
    reader.take(8193).read_line(&mut line)?;
    ensure!(line.len() <= 8192, "Inventory line exceeds size limit");
    Ok(line)
}

async fn import_inventory(
    pool: &SqlitePool,
    header: BootstrapHeader,
    source: &mut impl BufRead,
) -> Result<Occupancy> {
    let mut transaction = pool.begin().await?;
    let result = import_rows(&mut transaction, header, source).await;
    match result {
        Ok(occupancy) => {
            transaction.commit().await?;
            Ok(occupancy)
        }
        Err(error) => Err(rollback_after_error(transaction, error).await),
    }
}

async fn import_rows(
    transaction: &mut Transaction<'_, Sqlite>,
    header: BootstrapHeader,
    source: &mut impl BufRead,
) -> Result<Occupancy> {
    sqlx::raw_sql(SCHEMA).execute(&mut **transaction).await?;
    sqlx::query("INSERT INTO metadata (singleton, ledger_id, history_checkpoint) VALUES (1, ?, ?)")
        .bind(header.ledger_id.to_string())
        .bind(header.complete_history_checkpoint)
        .execute(&mut **transaction)
        .await?;
    let mut occupancy = Occupancy {
        payment_hashes: 0,
        released_entries: 0,
    };
    loop {
        let line = inventory_line(source)?;
        if line.is_empty() {
            return Ok(occupancy);
        }
        let reservation: Reservation = serde_json::from_str(&line)?;
        occupancy = reserve_in_transaction(transaction, &reservation, true).await??;
    }
}

#[cfg(test)]
mod tests;
