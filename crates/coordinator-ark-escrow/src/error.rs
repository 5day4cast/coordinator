use thiserror::Error;

/// Errors from encoding, decoding, or assembling Arkade scripts.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("script is empty")]
    EmptyScript,
    #[error("script is too short: expected at least {expected} instructions")]
    TooShort { expected: usize },
    #[error("invalid script: {0}")]
    InvalidScript(String),
    #[error("script is not in canonical form")]
    NonCanonical,
    #[error("at least one public key is required")]
    NoPubkeys,
    #[error("invalid relative timelock: {0}")]
    InvalidTimelock(String),
    #[error("at least one leaf script is required")]
    NoLeaves,
    #[error("invalid taproot tree: {0}")]
    Taproot(String),
    #[error("invalid TapTree encoding: {0}")]
    TapTree(String),
    #[error("invalid Ark address: {0}")]
    Address(String),
    #[error("invalid escrow: {0}")]
    Escrow(String),
}
