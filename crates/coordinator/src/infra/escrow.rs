//! The per-ticket escrow: the coordinator locks the entry fee it fronts for
//! a ticket in an output that the funding transaction later spends.
//!
//! Spending paths, in the order miniscript tries them:
//! 1. coordinator + user, any time: the DLC funding transaction;
//! 2. user + ticket preimage after [`USER_REFUND_DELAY_BLOCKS`]: the user's
//!    refund if they paid but the competition never funded;
//! 3. coordinator alone after [`ESCROW_RECLAIM_DELAY_BLOCKS`]: recovers an
//!    escrow nobody spent, without which a broadcast escrow could stay locked
//!    for ever.
//!
//! The escrow transaction is built and broadcast only once the ticket's HODL
//! invoice is accepted, never handed out beforehand.

use crate::infra::bitcoin::{fee_rate_from_estimate, Bitcoin};
use anyhow::anyhow;
use bitcoin::{
    absolute::LockTime,
    psbt::raw::ProprietaryKey,
    transaction::{predict_weight, InputWeightPrediction, Version},
    Amount, FeeRate, OutPoint, Psbt, PublicKey, Sequence, Transaction, TxIn, TxOut, Witness,
};
use log::debug;
use miniscript::Descriptor;
use std::{collections::HashMap, str::FromStr, sync::Arc};
use uuid::Uuid;

/// Blocks after which a paying user can take the escrow with the preimage.
pub const USER_REFUND_DELAY_BLOCKS: u16 = 144;
/// Blocks after which the coordinator can reclaim an unspent escrow. Well
/// after the user's window, so a paying user always gets first call.
pub const ESCROW_RECLAIM_DELAY_BLOCKS: u16 = 1008;

pub async fn generate_escrow_tx(
    bitcoin: Arc<dyn Bitcoin>,
    ticket_id: Uuid,
    user_pubkey: PublicKey,
    payment_hash: [u8; 32],
    amount_sats: u64,
    lease_deadline: u64,
) -> Result<Transaction, anyhow::Error> {
    let fee_rates = bitcoin.get_estimated_fee_rates().await?;

    // Choose the fee rate for 2-block confirmation
    // TODO(@tee8z): Make this configurable
    let estimated_fee_rate = fee_rates.get(&1u16).cloned().unwrap_or(1.0);
    debug!("Estimated fee rate: {} sats/vB", estimated_fee_rate);

    let fee_rate = fee_rate_from_estimate(estimated_fee_rate)?;
    debug!(
        "Transaction fee rate: {} sats/vB",
        fee_rate.to_sat_per_vb_ceil()
    );

    let coordinator_pubkey = bitcoin.get_public_key().await?;

    let escrow_descriptor =
        create_escrow_descriptor(&coordinator_pubkey, &user_pubkey, &payment_hash)?;

    let escrow_address = escrow_descriptor.address(bitcoin.get_network())?;
    debug!("Created escrow address: {}", escrow_address);

    //TODO(@tee8z): create smart UTXOs pool to use for escrow, for now we let the wallet decide
    let mut psbt = bitcoin
        .build_psbt(
            escrow_address.script_pubkey(),
            Amount::from_sat(amount_sats),
            fee_rate,
            vec![],
            vec![],
        )
        .await?;

    let proprietary_key = ProprietaryKey {
        prefix: b"competition".to_vec(),
        subtype: 0u8,
        key: b"ticket_id".to_vec(),
    };
    let proprietary_value = ticket_id.as_bytes().to_vec();
    psbt.proprietary.insert(proprietary_key, proprietary_value);

    let transaction = async {
        bitcoin
            .reserve_psbt_inputs_until(&psbt, lease_deadline)
            .await?;
        if !bitcoin.sign_psbt(&mut psbt).await? {
            return Err(anyhow!("LND did not finalize the escrow transaction"));
        }
        psbt.clone().extract_tx().map_err(anyhow::Error::from)
    }
    .await;
    let final_tx = match transaction {
        Ok(transaction) => transaction,
        Err(error) => {
            if let Err(release_error) = bitcoin.release_psbt_inputs(&psbt).await {
                log::warn!(
                    "Failed to release inputs after escrow preparation failed: {}",
                    release_error
                );
            }
            return Err(error);
        }
    };

    debug!(
        "Generated escrow transaction with ID: {} for ticket: {}",
        final_tx.compute_wtxid(),
        ticket_id
    );

    Ok(final_tx)
}

