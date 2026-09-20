//! Application capabilities returned confidentially by the registered verifier.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutCapabilities {
    pub payout: bool,
    pub lnurl: bool,
}
