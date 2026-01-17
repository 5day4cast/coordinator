use async_trait::async_trait;
use bdk_wallet::bitcoin::hashes::{sha256, Hash};
use log::{debug, info};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::lightning::{
    InvoiceAddResponse, InvoiceLookupResponse, InvoiceState, Ln, PaymentLookupResponse,
};
use crate::domain::PaymentStatus;

/// A mock invoice stored in the MockLnClient
#[derive(Debug, Clone)]
struct MockInvoice {
    payment_hash: String,
    payment_request: String,
    state: InvoiceState,
    value_sats: u64,
    memo: Option<String>,
    created_at: OffsetDateTime,
    preimage: Option<String>,
}

/// A mock Lightning Network client for E2E testing.
///
/// This client simulates LND behavior without requiring a real Lightning node.
/// Invoices can be auto-accepted after a configurable delay, or manually
/// controlled via the `accept_invoice` and `settle_invoice` methods.
#[derive(Clone)]
pub struct MockLnClient {
    invoices: Arc<RwLock<HashMap<String, MockInvoice>>>,
    payments: Arc<RwLock<HashMap<String, PaymentStatus>>>,
    auto_accept_delay: Option<Duration>,
    invoice_counter: Arc<RwLock<u64>>,
}

impl Default for MockLnClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockLnClient {
    /// Create a new MockLnClient with no auto-accept behavior.
    pub fn new() -> Self {
        Self {
            invoices: Arc::new(RwLock::new(HashMap::new())),
            payments: Arc::new(RwLock::new(HashMap::new())),
            auto_accept_delay: None,
            invoice_counter: Arc::new(RwLock::new(0)),
        }
    }

    /// Create a new MockLnClient that auto-accepts invoices after the given delay.
    pub fn with_auto_accept(delay: Duration) -> Self {
        Self {
            invoices: Arc::new(RwLock::new(HashMap::new())),
            payments: Arc::new(RwLock::new(HashMap::new())),
            auto_accept_delay: Some(delay),
            invoice_counter: Arc::new(RwLock::new(0)),
        }
    }

    /// Manually accept an invoice by its payment hash (hex-encoded).
    /// This simulates a user paying the invoice.
    pub fn accept_invoice(&self, payment_hash_hex: &str) -> Result<(), String> {
        let mut invoices = self.invoices.write().map_err(|e| e.to_string())?;
        if let Some(invoice) = invoices.get_mut(payment_hash_hex) {
            if invoice.state == InvoiceState::Open {
                invoice.state = InvoiceState::Accepted;
                info!("Mock: Invoice {} accepted", payment_hash_hex);
                Ok(())
            } else {
                Err(format!(
                    "Invoice {} is not in Open state (current: {:?})",
                    payment_hash_hex, invoice.state
                ))
            }
        } else {
            Err(format!("Invoice {} not found", payment_hash_hex))
        }
    }

    /// Manually settle an invoice by its payment hash (hex-encoded).
    pub fn settle_invoice_by_hash(&self, payment_hash_hex: &str) -> Result<(), String> {
        let mut invoices = self.invoices.write().map_err(|e| e.to_string())?;
        if let Some(invoice) = invoices.get_mut(payment_hash_hex) {
            if invoice.state == InvoiceState::Accepted || invoice.state == InvoiceState::Open {
                invoice.state = InvoiceState::Settled;
                info!("Mock: Invoice {} settled", payment_hash_hex);
                Ok(())
            } else {
                Err(format!(
                    "Invoice {} cannot be settled (current: {:?})",
                    payment_hash_hex, invoice.state
                ))
            }
        } else {
            Err(format!("Invoice {} not found", payment_hash_hex))
        }
    }

    /// Reset all mock state (invoices and payments).
    pub fn reset(&self) {
        if let Ok(mut invoices) = self.invoices.write() {
            invoices.clear();
        }
        if let Ok(mut payments) = self.payments.write() {
            payments.clear();
        }
        if let Ok(mut counter) = self.invoice_counter.write() {
            *counter = 0;
        }
        info!("Mock LN client state reset");
    }