pub fn create_escrow_descriptor(
    coordinator_pubkey: &PublicKey,
    user_pubkey: &PublicKey,
    payment_hash: &[u8; 32],
) -> Result<Descriptor<PublicKey>, anyhow::Error> {
    let payment_hash_hex = hex::encode(payment_hash);

    let descriptor_str = format!(
        "wsh(or_d(multi(2,{coordinator_pubkey},{user_pubkey}),or_i(and_v(v:pk({user_pubkey}),and_v(v:sha256({payment_hash_hex}),older({USER_REFUND_DELAY_BLOCKS}))),and_v(v:pk({coordinator_pubkey}),older({ESCROW_RECLAIM_DELAY_BLOCKS})))))"
    );

    Descriptor::from_str(&descriptor_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse descriptor: {}", e))
}

/// Sweep an escrow output back to the wallet through the reclaim branch.
/// Valid once the escrow output is [`ESCROW_RECLAIM_DELAY_BLOCKS`] deep.
pub async fn reclaim_escrow_tx(
    bitcoin: Arc<dyn Bitcoin>,
    escrow_tx: &Transaction,
    user_pubkey: PublicKey,
    payment_hash: [u8; 32],
    fee_rate: FeeRate,
) -> Result<Transaction, anyhow::Error> {
    let coordinator_pubkey = bitcoin.get_public_key().await?;
    let descriptor = create_escrow_descriptor(&coordinator_pubkey, &user_pubkey, &payment_hash)?;
    let script_pubkey = descriptor.script_pubkey();
    let (vout, escrow_output) = escrow_tx
        .output
        .iter()
        .enumerate()
        .find(|(_, output)| output.script_pubkey == script_pubkey)
        .ok_or_else(|| anyhow!("Escrow output not found in {}", escrow_tx.compute_txid()))?;
    let witness_script = descriptor.explicit_script()?;
    let destination = bitcoin.get_next_address().await?.script_pubkey();
    let sequence = Sequence::from_height(ESCROW_RECLAIM_DELAY_BLOCKS);

    // Witness: coordinator signature, branch selector, three empties for
    // the unsatisfied multisig, the script.
    let weight = predict_weight(
        [InputWeightPrediction::new(
            0,
            [73usize, 1, 1, 1, 1, witness_script.len()],
        )],
        [destination.len()],
    );
    let fee = weight * fee_rate;
    let value = escrow_output
        .value
        .checked_sub(fee)
        .filter(|value| *value >= destination.minimal_non_dust())
        .ok_or_else(|| anyhow!("Escrow output does not cover the reclaim fee"))?;

    let tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: escrow_tx.compute_txid(),
                vout: vout as u32,
            },
            sequence,
            ..Default::default()
        }],
        output: vec![TxOut {
            value,
            script_pubkey: destination,
        }],
    };
    let mut psbt = Psbt::from_unsigned_tx(tx)?;
    psbt.inputs[0].witness_utxo = Some(escrow_output.clone());
    psbt.inputs[0].witness_script = Some(witness_script);
    bitcoin.sign_psbt_with_escrow_support(&mut psbt).await?;

    let signatures: HashMap<PublicKey, bitcoin::ecdsa::Signature> = psbt.inputs[0]
        .partial_sigs
        .iter()
        .map(|(k, v)| (*k, *v))
        .collect();
    let (witness, _) = descriptor
        .get_satisfaction((signatures, sequence))
        .map_err(|e| anyhow!("Escrow reclaim is not satisfiable: {e}"))?;
    psbt.inputs[0].final_script_witness = Some(Witness::from_slice(&witness));
    psbt.extract_tx().map_err(anyhow::Error::from)
}

