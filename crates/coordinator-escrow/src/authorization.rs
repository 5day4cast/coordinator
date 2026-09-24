//! Coordinator-owned participant consent, encoded inside the generic escrow policy.
pub use keymeld_core::authorization::{
    authorization_digest, sign_authorization, verify_authorization,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutPolicy {
    pub automatic_lightning_address: Option<String>,
    pub allow_invoice_fallback: bool,
    /// Application consent supplements the independent generic key-release permission.
    pub release_entry_key_after_payment: bool,
    pub contract_terms: String,
    /// The buy-in is held in an Arkade escrow VTXO, and the pool is funded in an Arkade batch.
    /// Absent for tickets funded any other way, so their signed policies are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ark_escrow: Option<ArkEscrowPolicy>,
}

/// Consent to spend the entry's Arkade escrow into its pool's contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArkEscrowPolicy {
    /// The escrow's PSBT `TapTree` field, hex. It fixes every term: the player, coordinator, and
    /// server keys, the refund locktime, and the exit delays.
    pub escrow_tap_tree: String,
    /// The most the coordinator may take from each escrow as its fee in the funding batch.
    pub max_fee_sats: u64,
    /// The most the swap service may keep from this escrow for paying the player's Lightning
    /// Address, when a competition that never kicked off refunds it.
    pub max_refund_fee_sats: u64,
    /// The Arkade server's checkpoint exit script, hex. Every offchain spend passes through a
    /// checkpoint output made of the leaf being spent and this script, so a refund's destination
    /// can only be checked against a script the player consented to.
    pub checkpoint_exit_script: String,
}
