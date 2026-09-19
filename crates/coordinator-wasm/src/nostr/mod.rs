mod core;
mod login;
mod types;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use core::NostrClientCore;
pub use login::{LoginError, LoginKeys};
pub use types::{CustomSigner, SignerType};

#[cfg(target_arch = "wasm32")]
pub use wasm::{LoginCredentials, NostrClientWrapper};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum NostrError {
    #[error("No signer initialized")]
    NoSigner,
    #[error("The extension signer does not expose a local key")]
    NoLocalKey,
    #[error("Invalid {0}")]
    InvalidRequest(&'static str),
    #[error(transparent)]
    Login(#[from] LoginError),
    #[error("Key parsing error: {0}")]
    KeyParsing(#[from] nostr_sdk::key::Error),
    #[error("Key encoding error: {0}")]
    KeyEncoding(#[from] nostr_sdk::nips::nip19::Error),
    #[error("Signer error: {0}")]
    Signer(#[from] nostr_sdk::signer::SignerError),
    #[error("Event builder error: {0}")]
    EventBuilder(#[from] nostr_sdk::event::builder::Error),
    #[cfg(target_arch = "wasm32")]
    #[error("Browser signer error: {0}")]
    BrowserSigner(#[from] nostr_sdk::nips::nip07::Error),
}

#[cfg(target_arch = "wasm32")]
impl From<NostrError> for wasm_bindgen::JsValue {
    fn from(error: NostrError) -> Self {
        wasm_bindgen::JsValue::from_str(&error.to_string())
    }
}
