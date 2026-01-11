use crate::infra::bitcoin::Bitcoin;
use anyhow::anyhow;
use bdk_wallet::{
    bitcoin::{psbt::raw::ProprietaryKey, Amount, OutPoint, PublicKey, Transaction},
    miniscript::Descriptor,
    SignOptions,
};
use dlctix::bitcoin::FeeRate;
use log::debug;
use std::{str::FromStr, sync::Arc};
use uuid::Uuid;

pub async fn generate_escrow_tx(
    bitcoin: Arc<dyn Bitcoin>,
    ticket_id: Uuid,
    user_pubkey: PublicKey,
    payment_hash: [u8; 32],
    amount_sats: u64,
) -> Result<Transaction, anyhow::Error> {
    let fee_rates = bitcoin.get_estimated_fee_rates().await?;

    // Choose the fee rate for 2-block confirmation
    // TODO(@tee8z): Make this configurable
    let esplora_fee_rate = fee_rates.get(&1u16).cloned().unwrap_or(1.0);
    debug!("Esplora fee rate: {} sats/vB", esplora_fee_rate);

    let fee_rate_sat_vb = esplora_fee_rate.ceil() as u64;
    debug!("Transaction fee rate: {} sats/vB", fee_rate_sat_vb);

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
            FeeRate::from_sat_per_vb_unchecked(fee_rate_sat_vb),
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

    bitcoin.sign_psbt(&mut psbt, SignOptions::default()).await?;

    let final_tx = psbt.extract_tx()?;

    debug!(
        "Generated escrow transaction with ID: {} for ticket: {}",
        final_tx.compute_wtxid(),
        ticket_id
    );

    Ok(final_tx)
}

pub fn create_escrow_descriptor(
    coordinator_pubkey: &bdk_wallet::bitcoin::PublicKey,
    user_pubkey: &PublicKey,
    payment_hash: &[u8; 32],
) -> Result<Descriptor<PublicKey>, anyhow::Error> {
    let payment_hash_hex = hex::encode(payment_hash);

    // Spend via miniscript's language:
    // 1. Payment hash preimage + user's key after 1 day (144 blocks)
    // 2. Both coordinator and user signatures anytime
    // We may add another spending condition later that allows the coordinator to reclaim the utxo after a certain period of time.
    // Leaving out for now to avoid that complexity
    let descriptor_str = format!(
        "wsh(or_d(multi(2,{},{}),and_v(v:pk({}),and_v(v:sha256({}),older(144)))))",
        coordinator_pubkey, user_pubkey, user_pubkey, payment_hash_hex
    );

    Descriptor::from_str(&descriptor_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse descriptor: {}", e))
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
    use bdk_wallet::bitcoin::{Network, PublicKey};
    use std::str::FromStr;

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
