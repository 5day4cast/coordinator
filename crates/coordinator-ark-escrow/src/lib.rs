//! Arkade escrow VTXOs for competition entries.
//!
//! A player's buy-in is swapped from Lightning into an escrow VTXO.
//! At kickoff, an Arkade batch spends the escrow into the pool's on-chain DLC funding output.
//! If the competition never kicks off, the player takes the escrow back after the refund locktime.
//! See `docs/QUEUED_COMPETITIONS.md`.
//!
//! The scripts, taproot trees, and addresses match arkd and `@arkade-os/sdk` byte for byte.
//! Fixtures from that SDK's test suite check this.
//! This crate depends only on `bitcoin`, so the coordinator, the browser wallet, and the Keymeld verifier can all use it.

mod address;
mod error;
mod escrow;
mod tapscript;
mod vtxo_script;

pub use address::{ArkAddress, ADDRESS_VERSION, MAINNET_HRP, TESTNET_HRP};
pub use error::Error;
pub use escrow::{EntryEscrow, EscrowPath, EscrowTerms};
pub use tapscript::{cltv_condition, RelativeTimelock, Tapscript};
pub use vtxo_script::{assemble_btcd_tree, VtxoScript, UNSPENDABLE_INTERNAL_KEY};
