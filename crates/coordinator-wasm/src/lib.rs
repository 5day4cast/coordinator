//! coordinator-wasm: WASM client for coordinator
//!
//! This crate provides browser-side functionality for:
//! - Nostr authentication (NIP-98)
//! - Escrow PSBT signing
//! - Keymeld SDK integration for remote MuSig2 signing (requires `keymeld` feature)

use wasm_bindgen::prelude::*;

#[cfg(feature = "keymeld")]
pub mod keymeld;
pub mod nostr;
pub mod wallet;

// Re-export coordinator-core types
pub use coordinator_core::*;

// Re-export nostr types for internal use
pub use nostr::NostrClientCore;

// Re-export keymeld crypto functions for WASM bindings (when feature enabled)
// These are used for server-side keymeld registration - the coordinator
// handles all keymeld communication, users just provide encrypted keys
#[cfg(feature = "keymeld")]
pub use keymeld::{derive_keymeld_auth_pubkey, encrypt_private_key_for_enclave};

#[wasm_bindgen(start)]
pub fn start() {
    wasm_logger::init(wasm_logger::Config::default());
    log::info!("coordinator-wasm initialized");
}
