//! Shared error types

use thiserror::Error;

/// Core errors shared between server and client
#[derive(Error, Debug)]
pub enum CoreError {
    #[error("validation failed: {0}")]
    Validation(String),

    #[error("invalid observation: {0}")]
    InvalidObservation(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
