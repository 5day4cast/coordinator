//! coordinator-wasm: WASM client for coordinator
//!
//! This crate provides browser-side functionality for:
//! - Nostr authentication (NIP-98)
//! - Escrow PSBT signing
//! - Keymeld SDK integration (coming in Part 5)

use wasm_bindgen::prelude::*;

pub mod nostr;
pub mod wallet;

// Re-export coordinator-core types
pub use coordinator_core::*;

// Re-export nostr types for internal use
pub use nostr::NostrClientCore;

#[wasm_bindgen(start)]
pub fn start() {
    wasm_logger::init(wasm_logger::Config::default());
    log::info!("coordinator-wasm initialized");
}