    /// Get the current state of an invoice by payment hash.
    pub fn get_invoice_state(&self, payment_hash_hex: &str) -> Option<InvoiceState> {
        self.invoices
            .read()
            .ok()
            .and_then(|invoices| invoices.get(payment_hash_hex).map(|i| i.state.clone()))
    }

    /// Generate a deterministic mock BOLT11 invoice.
    fn generate_mock_invoice(&self, value_sats: u64, payment_hash_hex: &str) -> String {
        // Generate a fake but valid-looking BOLT11 invoice
        // In a real implementation, you'd use lightning-invoice crate
        // For mock purposes, we use a recognizable format
        format!(
            "lnbcrt{}n1mock{}",
            value_sats,
            &payment_hash_hex[..16] // Use first 16 chars of hash for uniqueness
        )
    }

    fn spawn_auto_accept(&self, payment_hash_hex: String, delay: Duration) {
        let invoices = self.invoices.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Ok(mut invoices) = invoices.write() {
                if let Some(invoice) = invoices.get_mut(&payment_hash_hex) {
                    if invoice.state == InvoiceState::Open {
                        invoice.state = InvoiceState::Accepted;
                        info!(
                            "Mock: Auto-accepted invoice {} after {:?}",
                            payment_hash_hex, delay
                        );
                    }
                }
            }
        });
    }
}

#[async_trait]
impl Ln for MockLnClient {
    async fn ping(&self) -> Result<(), anyhow::Error> {
        info!("Mock LN: ping successful");
        Ok(())
    }

    async fn add_hold_invoice(
        &self,
        value: u64,
        _expiry_time_secs: u64,
        ticket_hash_hex: String,
        competition_id: Uuid,
        hex_refund_tx: String,
    ) -> Result<InvoiceAddResponse, anyhow::Error> {
        debug!(
            "Mock LN: Creating hold invoice for {} sats, competition {}",
            value, competition_id
        );

        let payment_request = self.generate_mock_invoice(value, &ticket_hash_hex);

        let refund_tx_hash = sha256::Hash::hash(hex_refund_tx.as_bytes()).to_byte_array();
        let memo = format!("c:{};r:{:?}", competition_id, refund_tx_hash);

        let invoice = MockInvoice {
            payment_hash: ticket_hash_hex.clone(),
            payment_request: payment_request.clone(),
            state: InvoiceState::Open,
            value_sats: value,
            memo: Some(memo),
            created_at: OffsetDateTime::now_utc(),
            preimage: None,
        };

        {
            let mut invoices = self
                .invoices
                .write()
                .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
            invoices.insert(ticket_hash_hex.clone(), invoice);
        }

        // If auto-accept is enabled, schedule acceptance
        if let Some(delay) = self.auto_accept_delay {
            self.spawn_auto_accept(ticket_hash_hex, delay);
        }

        let mut counter = self
            .invoice_counter
            .write()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
        *counter += 1;
        let add_index = counter.to_string();

        Ok(InvoiceAddResponse {
            payment_request,
            add_index,
            payment_addr: String::new(),
        })
    }

    async fn add_invoice(
        &self,
        value: u64,
        _expiry_time_secs: u64,
        memo: String,
        competition_id: Uuid,
    ) -> Result<InvoiceAddResponse, anyhow::Error> {
        debug!(
            "Mock LN: Creating regular invoice for {} sats, competition {}",
            value, competition_id
        );

        // Generate a deterministic payment hash
        let hash_input = format!("{}:{}:{}", competition_id, value, memo);
        let payment_hash = sha256::Hash::hash(hash_input.as_bytes());
        let payment_hash_hex = hex::encode(payment_hash.to_byte_array());

        let payment_request = self.generate_mock_invoice(value, &payment_hash_hex);

        let invoice = MockInvoice {
            payment_hash: payment_hash_hex.clone(),
            payment_request: payment_request.clone(),
            state: InvoiceState::Open,
            value_sats: value,
            memo: Some(format!("{} - competition_id:{}", memo, competition_id)),
            created_at: OffsetDateTime::now_utc(),
            preimage: None,
        };

        {
            let mut invoices = self
                .invoices
                .write()
                .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
            invoices.insert(payment_hash_hex, invoice);
        }

        let mut counter = self
            .invoice_counter
            .write()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
        *counter += 1;
        let add_index = counter.to_string();

        Ok(InvoiceAddResponse {
            payment_request,
            add_index,
            payment_addr: String::new(),
        })
    }

