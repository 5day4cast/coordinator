mod core;

#[cfg(target_arch = "wasm32")]
mod wasm;

use dlctix::{bitcoin::OutPoint, musig2::PubNonce, ContractParameters, SigMap, TicketedDLC};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use core::{TaprootWalletCore, TaprootWalletCoreBuilder};

#[cfg(target_arch = "wasm32")]
pub use wasm::{TaprootWallet, TaprootWalletBuilder};

#[derive(Error, Debug)]
pub enum WalletError {
    #[error("Network error: {0}")]
    NetworkError(String),
    #[error("Key error: {0}")]
    KeyError(String),
    #[error("Contract error: {0}")]
    ContractError(String),
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("DLC error: {0}")]
    DlcError(String),
    #[error("DLC key error: {0}")]
    DlcKeyError(String),
    #[error("No contract found for entry index: {0}")]
    NoContract(u32),
    #[error("Failed to calculate checksum: {0}")]
    CheckSum(String),
    #[error("Failed to load wallet: {0}")]
    LoadWallet(String),
    #[error("Signer error: {0}")]
    SignerError(String),
    #[error("Nostr signer not initialized")]
    NostrNotInitialize,
    #[error("Failed to get public key: {0}")]
    PublicKeyError(String),
    #[error("Failed decryption: {0}")]
    DecryptionError(String),
    #[error("Failed encryption: {0}")]
    EncryptionError(String),
    #[error("Invalid xpriv: {0}")]
    InvalidXpriv(String),
    #[error("Invalid bech32 nostr public key: {0}")]
    InvalidPublicKey(String),
    #[error("Dlc entry with index {0} not found")]
    DlcEntryNotFound(u32),
}

#[cfg(target_arch = "wasm32")]
impl From<WalletError> for wasm_bindgen::JsValue {
    fn from(error: WalletError) -> Self {
        wasm_bindgen::JsValue::from_str(&error.to_string())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum NetworkKind {
    Main,
    Test,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct KeyPair {
    pub descriptor: String,
    pub change_descriptor: String,
    pub network: NetworkKind,
}

#[derive(Serialize, Deserialize)]
pub struct EncryptedKey {
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
}

pub struct DlcEntry {
    pub contract: Option<TicketedDLC>,
    pub data: DlcEntryData,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SigningState {
    pub seed: [u8; 32],
    pub public_nonces: SigMap<PubNonce>,
}

#[derive(Clone)]
pub struct DlcEntryData {
    pub payout_preimage: SecretString,
    pub funding_outpoint: Option<OutPoint>,
    pub params: Option<ContractParameters>,
    pub ticket_preimage: Option<SecretString>,
}
