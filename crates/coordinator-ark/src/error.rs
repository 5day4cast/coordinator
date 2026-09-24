use bitcoin::{OutPoint, XOnlyPublicKey};
use thiserror::Error;

/// An error from a signer or hook that the caller supplies.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Arkade server request failed: {0}")]
    Server(#[from] ark_grpc::Error),
    #[error("Arkade transaction error: {0}")]
    Ark(#[from] ark_core::Error),
    #[error(transparent)]
    Escrow(#[from] coordinator_ark_escrow::Error),
    #[error("the server's parameters are unusable: {0}")]
    ServerInfo(String),
    #[error("the pool cannot be funded: {0}")]
    InvalidPool(String),
    #[error("signing failed: {0}")]
    Signer(BoxError),
    #[error("a signer returned a bad signature for escrow {escrow} with key {key}")]
    BadSignature {
        escrow: OutPoint,
        key: XOnlyPublicKey,
    },
    #[error("the batch would not fund the pool, so nothing was forfeited: {0}")]
    Unfunded(String),
    #[error("the before-forfeits hook failed, so nothing was forfeited: {0}")]
    Hook(BoxError),
    #[error("batch {id} failed: {reason}")]
    BatchFailed { id: String, reason: String },
    #[error("unexpected batch event: {0}")]
    Protocol(String),
    #[error("timed out {0}")]
    Timeout(&'static str),
}
