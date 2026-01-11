//! coordinator-wasm: WASM client for coordinator
//!
//! This crate provides browser-side functionality for:
//! - Nostr authentication (NIP-98)
//! - Escrow PSBT signing
//! - Keymeld SDK integration for remote MuSig2 signing

use wasm_bindgen::prelude::*;

pub mod keymeld;
pub mod nostr;
pub mod wallet;

// Re-export coordinator-core types
pub use coordinator_core::*;

// Re-export nostr types for internal use
pub use nostr::NostrClientCore;

// Re-export keymeld types for WASM bindings
pub use keymeld::{KeymeldClientConfig, KeymeldParticipant};

#[wasm_bindgen(start)]
pub fn start() {
    wasm_logger::init(wasm_logger::Config::default());
    log::info!("coordinator-wasm initialized");
}
