//! An entry's Arkade escrow, rebuilt from its record and checked against the derived entry key.
//!
//! The escrow's leaves (`coordinator-ark-escrow`) give the player two ways out without the
//! coordinator: the refund leaf with the Arkade server from `T` on (cooperative, see
//! [`crate::native::ark`]), and after an unroll the unilateral refund leaf alone, a relative
//! delay after the escrow output confirms on chain ([`unilateral_refund_tx`]).

use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::Message;
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::taproot::LeafVersion;
use bitcoin::transaction::{predict_weight, InputWeightPrediction, Version};
use bitcoin::{
    Amount, FeeRate, OutPoint, ScriptBuf, TapLeafHash, TapSighashType, Transaction, TxIn, TxOut,
    Witness, XOnlyPublicKey,
};
use coordinator_ark_escrow::{EntryEscrow, EscrowPath, RelativeTimelock, VtxoScript};

use crate::spec::{EntryRecord, EscrowRecord};
use crate::{EntryKey, Error, Result};

/// An entry's escrow, with the terms its record states.
pub struct Escrow {
    pub script: EntryEscrow,
    pub outpoint: OutPoint,
    pub amount: Amount,
}

impl Escrow {
    /// Rebuild the escrow from `entry`'s record. Refuses unless its player key is `key`. `None`
    /// if the entry has no escrow, or one that was never funded.
    pub fn new(entry: &EntryRecord, key: &EntryKey) -> Result<Option<Self>> {
        let Some(record) = &entry.escrow else {
            return Ok(None);
        };
        let (Some(outpoint), Some(amount)) = (&record.outpoint, record.amount_sat) else {
            return Ok(None);
        };
        if !key.matches(&entry.entry_pubkey) {
            return Err(Error::ForeignEntry(entry.entry_id));
        }
        let script = escrow_script(record)?;
        let terms = script.terms();
        if terms.player != key.xonly() {
            return Err(Error::ForeignEntry(entry.entry_id));
        }
        let invalid =
            |what: &str| Error::Record(format!("entry {}: escrow {what}", entry.entry_id));
        if record.server_pubkey.parse::<XOnlyPublicKey>().ok() != Some(terms.server)
            && !record.server_pubkey.is_empty()
        {
            return Err(invalid("server key differs from its leaves"));
        }
        if u64::from(terms.refund_locktime.to_consensus_u32()) != record.refund_locktime {
            return Err(invalid("refund locktime differs from its leaves"));
        }
        Ok(Some(Self {
            script,
            outpoint: outpoint.parse().map_err(|_| invalid("outpoint"))?,
            amount: Amount::from_sat(amount),
        }))
    }

    /// `T`, from which the refund leaf opens, in unix seconds.
    pub fn refund_at(&self) -> u64 {
        u64::from(self.script.terms().refund_locktime.to_consensus_u32())
    }

    /// The unilateral refund leaf's delay after the unrolled escrow confirms, in seconds or
    /// blocks.
    pub fn unilateral_refund_delay(&self) -> RelativeTimelock {
        self.script.terms().unilateral_refund_delay
    }
}

/// The escrow script from the record's leaves: hex leaf scripts in order or, as the coordinator
/// stores it, one PSBT `TapTree` field.
fn escrow_script(record: &EscrowRecord) -> Result<EntryEscrow> {
    let invalid = |e: String| Error::Record(format!("escrow leaves: {e}"));
    let leaves = record
        .tap_tree
        .iter()
        .map(|leaf| hex::decode(leaf).map(ScriptBuf::from_bytes))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| invalid(e.to_string()))?;
    let as_leaves =
        VtxoScript::new(leaves.clone()).and_then(|vtxo| EntryEscrow::from_vtxo_script(&vtxo));
    match (as_leaves, leaves.as_slice()) {
        (Ok(escrow), _) => Ok(escrow),
        (Err(_), [tap_tree]) => VtxoScript::decode_tap_tree(tap_tree.as_bytes())
            .and_then(|vtxo| EntryEscrow::from_vtxo_script(&vtxo))
            .map_err(|e| invalid(e.to_string())),
        (Err(e), _) => Err(invalid(e.to_string())),
    }
}

