use super::CompetitionStatus;
use crate::domain::CompetitionStore;
use crate::infra::{bitcoin::Bitcoin, keymeld::Keymeld, lightning::Ln, oracle::Oracle};
use dlctix::secp::Scalar;
use std::sync::Arc;

pub struct StateContext {
    pub oracle: Arc<dyn Oracle>,
    pub bitcoin: Arc<dyn Bitcoin>,
    pub ln: Arc<dyn Ln>,
    pub keymeld: Arc<dyn Keymeld>,
    pub store: Arc<CompetitionStore>,
    pub coordinator_private_key: Scalar,
    pub required_confirmations: u32,
    pub relative_locktime_block_delta: u32,
}

impl StateContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        oracle: Arc<dyn Oracle>,
        bitcoin: Arc<dyn Bitcoin>,
        ln: Arc<dyn Ln>,
        keymeld: Arc<dyn Keymeld>,
        store: Arc<CompetitionStore>,
        coordinator_private_key: Scalar,
        required_confirmations: u32,
        relative_locktime_block_delta: u32,
    ) -> Self {
        Self {
            oracle,
            bitcoin,
            ln,
            keymeld,
            store,
            coordinator_private_key,
            required_confirmations,
            relative_locktime_block_delta,
        }
    }
}

pub type ProcessResult = Result<CompetitionStatus, StateProcessingError>;

#[derive(Debug, thiserror::Error)]
pub enum StateProcessingError {
    #[error("Bitcoin: {0}")]
    Bitcoin(#[from] anyhow::Error),

    #[error("Oracle: {0}")]
    Oracle(String),

    #[error("Database: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Keymeld: {0}")]
    Keymeld(String),

    #[error("Invalid state: {0}")]
    InvalidState(String),

    #[error("Missing data: {0}")]
    MissingData(String),

    #[error("Verification: {0}")]
    Verification(String),
}

impl From<crate::infra::oracle::Error> for StateProcessingError {
    fn from(err: crate::infra::oracle::Error) -> Self {
        StateProcessingError::Oracle(err.to_string())
    }
}

impl From<crate::infra::keymeld::KeymeldError> for StateProcessingError {
    fn from(err: crate::infra::keymeld::KeymeldError) -> Self {
        StateProcessingError::Keymeld(err.to_string())
    }
}
