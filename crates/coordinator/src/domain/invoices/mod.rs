mod invoice_subscriber;
mod invoice_watcher;
mod payment_subscriber;
mod payout_watcher;

pub use invoice_subscriber::InvoiceSubscriber;
pub use invoice_watcher::InvoiceWatcher;
pub use payment_subscriber::PaymentSubscriber;
pub use payout_watcher::PayoutWatcher;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentLookupResponse {
    pub status: PaymentStatus,
    pub payment_error: Option<String>,
    pub payment_preimage: Option<String>,
    pub payment_hash: String,
    pub value_sat: String,
    pub fee_sat: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentStatus {
    Unknown,
    InFlight,
    Succeeded,
    Failed,
    Initiated,
}