pub fn get_escrow_outpoint(
    transaction: &Transaction,
    escrow_amount: Amount,
) -> Result<OutPoint, anyhow::Error> {
    let txid = transaction.compute_txid();

    // Find the output with the matching amount that's also a P2WSH script
    for (index, output) in transaction.output.iter().enumerate() {
        if output.value == escrow_amount && output.script_pubkey.is_p2wsh() {
            debug!("Escrow output found: output {:?} index {}", output, index);
            return Ok(OutPoint {
                txid,
                vout: index as u32,
            });
        }
    }

    Err(anyhow!("Escrow output not found for transaction {}", txid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        ecdsa,
        hashes::Hash as _,
        secp256k1::{Message, Secp256k1, SecretKey},
        sighash::{EcdsaSighashType, SighashCache},
        Network, PublicKey,
    };
    use std::str::FromStr;

    fn key(secp: &Secp256k1<bitcoin::secp256k1::All>, byte: u8) -> (SecretKey, PublicKey) {
        let secret = SecretKey::from_slice(&[byte; 32]).unwrap();
        (secret, PublicKey::new(secret.public_key(secp)))
    }

    /// Signature of `spend`'s only input over the escrow script.
    fn sign(
        secp: &Secp256k1<bitcoin::secp256k1::All>,
        spend: &Transaction,
        script: &bitcoin::ScriptBuf,
        value: Amount,
        secret: &SecretKey,
    ) -> ecdsa::Signature {
        let sighash = SighashCache::new(spend)
            .p2wsh_signature_hash(0, script, value, EcdsaSighashType::All)
            .unwrap();
        ecdsa::Signature {
            signature: secp.sign_ecdsa(&Message::from_digest(sighash.to_byte_array()), secret),
            sighash_type: EcdsaSighashType::All,
        }
    }

    #[test]
    fn reclaim_needs_the_delay_and_only_the_coordinator_key() {
        let secp = Secp256k1::new();
        let (coordinator_secret, coordinator) = key(&secp, 1);
        let (user_secret, user) = key(&secp, 2);
        let descriptor = create_escrow_descriptor(&coordinator, &user, &[3; 32]).unwrap();
        let script = descriptor.explicit_script().unwrap();
        let value = Amount::from_sat(50_000);
        let spend = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                sequence: Sequence::from_height(ESCROW_RECLAIM_DELAY_BLOCKS),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(49_000),
                script_pubkey: descriptor.script_pubkey(),
            }],
        };
        let coordinator_only: HashMap<PublicKey, ecdsa::Signature> = HashMap::from([(
            coordinator,
            sign(&secp, &spend, &script, value, &coordinator_secret),
        )]);

        // Reclaim: coordinator signature once the delay has passed.
        let (witness, _) = descriptor
            .get_satisfaction((
                coordinator_only.clone(),
                Sequence::from_height(ESCROW_RECLAIM_DELAY_BLOCKS),
            ))
            .expect("reclaim path");
        assert_eq!(witness.last().unwrap(), script.as_bytes());
        assert!(descriptor
            .get_satisfaction((
                coordinator_only.clone(),
                Sequence::from_height(ESCROW_RECLAIM_DELAY_BLOCKS - 1),
            ))
            .is_err());
        assert!(descriptor
            .get_satisfaction((coordinator_only, Sequence::ZERO))
            .is_err());

        // The funding path still needs both keys and no delay.
        let both: HashMap<PublicKey, ecdsa::Signature> = HashMap::from([
            (
                coordinator,
                sign(&secp, &spend, &script, value, &coordinator_secret),
            ),
            (user, sign(&secp, &spend, &script, value, &user_secret)),
        ]);
        let (witness, _) = descriptor
            .get_satisfaction((both, Sequence::ZERO))
            .expect("funding path");
        assert_eq!(witness.len(), 4, "empty, two signatures, script");
    }

    #[test]
    fn test_create_escrow_descriptor_valid_miniscript() {
        let coordinator_pubkey = PublicKey::from_str(
            "02e58afe51f9ed8ad3cc7897f634d881fdbe49a81564629ded8156bebd2ffd1af3",
        )
        .unwrap();

        let user_pubkey = PublicKey::from_str(
            "039b6347398505f5ec93826dc61c19f47c66c0283ee9be980e29ce325a0f4679ef",
        )
        .unwrap();

        let payment_hash = [0u8; 32];

        let result = create_escrow_descriptor(&coordinator_pubkey, &user_pubkey, &payment_hash);

        assert!(
            result.is_ok(),
            "Failed to create descriptor: {:?}",
            result.err()
        );

        let descriptor = result.unwrap();

        let address = descriptor.address(Network::Bitcoin);
        assert!(
            address.is_ok(),
            "Failed to derive address: {:?}",
            address.err()
        );

        let addr = address.unwrap();
        assert!(addr.script_pubkey().is_p2wsh(), "Expected P2WSH address");

        println!("Created descriptor: {}", descriptor);
        println!("Derived address: {}", addr);
    }
}
