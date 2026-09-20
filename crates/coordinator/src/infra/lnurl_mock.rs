//! LNURL-pay stand-in for mocked Lightning: every well-formed address
//! resolves, and ordinary invoices are signed locally so the claim flow runs
//! without a network. Metadata and invoice descriptions are display data.

use super::lnurl::{LightningAddress, LnurlError, LnurlPay, PayRequest};
use async_trait::async_trait;
use bitcoin::{
    hashes::{sha256, Hash},
    secp256k1::{Secp256k1, SecretKey},
    Network,
};
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};
use rand::RngCore;

pub struct MockLnurlPay {
    network: Network,
}

impl MockLnurlPay {
    pub fn new(network: Network) -> Self {
        Self { network }
    }
}

fn currency(network: Network) -> Currency {
    match network {
        Network::Bitcoin => Currency::Bitcoin,
        Network::Testnet => Currency::BitcoinTestnet,
        Network::Signet => Currency::Signet,
        _ => Currency::Regtest,
    }
}

#[async_trait]
impl LnurlPay for MockLnurlPay {
    async fn resolve(&self, address: &LightningAddress) -> Result<PayRequest, LnurlError> {
        let metadata = serde_json::json!([
            ["text/plain", format!("Pay {address}")],
            ["text/identifier", address.to_string()],
        ])
        .to_string();
        PayRequest::new(
            address.clone(),
            "https://mock-wallet.dev/lnurlp/callback",
            1_000,
            100_000_000_000,
            metadata,
        )
    }

    async fn request_invoice(
        &self,
        request: &PayRequest,
        amount_msat: u64,
    ) -> Result<Bolt11Invoice, LnurlError> {
        request.check_amount(amount_msat)?;
        let secp = Secp256k1::new();
        let node_key = SecretKey::from_slice(&[0x11; 32]).expect("constant key is valid");
        let mut preimage = [0u8; 32];
        rand::rng().fill_bytes(&mut preimage);
        let invoice = InvoiceBuilder::new(currency(self.network))
            .description(format!("Payout to {}", request.address()))
            .payment_hash(sha256::Hash::hash(&preimage))
            .payment_secret(PaymentSecret(preimage))
            .amount_milli_satoshis(amount_msat)
            .current_timestamp()
            .min_final_cltv_expiry_delta(18)
            .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &node_key))
            .map_err(|e| LnurlError::Invoice(e.to_string()))?;
        request.verify_invoice(&invoice.to_string(), amount_msat, self.network)
    }
}
