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
}
