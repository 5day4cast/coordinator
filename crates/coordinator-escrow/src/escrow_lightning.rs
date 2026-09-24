//! Optional BOLT11 evidence validation, independent of DLCs and application economics.
//! An invoice signature authenticates its payee key, not a Lightning Address.
//! The caller must bind the invoice to a participant-authorized destination.

use keymeld_core::escrow::sha256;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef, Currency};
use serde::{Deserialize, Serialize};
use std::{str::FromStr, time::Duration};
use thiserror::Error;

pub const MAX_INVOICE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightningNetwork {
    Bitcoin,
    Testnet,
    Signet,
    Regtest,
}

impl LightningNetwork {
    fn currency(self) -> Currency {
        match self {
            Self::Bitcoin => Currency::Bitcoin,
            Self::Testnet => Currency::BitcoinTestnet,
            Self::Signet => Currency::Signet,
            Self::Regtest => Currency::Regtest,
        }
    }
}

#[derive(Debug, Error)]
pub enum LightningEvidenceError {
    #[error("Invalid invoice: {0}")]
    InvalidInvoice(String),
    #[error("Payment preimage does not match the committed invoice")]
    PreimageMismatch,
}

fn parse(invoice: &str) -> Result<Bolt11Invoice, LightningEvidenceError> {
    if invoice.len() > MAX_INVOICE_BYTES {
        return Err(LightningEvidenceError::InvalidInvoice(
            "Invoice exceeds size limit".into(),
        ));
    }
    Bolt11Invoice::from_str(invoice)
        .map_err(|e| LightningEvidenceError::InvalidInvoice(e.to_string()))
}

/// Validate signature, exact positive amount, network and expiry before a new payment.
pub fn validate_invoice(
    invoice: &str,
    amount_msat: u64,
    network: LightningNetwork,
    now_secs: u64,
) -> Result<Bolt11Invoice, LightningEvidenceError> {
    let invoice = validate_prepared_invoice(invoice, amount_msat, network)?;
    if invoice.would_expire(Duration::from_secs(now_secs)) {
        return Err(LightningEvidenceError::InvalidInvoice(
            "Invoice expired before preparation".into(),
        ));
    }
    Ok(invoice)
}

/// Recovery retains structural validation after expiry. This does not authorize
/// initiating a new payment; the caller must reconcile any previous attempt.
pub fn validate_prepared_invoice(
    invoice: &str,
    amount_msat: u64,
    network: LightningNetwork,
) -> Result<Bolt11Invoice, LightningEvidenceError> {
    let invoice = parse(invoice)?;
    if amount_msat == 0 || invoice.amount_milli_satoshis() != Some(amount_msat) {
        return Err(LightningEvidenceError::InvalidInvoice(
            "Invoice amount differs from authorization".into(),
        ));
    }
    if invoice.currency() != network.currency() {
        return Err(LightningEvidenceError::InvalidInvoice(
            "Invoice network differs from authorization".into(),
        ));
    }
    Ok(invoice)
}

/// Check an invoice was issued for a Lightning Address, by its LUD-06 metadata binding.
///
/// A provider serves metadata for each address it hosts, and commits its SHA-256 in the `h` tag
/// of every invoice it issues for that address. An invoice carrying a different commitment was
/// issued for someone else, or by someone else.
///
/// This lets a caller supply the invoice, which an enclave needs when the payment hash must be
/// known before the transaction paying it exists. It is weaker than issuing the request
/// directly: it inherits whatever the provider puts in its metadata.
pub fn validate_address_invoice(
    invoice: &Bolt11Invoice,
    metadata_hash: [u8; 32],
) -> Result<(), LightningEvidenceError> {
    match invoice.description() {
        Bolt11InvoiceDescriptionRef::Hash(hash) if hash.0.as_ref() as &[u8] == metadata_hash => {
            Ok(())
        }
        Bolt11InvoiceDescriptionRef::Hash(_) => Err(LightningEvidenceError::InvalidInvoice(
            "Invoice was issued for another Lightning Address".into(),
        )),
        Bolt11InvoiceDescriptionRef::Direct(_) => Err(LightningEvidenceError::InvalidInvoice(
            "Invoice does not commit to its Lightning Address metadata".into(),
        )),
    }
}

