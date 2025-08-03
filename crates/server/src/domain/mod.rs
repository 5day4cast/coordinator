mod competitions;
mod invoices;
mod users;

pub use competitions::*;
pub use invoices::*;
use thiserror::Error;
use time::OffsetDateTime;
pub use users::*;

use crate::OracleError;

#[derive(Error, Debug)]
pub enum Error {
    #[error("item not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("problem querying db: {0}")]
    DbError(#[from] sqlx::Error),
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
