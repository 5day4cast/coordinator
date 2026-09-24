//! Browser wallet for DLC entries.
//!
//! Trust boundary: the coordinator and everything it sends (contract
//! parameters, PSBTs, aggregate nonces, enclave keys) are untrusted. The seed
//! and every entry key stay inside WASM. The wallet encrypts only to the
//! user's own Nostr key. The one intentional plaintext export is an entry's
//! key and payout preimage, sold to the coordinator for an off-chain payout,
//! and only after re-deriving the entry's recorded pubkey.
//!
//! Design and invariants: `docs/BROWSER_WALLET.md`.

mod core;
mod keymeld_trust;
mod keys;
mod qr;

#[cfg(target_arch = "wasm32")]
mod wasm;

use serde::Serialize;
use thiserror::Error;

pub use core::DlcWalletCore;
pub use keymeld_trust::{enclave_trust, EnclaveTrust};

#[cfg(target_arch = "wasm32")]
pub use wasm::DlcWallet;

#[derive(Error, Debug)]
pub enum WalletError {
    #[error("Unsupported network: {0}")]
    Network(String),
    #[error("Invalid wallet backup")]
    InvalidBackup,
    #[error("Entry key derivation failed")]
    DerivationFailed,
    #[error("Invalid entry id: {0}")]
    InvalidEntryId(String),
    #[error("Entry {0} was not created by this wallet")]
    ForeignEntry(uuid::Uuid),
    #[error("Nostr signer not initialized")]
    NostrNotInitialized,
    #[error("Nostr signer error: {0}")]
    Signer(String),
    #[error("Keymeld error: {0}")]
    Keymeld(String),
    #[error("Invalid invoice: {0}")]
    Invoice(String),
    #[error("Invalid contract: {0}")]
    Contract(String),
    #[error("No contract loaded for entry {0}")]
    NoContract(uuid::Uuid),
    #[error("Entry {0} already signed different aggregate nonces")]
    ConflictingAggregateNonces(uuid::Uuid),
    #[error("DLC signing failed: {0}")]
    Signing(String),
    #[error("Refusing to sign funding PSBT: {0}")]
    FundingPsbtRejected(String),
    #[error("No outcome matches the attestation")]
    NoMatchingOutcome,
}

/// Public values an entry submission needs.
#[derive(Serialize)]
pub struct EntryRegistration {
    pub ephemeral_pubkey: String,
    pub payout_hash: String,
}

#[derive(Serialize)]
pub struct EncryptedWalletBackup {
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
}

/// Secrets sold to the coordinator in exchange for an off-chain payout.
/// The fields are plaintext, so this type is neither `Debug` nor `Clone`.
#[derive(Serialize)]
pub struct PayoutRelease {
    pub ephemeral_private_key: String,
    pub payout_preimage: String,
}

impl Drop for PayoutRelease {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.ephemeral_private_key.zeroize();
        self.payout_preimage.zeroize();
    }
}

#[cfg(target_arch = "wasm32")]
impl From<WalletError> for wasm_bindgen::JsValue {
    fn from(error: WalletError) -> Self {
        wasm_bindgen::JsValue::from_str(&error.to_string())
    }
}
