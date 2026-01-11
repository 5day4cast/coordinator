mod core;
mod types;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use core::NostrClientCore;
pub use types::{CustomSigner, SignerType};

use thiserror::Error;

#[cfg(target_arch = "wasm32")]
pub use wasm::{Nip04Methods, Nip44Methods, NostrClientWrapper};

#[cfg(not(target_arch = "wasm32"))]
pub type NostrClientWrapper = NostrClientCore;

// Re-export common traits and types that are used in public interfaces
pub use nostr_sdk::{
    nips::nip04,
    signer::{NostrSigner, SignerBackend, SignerError},
    Relay, RelayUrl,
};

#[cfg(target_arch = "wasm32")]
pub use wasm_bindgen;

#[derive(Error, Debug)]
pub enum NostrError {
    #[error("No signer initialized")]
    NoSigner(String),
    #[error("Key parsing error: {0}")]
    KeyParsing(#[from] nostr_sdk::key::Error),
    #[error("Relay connection error: {0}")]
    RelayConnection(#[from] nostr_sdk::client::Error),
    #[error("Signer error: {0}")]
    SignerError(#[from] nostr_sdk::signer::SignerError),
    #[error("Event builder error: {0}")]
    EventBuilderError(#[from] nostr_sdk::event::builder::Error),
    #[error("Browser signer error: {0}")]
    #[cfg(target_arch = "wasm32")]
    BrowserSigner(#[from] nostr_sdk::nips::nip07::Error),
}

#[cfg(target_arch = "wasm32")]
impl From<NostrError> for wasm_bindgen::JsValue {
    fn from(error: NostrError) -> Self {
        wasm_bindgen::JsValue::from_str(&error.to_string())
    }
}
