//! Checks for the payout sellback.
//!
//! A winner sells their payout preimage (the preimage of the dlctix player's
//! `payout_hash`) and their entry key back to the market maker for a
//! Lightning payment. These functions hold the checks that make that exchange
//! safe for the coordinator; they are pure so they can be tested without a
//! node.

use dlctix::{
    hashlock,
    secp::{Point, Scalar},
    ContractParameters, Outcome,
};
use lightning_invoice::Bolt11Invoice;
use std::str::FromStr;

/// Upper bound on a submitted BOLT11 invoice. Route hints make real invoices
/// long, but nothing legitimate comes close to this.
pub const MAX_INVOICE_LEN: usize = 4096;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PayoutRejection {
    #[error("Lightning invoice is longer than {MAX_INVOICE_LEN} characters")]
    InvoiceTooLong,
    #[error("Invalid lightning invoice: {0}")]
    InvalidInvoice(String),
    #[error("Entry is not a winner")]
    NotAWinner,
    #[error("Invalid payout preimage")]
    InvalidPreimage,
    #[error("Payout preimage does not match this entry's payout hash")]
    PreimageMismatch,
    #[error("Invalid entry key")]
    InvalidKey,
    #[error("Entry key does not match this entry")]
    KeyMismatch,
}

/// What the market maker owes a winner, in sats: their weight's share of the
/// funding value. Weights sum to 100 (see `get_percentage_weights`).
pub fn winner_payout_sats(
    params: &ContractParameters,
    outcome: &Outcome,
    entry_pubkey: &Point,
) -> Result<u64, PayoutRejection> {
    let weights = params
        .outcome_payouts
        .get(outcome)
        .ok_or(PayoutRejection::NotAWinner)?;
    let weight = weights
        .iter()
        .find_map(|(index, weight)| {
            (params.players.get(*index)?.pubkey == *entry_pubkey).then_some(*weight)
        })
        .filter(|weight| *weight > 0)
        .ok_or(PayoutRejection::NotAWinner)?;
    Ok(params.funding_value.to_sat().saturating_mul(weight) / 100)
}

pub fn parse_invoice(invoice: &str) -> Result<Bolt11Invoice, PayoutRejection> {
    if invoice.len() > MAX_INVOICE_LEN {
        return Err(PayoutRejection::InvoiceTooLong);
    }
    Bolt11Invoice::from_str(invoice.trim())
        .map_err(|e| PayoutRejection::InvalidInvoice(e.to_string()))
}

/// A payout preimage, accepted only if it opens the entry's payout hash.
/// Hex or base64, since LND reports preimages both ways.
pub fn verify_payout_preimage(
    preimage: &str,
    payout_hash_hex: &str,
) -> Result<[u8; 32], PayoutRejection> {
    let preimage_hex = normalize_hex32(preimage).ok_or(PayoutRejection::InvalidPreimage)?;
    let preimage =
        hashlock::preimage_from_hex(&preimage_hex).map_err(|_| PayoutRejection::InvalidPreimage)?;
    let mut expected = [0u8; 32];
    hex::decode_to_slice(payout_hash_hex, &mut expected)
        .map_err(|_| PayoutRejection::PreimageMismatch)?;
    if hashlock::sha256(&preimage) != expected {
        return Err(PayoutRejection::PreimageMismatch);
    }
    Ok(preimage)
}

/// 32 bytes given as 64 hex characters or as base64, as lowercase hex.
fn normalize_hex32(value: &str) -> Option<String> {
    use base64::Engine;
    let value = value.trim();
    if value.len() == 64 && hex::decode(value).is_ok() {
        return Some(value.to_ascii_lowercase());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(value))
        .ok()?;
    (bytes.len() == 32).then(|| hex::encode(bytes))
}