/// Prove knowledge of the invoice preimage. Its recipient controls this secret;
/// authenticity of the recipient must be established independently.
pub fn verify_payment_preimage(
    invoice: &str,
    preimage: &[u8; 32],
) -> Result<(), LightningEvidenceError> {
    let invoice = parse(invoice)?;
    let expected: &[u8] = invoice.payment_hash().as_ref();
    if expected != sha256(preimage).as_slice() {
        return Err(LightningEvidenceError::PreimageMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Published BOLT11 vector also used by lightning-invoice's parser tests.
    const INVOICE: &str = "lnbc100p1psj9jhxdqud3jxktt5w46x7unfv9kz6mn0v3jsnp4q0d3p2sfluzdx45tqcs\
			h2pu5qc7lgq0xs578ngs6s0s68ua4h7cvspp5q6rmq35js88zp5dvwrv9m459tnk2zunwj5jalqtyxqulh0l\
			5gflssp5nf55ny5gcrfl30xuhzj3nphgj27rstekmr9fw3ny5989s300gyus9qyysgqcqpcrzjqw2sxwe993\
			h5pcm4dxzpvttgza8zhkqxpgffcrf5v25nwpr3cmfg7z54kuqq8rgqqqqqqqq2qqqqq9qq9qrzjqd0ylaqcl\
			j9424x9m8h2vcukcgnm6s56xfgu3j78zyqzhgs4hlpzvznlugqq9vsqqqqqqqlgqqqqqeqq9qrzjqwldmj9d\
			ha74df76zhx6l9we0vjdquygcdt3kssupehe64g6yyp5yz5rhuqqwccqqyqqqqlgqqqqjcqq9qrzjqf9e58a\
			guqr0rcun0ajlvmzq3ek63cw2w282gv3z5uupmuwvgjtq2z55qsqqg6qqqyqqqrtnqqqzq3cqygrzjqvphms\
			ywntrrhqjcraumvc4y6r8v4z5v593trte429v4hredj7ms5z52usqq9ngqqqqqqqlgqqqqqqgq9qrzjq2v0v\
			p62g49p7569ev48cmulecsxe59lvaw3wlxm7r982zxa9zzj7z5l0cqqxusqqyqqqqlgqqqqqzsqygarl9fh3\
			8s0gyuxjjgux34w75dnc6xp2l35j7es3jd4ugt3lu0xzre26yg5m7ke54n2d5sym4xcmxtl8238xxvw5h5h5\
			j5r6drg6k6zcqj0fcwg";

    #[test]
    fn standalone_lightning_verifier_enforces_amount_network_and_expiry() {
        let invoice = parse(INVOICE).unwrap();
        let amount = invoice.amount_milli_satoshis().unwrap();
        let now = invoice.duration_since_epoch().as_secs();
        assert!(validate_invoice(INVOICE, amount, LightningNetwork::Bitcoin, now).is_ok());
        assert!(validate_invoice(INVOICE, amount + 1, LightningNetwork::Bitcoin, now).is_err());
        assert!(validate_invoice(INVOICE, 0, LightningNetwork::Bitcoin, now).is_err());
        assert!(validate_invoice(INVOICE, amount, LightningNetwork::Regtest, now).is_err());
        let expired = now + invoice.expiry_time().as_secs() + 1;
        assert!(validate_invoice(INVOICE, amount, LightningNetwork::Bitcoin, expired).is_err());
        assert!(validate_prepared_invoice(INVOICE, amount, LightningNetwork::Bitcoin).is_ok());
        assert!(verify_payment_preimage(INVOICE, &[0; 32]).is_err());
        let mut changed = INVOICE.to_string();
        changed.pop();
        changed.push('q');
        assert!(validate_prepared_invoice(&changed, amount, LightningNetwork::Bitcoin).is_err());
        assert!(parse(&"x".repeat(MAX_INVOICE_BYTES + 1)).is_err());
    }
}