    async fn create_invoice(
        &self,
        value: u64,
        _expiry_time_secs: u64,
    ) -> Result<String, anyhow::Error> {
        let hash_input = format!(
            "simple:{}:{}",
            value,
            OffsetDateTime::now_utc().unix_timestamp()
        );
        let payment_hash = sha256::Hash::hash(hash_input.as_bytes());
        let payment_hash_hex = hex::encode(payment_hash.to_byte_array());

        let payment_request = self.generate_mock_invoice(value, &payment_hash_hex);

        let invoice = MockInvoice {
            payment_hash: payment_hash_hex.clone(),
            payment_request: payment_request.clone(),
            state: InvoiceState::Open,
            value_sats: value,
            memo: None,
            created_at: OffsetDateTime::now_utc(),
            preimage: None,
        };

        {
            let mut invoices = self
                .invoices
                .write()
                .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
            invoices.insert(payment_hash_hex, invoice);
        }

        Ok(payment_request)
    }

    async fn cancel_hold_invoice(&self, ticket_hash_hex: String) -> Result<(), anyhow::Error> {
        let mut invoices = self
            .invoices
            .write()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;

        if let Some(invoice) = invoices.get_mut(&ticket_hash_hex) {
            invoice.state = InvoiceState::Canceled;
            info!("Mock LN: Canceled invoice {}", ticket_hash_hex);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Invoice {} not found", ticket_hash_hex))
        }
    }

