//! A payment principal is bound to a signed invoice and an operator-controlled budget.
use anyhow::{Context, Result};
use lightning_invoice::{Bolt11Invoice, Currency};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedInvoice {
    invoice: String,
    payment_hash: String,
    amount_sats: u64,
    expires_at: u64,
}

impl ValidatedInvoice {
    pub fn validate(
        invoice: &str,
        expected_hash: Option<&str>,
        amount_sats: u64,
        max_sats: u64,
        network: Currency,
    ) -> Result<Self> {
        anyhow::ensure!(
            amount_sats > 0 && amount_sats <= max_sats,
            "invoice exceeds payment budget"
        );
        let decoded: Bolt11Invoice = invoice.parse().context("decode signed BOLT11 invoice")?;
        decoded
            .check_signature()
            .context("verify invoice signature")?;
        anyhow::ensure!(
            decoded.currency() == network,
            "invoice network does not match the paying node"
        );
        let amount_msats = amount_sats
            .checked_mul(1000)
            .context("invoice amount overflow")?;
        anyhow::ensure!(
            decoded.amount_milli_satoshis() == Some(amount_msats),
            "invoice principal differs from the authorized amount"
        );
        let payment_hash = decoded.payment_hash().to_string();
        if let Some(expected) = expected_hash {
            anyhow::ensure!(
                hex::decode(expected).ok() == hex::decode(&payment_hash).ok(),
                "invoice hash differs from the ticket hash"
            );
        }
        let intent = Self {
            invoice: decoded.to_string(),
            payment_hash,
            amount_sats,
            expires_at: decoded
                .expires_at()
                .context("invoice expiry overflow")?
                .as_secs(),
        };
        intent.ensure_fresh()?;
        Ok(intent)
    }

    pub fn ensure_fresh(&self) -> Result<()> {
        anyhow::ensure!(!self.expired(), "invoice has expired");
        Ok(())
    }

    pub fn expired(&self) -> bool {
        i128::from(time::OffsetDateTime::now_utc().unix_timestamp()) >= i128::from(self.expires_at)
    }

    pub fn invoice(&self) -> &str {
        &self.invoice
    }
    pub fn payment_hash(&self) -> &str {
        &self.payment_hash
    }
    pub fn amount_sats(&self) -> u64 {
        self.amount_sats
    }

    pub fn verify_paid(&self, paid: &crate::lnd::Paid) -> Result<()> {
        anyhow::ensure!(
            paid.payment_hash == self.payment_hash && paid.value_sat == self.amount_sats,
            "node reported a different payment hash or principal"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dlctix::bitcoin::hashes::{sha256, Hash};
    use dlctix::bitcoin::secp256k1::{Secp256k1, SecretKey};
    use lightning_invoice::{InvoiceBuilder, PaymentSecret};
    use std::time::Duration;

    fn invoice(amount_msats: Option<u64>, created: u64, network: Currency) -> String {
        let secp = Secp256k1::new();
        let key = SecretKey::from_slice(&[11; 32]).unwrap();
        let builder = InvoiceBuilder::new(network)
            .description("bounded synthetic payment".into())
            .payment_hash(sha256::Hash::from_byte_array([3; 32]))
            .payment_secret(PaymentSecret([7; 32]))
            .duration_since_epoch(Duration::from_secs(created))
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(600));
        let builder = match amount_msats {
            Some(amount) => builder.amount_milli_satoshis(amount),
            None => builder,
        };
        builder
            .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &key))
            .unwrap()
            .to_string()
    }

    #[test]
    fn invoice_principal_hash_network_expiry_and_budget_are_bound() {
        let created = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
        let encoded = invoice(Some(5_300_000), created, Currency::Regtest);
        let hash = "03".repeat(32);
        let validate = |encoded: &str, hash: &str, sats, cap, network| {
            ValidatedInvoice::validate(encoded, Some(hash), sats, cap, network)
        };
        let good = validate(&encoded, &hash, 5_300, 6_000, Currency::Regtest).unwrap();
        assert_eq!(good.invoice(), encoded);
        assert_eq!(good.payment_hash(), hash);
        assert!(validate(&encoded, &hash, 1_100, 6_000, Currency::Regtest).is_err());
        assert!(validate(&encoded, &"04".repeat(32), 5_300, 6_000, Currency::Regtest).is_err());
        assert!(validate(&encoded, &hash, 5_300, 5_299, Currency::Regtest).is_err());
        assert!(validate(&encoded, &hash, 5_300, 6_000, Currency::Bitcoin).is_err());
        let expired = invoice(Some(5_300_000), created - 601, Currency::Regtest);
        assert!(validate(&expired, &hash, 5_300, 6_000, Currency::Regtest).is_err());
        let amountless = invoice(None, created, Currency::Regtest);
        assert!(validate(&amountless, &hash, 5_300, 6_000, Currency::Regtest).is_err());
        let fractional = invoice(Some(5_300_001), created, Currency::Regtest);
        assert!(validate(&fractional, &hash, 5_300, 6_000, Currency::Regtest).is_err());
        assert!(validate(&encoded, &hash, u64::MAX, u64::MAX, Currency::Regtest).is_err());
        assert!(validate("invalid", &hash, 5_300, 6_000, Currency::Regtest).is_err());
    }
}
