mod competitions;
mod invoices;
pub mod leaderboard;
pub mod users;

pub use competitions::*;
pub use invoices::*;
use thiserror::Error;
use time::OffsetDateTime;
pub use users::*;

use crate::infra::{db::DatabaseWriteError, oracle::Error as OracleError};

#[derive(Error, Debug)]
pub enum Error {
    #[error("item not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("problem querying db: {0}")]
    DbError(#[from] sqlx::Error),
    #[error("database write failed: {0}")]
    DatabaseWrite(#[from] DatabaseWriteError),
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
    #[error("Competition full, total_allowed_entries matches total_entries")]
    CompetitionFull,
    #[error("No ticket available for competition")]
    NoAvailableTickets,
    #[error("Too late to sign with ticket. Signing must end by {0}, but current time is {1}")]
    TooLateToSign(OffsetDateTime, OffsetDateTime),
    #[error("Payout payment failed: {0}")]
    PaymentFailed(String),
    /// Retryable: a ticket is never priced without a network fee estimate.
    #[error("{}", competitions::FEE_ESTIMATE_UNAVAILABLE)]
    FeeEstimateUnavailable,
    /// Retryable later: no ticket is issued while the network fee would be too large a share of
    /// the entry.
    #[error("{}", competitions::ENTRIES_PAUSED)]
    EntriesPaused,
    /// Retryable: the swap service cannot fund an entry's swap right now.
    #[error("{}", competitions::SWAPS_UNAVAILABLE)]
    SwapsUnavailable,
}

impl Error {
    /// A refusal the player can act on, such as a full competition or a used ticket, rather
    /// than a fault. Routes log these as warnings, so errors mean something is broken.
    pub fn is_refusal(&self) -> bool {
        matches!(
            self,
            Error::NoAvailableTickets
                | Error::CompetitionFull
                | Error::BadRequest(_)
                | Error::NotFound(_)
                | Error::InvalidSignature(_)
                | Error::FeeEstimateUnavailable
                | Error::EntriesPaused
                | Error::SwapsUnavailable
        )
    }

    /// The error with its causes, for logs. Players see `Bitcoin` errors only as "internal
    /// error", which says nothing to an operator.
    pub fn detail(&self) -> String {
        match self {
            Error::Bitcoin(error) => format!("{error:#}"),
            other => other.to_string(),
        }
    }
}