    async fn settle_hold_invoice(&self, ticket_preimage: String) -> Result<(), anyhow::Error> {
        // Compute the payment hash from the preimage
        let preimage_bytes = hex::decode(&ticket_preimage)
            .map_err(|e| anyhow::anyhow!("Failed to decode preimage: {}", e))?;
        let payment_hash = sha256::Hash::hash(&preimage_bytes);
        let payment_hash_hex = hex::encode(payment_hash.to_byte_array());

        let mut invoices = self
            .invoices
            .write()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;

        if let Some(invoice) = invoices.get_mut(&payment_hash_hex) {
            if invoice.state == InvoiceState::Accepted {
                invoice.state = InvoiceState::Settled;
                invoice.preimage = Some(ticket_preimage);
                info!("Mock LN: Settled invoice {}", payment_hash_hex);
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "Invoice {} is not in Accepted state",
                    payment_hash_hex
                ))
            }
        } else {
            Err(anyhow::anyhow!("Invoice {} not found", payment_hash_hex))
        }
    }

    async fn lookup_invoice(&self, r_hash: &str) -> Result<InvoiceLookupResponse, anyhow::Error> {
        let invoices = self
            .invoices
            .read()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;

        if let Some(invoice) = invoices.get(r_hash) {
            Ok(InvoiceLookupResponse {
                state: invoice.state.clone(),
                memo: invoice.memo.clone(),
                r_hash: invoice.payment_hash.clone(),
                value: invoice.value_sats.to_string(),
                settled: invoice.state == InvoiceState::Settled,
                creation_date: invoice.created_at.unix_timestamp().to_string(),
                settle_date: if invoice.state == InvoiceState::Settled {
                    OffsetDateTime::now_utc().unix_timestamp().to_string()
                } else {
                    "0".to_string()
                },
                payment_request: invoice.payment_request.clone(),
                expiry: "3600".to_string(),
            })
        } else {
            Err(anyhow::anyhow!("Invoice {} not found", r_hash))
        }
    }

    async fn lookup_payment(&self, r_hash: &str) -> Result<PaymentLookupResponse, anyhow::Error> {
        let payments = self
            .payments
            .read()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;

        let status = payments
            .get(r_hash)
            .cloned()
            .unwrap_or(PaymentStatus::Unknown);

        Ok(PaymentLookupResponse {
            payment_hash: r_hash.to_string(),
            value: "0".to_string(),
            creation_date: OffsetDateTime::now_utc().unix_timestamp().to_string(),
            fee: "0".to_string(),
            payment_preimage: None,
            value_sat: "0".to_string(),
            value_msat: "0".to_string(),
            payment_request: String::new(),
            status,
            fee_sat: "0".to_string(),
            fee_msat: "0".to_string(),
            creation_time_ns: "0".to_string(),
            failure_reason: String::new(),
        })
    }

    async fn send_payment(
        &self,
        payout_payment_request: String,
        amount_sats: u64,
        _timeout_seconds: u64,
        _fee_limit_sat: u64,
    ) -> Result<(), anyhow::Error> {
        debug!(
            "Mock LN: Sending payment of {} sats to {}",
            amount_sats, payout_payment_request
        );

        // Extract or generate a payment hash for tracking
        let hash_input = format!("payment:{}:{}", payout_payment_request, amount_sats);
        let payment_hash = sha256::Hash::hash(hash_input.as_bytes());
        let payment_hash_hex = hex::encode(payment_hash.to_byte_array());

        {
            let mut payments = self
                .payments
                .write()
                .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
            payments.insert(payment_hash_hex, PaymentStatus::Succeeded);
        }

        info!("Mock LN: Payment sent successfully");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_ln_ping() {
        let client = MockLnClient::new();
        assert!(client.ping().await.is_ok());
    }

    #[tokio::test]
    async fn test_mock_ln_add_hold_invoice() {
        let client = MockLnClient::new();
        let competition_id = Uuid::now_v7();
        let ticket_hash = "a".repeat(64); // 32 bytes hex

        let response = client
            .add_hold_invoice(
                1000,
                3600,
                ticket_hash.clone(),
                competition_id,
                "refund".to_string(),
            )
            .await
            .unwrap();

        assert!(!response.payment_request.is_empty());

        // Invoice should be in Open state
        let state = client.get_invoice_state(&ticket_hash).unwrap();
        assert_eq!(state, InvoiceState::Open);
    }

    #[tokio::test]
    async fn test_mock_ln_accept_invoice() {
        let client = MockLnClient::new();
        let competition_id = Uuid::now_v7();
        let ticket_hash = "b".repeat(64);

        client
            .add_hold_invoice(
                1000,
                3600,
                ticket_hash.clone(),
                competition_id,
                "refund".to_string(),
            )
            .await
            .unwrap();

        // Accept the invoice
        client.accept_invoice(&ticket_hash).unwrap();

        let state = client.get_invoice_state(&ticket_hash).unwrap();
        assert_eq!(state, InvoiceState::Accepted);
    }

    #[tokio::test]
    async fn test_mock_ln_auto_accept() {
        let client = MockLnClient::with_auto_accept(Duration::from_millis(100));
        let competition_id = Uuid::now_v7();
        let ticket_hash = "c".repeat(64);

        client
            .add_hold_invoice(
                1000,
                3600,
                ticket_hash.clone(),
                competition_id,
                "refund".to_string(),
            )
            .await
            .unwrap();

        // Wait for auto-accept
        tokio::time::sleep(Duration::from_millis(200)).await;

        let state = client.get_invoice_state(&ticket_hash).unwrap();
        assert_eq!(state, InvoiceState::Accepted);
    }

    #[tokio::test]
    async fn test_mock_ln_lookup_invoice() {
        let client = MockLnClient::new();
        let competition_id = Uuid::now_v7();
        let ticket_hash = "d".repeat(64);

        client
            .add_hold_invoice(
                1000,
                3600,
                ticket_hash.clone(),
                competition_id,
                "refund".to_string(),
            )
            .await
            .unwrap();

        let lookup = client.lookup_invoice(&ticket_hash).await.unwrap();
        assert_eq!(lookup.value, "1000");
        assert_eq!(lookup.state, InvoiceState::Open);
    }

    #[tokio::test]
    async fn test_mock_ln_reset() {
        let client = MockLnClient::new();
        let competition_id = Uuid::now_v7();
        let ticket_hash = "e".repeat(64);

        client
            .add_hold_invoice(
                1000,
                3600,
                ticket_hash.clone(),
                competition_id,
                "refund".to_string(),
            )
            .await
            .unwrap();

        client.reset();

        assert!(client.get_invoice_state(&ticket_hash).is_none());
    }
}
