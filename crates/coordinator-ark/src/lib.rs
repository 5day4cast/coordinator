//! Arkade batches for queued competitions, over arkd's gRPC API.
//!
//! Each entry's buy-in waits in an escrow VTXO (see `coordinator-ark-escrow`).
//! At kickoff, [`fund_pool`] spends a pool's escrows into its on-chain DLC funding output in one Arkade batch.
//! The Arkade server supplies the on-chain liquidity, so the coordinator fronts nothing.
//!
//! - [`ArkServer`]: connect, read `/v1/info`, and issue escrows the server will accept.
//! - [`EscrowSigner`]: Keymeld signs as each player, and the coordinator signs with its own key.
//! - [`KickoffHooks::before_forfeits`]: runs once the commitment transaction is known, before anything is forfeited.
//! - [`DlcKickoff`]: signs the pool's ticketed DLC in that hook, against the batch's funding output.
//!
//! See `docs/QUEUED_COMPETITIONS.md`.

mod dlc;
mod error;
mod kickoff;
mod server;
mod signer;
#[cfg(feature = "test-utils")]
pub mod testing;
mod transport;

pub use dlc::{ContractSigner, DlcKickoff, LocalContractSigner};
pub use error::{BoxError, Error};
pub use kickoff::{fund_pool, EscrowInput, Kickoff, KickoffConfig, KickoffHooks, PoolFunding};
pub use server::{address_hrp, escrow_terms, server_rules, ArkServer};
pub use signer::{
    script_spend_sighash, EscrowSigner, KeypairSigner, SigningPurpose, SigningRequest,
};
pub use transport::{ArkTransport, EventStream};
