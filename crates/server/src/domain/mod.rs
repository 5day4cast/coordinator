mod competitions;
mod invoices;
mod users;

pub use competitions::*;
pub use invoices::*;
use time::OffsetDateTime;
pub use users::*;

use duckdb::{AccessMode, Config, Connection};
use log::info;
use std::time::Duration as StdDuration;
use thiserror::Error;
use tokio::time::timeout;

use crate::OracleError;

#[derive(Error, Debug)]
pub enum Error {
    #[error("item not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("problem querying db: {0}")]
    DbError(#[from] duckdb::Error),
    #[error("{0}")]
    OracleFailed(#[from] OracleError),
    #[error("invalid signature for request")]
    InvalidSignature(String),
    #[error("invalid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("background thread died: {0}")]
    Thread(String),
    #[error("internal error")]
    Bitcoin(#[from] anyhow::Error),
    #[error("signing error: {0}")]
    SigningError(String),
    #[error("Failed to create hold invoice: {0}")]
    HoldError(anyhow::Error),
    #[error("Failed to create or manage Lightning invoice: {0}")]
    LnError(anyhow::Error),
    #[error("Competition full, total_allowed_entries matches total_entries")]
    CompetitionFull,
    #[error("No ticket available for competition")]
    NoAvailableTickets,
    #[error("Too late to sign with ticket. Signing must end by {0}, but current time is {1}")]
    TooLateToSign(OffsetDateTime, OffsetDateTime),
    #[error("Payout payment failed: {0}")]
    PaymentFailed(String),
}

#[derive(Clone)]
pub struct DBConnection {
    pub connection_path: String,
    retry_duration: StdDuration,
    retry_max_attemps: i32,
}

impl DBConnection {
    pub fn new(path: &str, db_name: &str) -> Result<Self, duckdb::Error> {
        let connection_path = format!("{}/{}.db3", path, db_name);
        Ok(Self {
            connection_path,
            retry_duration: StdDuration::from_millis(100),
            retry_max_attemps: 5,
        })
    }

    async fn new_readonly_connection(&self) -> Result<Connection, duckdb::Error> {
        let config = Config::default().access_mode(AccessMode::ReadOnly)?;
        Connection::open_with_flags(self.connection_path.clone(), config)
    }

    pub async fn new_readonly_connection_retry(&self) -> Result<Connection, duckdb::Error> {
        let mut attempt = 0;
        loop {
            match timeout(self.retry_duration, self.new_readonly_connection()).await {
                Ok(Ok(connection)) => return Ok(connection),
                Ok(Err(e)) => {
                    if attempt >= self.retry_max_attemps
                        || !e.to_string().contains("Could not set lock on file")
                    {
                        return Err(e);
                    }
                    info!("Retrying: {}", e);
                    attempt += 1;
                }
                Err(_) => {
                    return Err(duckdb::Error::DuckDBFailure(
                        duckdb::ffi::Error {
                            code: duckdb::ErrorCode::DatabaseLocked,
                            extended_code: 0,
                        },
                        None,
                    ));
                }
            }
        }
    }

    async fn new_write_connection(&self) -> Result<Connection, duckdb::Error> {
        let config = Config::default().access_mode(AccessMode::ReadWrite)?;
        Connection::open_with_flags(self.connection_path.clone(), config)
    }

    pub async fn new_write_connection_retry(&self) -> Result<Connection, duckdb::Error> {
        let mut attempt = 0;
        loop {
            match timeout(self.retry_duration, self.new_write_connection()).await {
                Ok(Ok(connection)) => return Ok(connection),
                Ok(Err(e)) => {
                    if attempt >= self.retry_max_attemps
                        || !e.to_string().contains("Could not set lock on file")
                    {
                        return Err(e);
                    }
                    info!("Retrying: {}", e);
                    attempt += 1;
                }
                Err(_) => {
                    return Err(duckdb::Error::DuckDBFailure(
                        duckdb::ffi::Error {
                            code: duckdb::ErrorCode::DatabaseLocked,
                            extended_code: 0,
                        },
                        None,
                    ));
                }
            }
        }
    }
}
