//! Player fund recovery without the coordinator.
//!
//! Given only a player's nsec, or the downloadable recovery file and the nsec, this crate finds
//! every entry the coordinator recorded for them and says where its money is and how to move it.
//! It needs no coordinator, coordinator database or Keymeld: the records come from Nostr relays
//! or the file (`docs/RECOVERY.md`), the keys from the wallet seed, and the transactions from the
//! contract every player signed.
//!
//! - [`Identity`]: the player's Nostr key, which decrypts the records.
//! - [`Session`]: collects records, contracts, attestations and chain state, then reports on
//!   each entry ([`Session::inspect`]) and builds the transactions that claim it
//!   ([`Session::claim`]). It does no I/O: the caller fetches what it asks for, so the CLI and
//!   the browser page share it.
//! - [`spec`]: the published record formats.
//! - [`fees`]: which transactions can be fee bumped, and the hook for anchor CPFP.
//!
//! The `native` feature adds the CLI's I/O: relays, Esplora, the oracle and Arkade.

pub mod attestation;
pub mod chain;
pub mod contract;
pub mod escrow;
pub mod fees;
mod identity;
pub mod inspect;
mod keys;
mod session;
pub mod spec;

#[cfg(feature = "native")]
pub mod native;

pub use identity::Identity;
pub use keys::{EntryKey, WalletSeed};
pub use session::{network as parse_network, ClaimPlan, ClaimTx, Session};

use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid nsec")]
    Nsec,
    #[error("Invalid {0}")]
    Invalid(String),
    #[error("Cannot decrypt the {0}")]
    Decrypt(&'static str),
    #[error("Invalid record: {0}")]
    Record(String),
    #[error("No wallet backup was found for this nsec")]
    NoWallet,
    #[error("No coordinator recovery pubkey: pass --coordinator-pubkey or the recovery file")]
    NoCoordinator,
    #[error(
        "Entry {0} was not created by this wallet: its recorded key differs from the one the seed derives"
    )]
    ForeignEntry(Uuid),
    #[error("Contract for entry {entry}: {reason}")]
    Contract { entry: Uuid, reason: String },
    #[error("Unknown entry {0}")]
    UnknownEntry(Uuid),
    #[error("{0}")]
    NotYet(String),
}

pub type Result<T> = std::result::Result<T, Error>;
