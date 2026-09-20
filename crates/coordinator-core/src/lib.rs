//! coordinator-core: Shared types for coordinator server and WASM client
//!
//! This crate contains types that are shared between the server and browser client.

pub mod types;

#[cfg(feature = "keymeld")]
pub mod keymeld;
#[cfg(feature = "keymeld")]
pub use keymeld_sdk::{derive_payout_preimage, PayoutPolicy};

pub use types::*;