/// Spend an unrolled escrow output through the unilateral refund leaf, with the entry key alone,
/// to `destination`. Valid once the output has waited the leaf's delay on chain.
pub fn unilateral_refund_tx(
    escrow: &Escrow,
    key: &EntryKey,
    destination: ScriptBuf,
    fee_rate: FeeRate,
) -> Result<Transaction> {
    let script = &escrow.script;
    if script.terms().player != key.xonly() {
        return Err(Error::Invalid("entry key for this escrow".into()));
    }
    let leaf = script.script(EscrowPath::UnilateralRefund).clone();
    let control_block = script.control_block(EscrowPath::UnilateralRefund);
    let sequence = script
        .terms()
        .unilateral_refund_delay
        .to_sequence()
        .map_err(|e| Error::Record(format!("escrow delay: {e}")))?;
    let weight = predict_weight(
        [InputWeightPrediction::new(
            0,
            [64, leaf.len(), control_block.size()],
        )],
        [destination.len()],
    );
    let fee = fee_rate
        .checked_mul_by_weight(weight)
        .ok_or_else(|| Error::Invalid("fee rate".into()))?;
    let value = escrow
        .amount
        .checked_sub(fee)
        .filter(|value| *value >= destination.minimal_non_dust())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "fee rate: a {fee} fee leaves nothing of the {} escrow",
                escrow.amount
            ))
        })?;
    let prevout = TxOut {
        value: escrow.amount,
        script_pubkey: script.script_pubkey(),
    };
    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: escrow.outpoint,
            sequence,
            ..TxIn::default()
        }],
        output: vec![TxOut {
            value,
            script_pubkey: destination,
        }],
    };
    let leaf_hash = TapLeafHash::from_script(&leaf, LeafVersion::TapScript);
    let sighash = SighashCache::new(&tx)
        .taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(&[prevout]),
            leaf_hash,
            TapSighashType::Default,
        )
        .map_err(|e| Error::Invalid(format!("sighash: {e}")))?;
    let signature = Secp256k1::new().sign_schnorr_no_aux_rand(
        &Message::from_digest(sighash.to_byte_array()),
        &key.keypair(),
    );
    let mut witness = Witness::new();
    witness.push(signature.serialize());
    witness.push(leaf.as_bytes());
    witness.push(control_block.serialize());
    tx.input[0].witness = witness;
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::WalletSeed;
    use crate::spec::EscrowRecord;
    use bitcoin::key::Keypair;
    use bitcoin::secp256k1::schnorr;
    use bitcoin::Network;
    use coordinator_ark_escrow::EscrowTerms;
    use uuid::Uuid;

    fn other_key(byte: u8) -> XOnlyPublicKey {
        Keypair::from_seckey_slice(&Secp256k1::new(), &[byte; 32])
            .unwrap()
            .x_only_public_key()
            .0
    }

    fn record(
        key: &EntryKey,
        entry_id: Uuid,
        player: XOnlyPublicKey,
    ) -> (EntryRecord, EntryEscrow) {
        let terms = EscrowTerms {
            player,
            coordinator: other_key(2),
            server: other_key(3),
            refund_locktime: LockTime::from_time(1_700_000_000).unwrap(),
            exit_delay: RelativeTimelock::Seconds(512 * 168),
            unilateral_refund_delay: RelativeTimelock::Seconds(512 * 400),
        };
        let escrow = EntryEscrow::new(terms).unwrap();
        let record = EntryRecord {
            network: "signet".into(),
            coordinator_pubkey: String::new(),
            user_pubkey: String::new(),
            competition_id: Uuid::nil(),
            entry_id,
            entry_pubkey: hex::encode(key.point().serialize()),
            status: "escrowed".into(),
            escrow: Some(EscrowRecord {
                kind: "ark".into(),
                arkd_url: String::new(),
                server_pubkey: other_key(3).to_string(),
                outpoint: Some(format!("{}:1", "ab".repeat(32))),
                amount_sat: Some(50_000),
                refund_locktime: 1_700_000_000,
                exit_delay_secs: 512 * 168,
                unilateral_refund_delay_secs: 512 * 400,
                tap_tree: escrow
                    .vtxo_script()
                    .scripts()
                    .iter()
                    .map(|script| hex::encode(script.as_bytes()))
                    .collect(),
                created_at: 0,
            }),
            ticket: None,
            contract: None,
            updated_at: 0,
        };
        (record, escrow)
    }

    #[test]
    fn signs_the_unilateral_refund_leaf_with_the_entry_key() {
        let seed = WalletSeed::from_bytes([5; 32]);
        let entry_id = Uuid::now_v7();
        let key = seed.entry_key(Network::Signet, entry_id).unwrap();
        let (entry, script) = record(&key, entry_id, key.xonly());
        let escrow = Escrow::new(&entry, &key).unwrap().unwrap();
        assert_eq!(escrow.script, script);
        assert_eq!(escrow.refund_at(), 1_700_000_000);

        let destination = ScriptBuf::new_p2tr(&Secp256k1::new(), other_key(9), None);
        let tx = unilateral_refund_tx(
            &escrow,
            &key,
            destination,
            FeeRate::from_sat_per_vb(2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            tx.input[0].sequence,
            RelativeTimelock::Seconds(512 * 400).to_sequence().unwrap()
        );
        assert!(tx.output[0].value < escrow.amount);
        let witness: Vec<&[u8]> = tx.input[0].witness.iter().collect();
        assert_eq!(witness.len(), 3);
        assert_eq!(
            witness[1],
            script.script(EscrowPath::UnilateralRefund).as_bytes()
        );

        let prevout = TxOut {
            value: escrow.amount,
            script_pubkey: script.script_pubkey(),
        };
        let sighash = SighashCache::new(&tx)
            .taproot_script_spend_signature_hash(
                0,
                &Prevouts::All(&[prevout]),
                TapLeafHash::from_script(
                    script.script(EscrowPath::UnilateralRefund),
                    LeafVersion::TapScript,
                ),
                TapSighashType::Default,
            )
            .unwrap();
        let signature = schnorr::Signature::from_slice(witness[0]).unwrap();
        Secp256k1::verification_only()
            .verify_schnorr(
                &signature,
                &Message::from_digest(sighash.to_byte_array()),
                &key.xonly(),
            )
            .unwrap();
    }

    #[test]
    fn an_escrow_never_funded_holds_nothing() {
        let seed = WalletSeed::from_bytes([5; 32]);
        let entry_id = Uuid::now_v7();
        let key = seed.entry_key(Network::Signet, entry_id).unwrap();
        let (mut entry, _) = record(&key, entry_id, key.xonly());
        let escrow = entry.escrow.as_mut().unwrap();
        escrow.outpoint = None;
        escrow.amount_sat = None;
        assert!(Escrow::new(&entry, &key).unwrap().is_none());
    }

    #[test]
    fn refuses_an_escrow_locked_to_another_key() {
        let seed = WalletSeed::from_bytes([5; 32]);
        let entry_id = Uuid::now_v7();
        let key = seed.entry_key(Network::Signet, entry_id).unwrap();
        let (entry, _) = record(&key, entry_id, other_key(7));
        assert!(matches!(
            Escrow::new(&entry, &key),
            Err(Error::ForeignEntry(id)) if id == entry_id
        ));
    }
}