/// Check that an entry key opens the entry's recorded pubkey.
pub fn verify_entry_key(key_hex: &str, entry_pubkey: &Point) -> Result<(), PayoutRejection> {
    let key = Scalar::from_hex(key_hex).map_err(|_| PayoutRejection::InvalidKey)?;
    if key.base_point_mul() != *entry_pubkey {
        return Err(PayoutRejection::KeyMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        hashes::{sha256, Hash},
        secp256k1::{Secp256k1, SecretKey},
        Amount,
    };
    use dlctix::{attestation_locking_point, MarketMaker, PayoutWeights, Player};
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    use std::collections::BTreeMap;

    /// A signed regtest invoice for `payment_hash` and `amount_msat`.
    fn invoice(payment_hash: [u8; 32], amount_msat: Option<u64>) -> String {
        let secp = Secp256k1::new();
        let node_key = SecretKey::from_slice(&[0x11; 32]).unwrap();
        let builder = InvoiceBuilder::new(Currency::Regtest)
            .description("payout".into())
            .payment_hash(sha256::Hash::from_byte_array(payment_hash))
            .payment_secret(PaymentSecret([7; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144);
        let signed = match amount_msat {
            Some(amount) => builder
                .amount_milli_satoshis(amount)
                .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &node_key)),
            None => builder.build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &node_key)),
        };
        signed.unwrap().to_string()
    }

    struct Entry {
        preimage: [u8; 32],
        payout_hash: String,
    }

    fn entry() -> Entry {
        let preimage = hashlock::preimage_random(&mut rand::rng());
        Entry {
            payout_hash: hex::encode(hashlock::sha256(&preimage)),
            preimage,
        }
    }

    #[test]
    fn parses_invoices_and_rejects_malformed_or_oversized_ones() {
        let e = entry();
        let hash = hashlock::sha256(&e.preimage);
        let parsed = parse_invoice(&invoice(hash, Some(45_000_000))).unwrap();
        assert_eq!(parsed.amount_milli_satoshis(), Some(45_000_000));
        assert!(parse_invoice(&invoice(hash, None))
            .unwrap()
            .amount_milli_satoshis()
            .is_none());
        assert!(matches!(
            parse_invoice("lnbcrt1notaninvoice"),
            Err(PayoutRejection::InvalidInvoice(_))
        ));
        assert_eq!(
            parse_invoice(&"l".repeat(MAX_INVOICE_LEN + 1)).map(|_| ()),
            Err(PayoutRejection::InvoiceTooLong)
        );
    }

    #[test]
    fn payout_preimage_is_accepted_only_if_it_opens_the_payout_hash() {
        use base64::Engine;
        let e = entry();
        assert_eq!(
            verify_payout_preimage(&hex::encode(e.preimage), &e.payout_hash),
            Ok(e.preimage)
        );
        let as_base64 = base64::engine::general_purpose::STANDARD.encode(e.preimage);
        assert_eq!(
            verify_payout_preimage(&as_base64, &e.payout_hash),
            Ok(e.preimage)
        );
        let wrong = hashlock::preimage_random(&mut rand::rng());
        assert_eq!(
            verify_payout_preimage(&hex::encode(wrong), &e.payout_hash),
            Err(PayoutRejection::PreimageMismatch)
        );
        for malformed in ["", "zz", "00"] {
            assert_eq!(
                verify_payout_preimage(malformed, &e.payout_hash),
                Err(PayoutRejection::InvalidPreimage)
            );
        }
    }

    #[test]
    fn entry_key_must_open_the_entry_pubkey() {
        let key = Scalar::random(&mut rand::rng());
        let pubkey = key.base_point_mul();
        let other = Scalar::random(&mut rand::rng());
        assert!(verify_entry_key(&hex::encode(key.serialize()), &pubkey).is_ok());
        assert_eq!(
            verify_entry_key(&hex::encode(other.serialize()), &pubkey),
            Err(PayoutRejection::KeyMismatch)
        );
        assert_eq!(
            verify_entry_key("nope", &pubkey),
            Err(PayoutRejection::InvalidKey)
        );
    }

    #[test]
    fn payout_is_the_winners_weighted_share_and_losers_get_nothing() {
        let mut rng = rand::rng();
        let player = |rng: &mut rand::rngs::ThreadRng| Player {
            pubkey: Scalar::random(rng).base_point_mul(),
            ticket_hash: hashlock::sha256(&hashlock::preimage_random(rng)),
            payout_hash: hashlock::sha256(&hashlock::preimage_random(rng)),
        };
        let players = vec![player(&mut rng), player(&mut rng), player(&mut rng)];
        let oracle = Scalar::random(&mut rng).base_point_mul();
        let nonce = Scalar::random(&mut rng).base_point_mul();
        let params = ContractParameters {
            market_maker: MarketMaker {
                pubkey: Scalar::random(&mut rng).base_point_mul(),
            },
            players: players.clone(),
            event: dlctix::EventLockingConditions {
                locking_points: vec![attestation_locking_point(oracle, nonce, b"a")],
                expiry: None,
            },
            outcome_payouts: BTreeMap::from([(
                Outcome::Attestation(0),
                PayoutWeights::from([(0, 60), (1, 40)]),
            )]),
            fee_rate: dlctix::bitcoin::FeeRate::from_sat_per_vb_u32(1),
            funding_value: Amount::from_sat(100_000),
            relative_locktime_block_delta: 72,
        };
        let outcome = Outcome::Attestation(0);

        assert_eq!(
            winner_payout_sats(&params, &outcome, &players[0].pubkey),
            Ok(60_000)
        );
        assert_eq!(
            winner_payout_sats(&params, &outcome, &players[1].pubkey),
            Ok(40_000)
        );
        assert_eq!(
            winner_payout_sats(&params, &outcome, &players[2].pubkey),
            Err(PayoutRejection::NotAWinner)
        );
        assert_eq!(
            winner_payout_sats(&params, &Outcome::Expiry, &players[0].pubkey),
            Err(PayoutRejection::NotAWinner)
        );
    }
}
