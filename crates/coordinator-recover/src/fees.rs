//! Which recovery transactions can be fee bumped, and the child that bumps them.
//!
//! dlctix 0.1's outcome, expiry and split transactions are signed at a fixed fee rate and have no
//! anchor output. The win transaction cannot pay for them either: it waits `delta` blocks behind
//! the split. So a recovery of such a contract that does not confirm in a fee spike can only
//! wait. Contracts built with dlctix 0.2's anchors carry a pay-to-anchor (P2A) output on each of
//! those transactions instead: anyone can spend it in a child transaction that pays for both
//! (CPFP). [`bump_status`] finds one; an [`AnchorBumper`] builds the child, and [`FeeCoin`] is the
//! one the CLI uses, paying from a single coin of the player's own.

use bitcoin::key::{Keypair, Secp256k1, TapTweak};
use bitcoin::secp256k1::Message;
use bitcoin::sighash::{EcdsaSighashType, Prevouts, SighashCache, TapSighashType};
use bitcoin::transaction::{InputWeightPrediction, Version};
use bitcoin::{
    ecdsa, taproot, Amount, CompressedPublicKey, FeeRate, OutPoint, PrivateKey, ScriptBuf,
    Transaction, TxOut, Witness,
};
use dlctix::anchor::{cpfp_child_template, find_anchor, CpfpFundingInput};
use serde::Serialize;

/// The pay-to-anchor output script: `OP_1 <0x4e73>`.
pub fn p2a_script() -> ScriptBuf {
    ScriptBuf::from_bytes(vec![0x51, 0x02, 0x4e, 0x73])
}

/// Whether a transaction can be fee bumped, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BumpStatus {
    /// No anchor: the transaction pays what it was signed with, and cannot be bumped.
    Fixed { fee_sat: Option<u64> },
    /// A child spending this anchor can pay for the transaction. `fee_sat` is what the
    /// transaction pays on its own, if known.
    Anchor {
        outpoint: OutPoint,
        value_sat: u64,
        fee_sat: Option<u64>,
    },
}

impl BumpStatus {
    pub fn describe(&self) -> String {
        match self {
            BumpStatus::Fixed { fee_sat: Some(fee) } => format!(
                "pays the fixed {fee} sat fee it was signed with; it has no anchor and cannot be fee bumped"
            ),
            BumpStatus::Fixed { fee_sat: None } => {
                "pays the fee it was signed with; it has no anchor and cannot be fee bumped".into()
            }
            BumpStatus::Anchor {
                outpoint,
                fee_sat: Some(fee),
                ..
            } => format!(
                "pays {fee} sat on its own and can be fee bumped by a child spending its anchor {outpoint}"
            ),
            BumpStatus::Anchor { outpoint, .. } => {
                format!("can be fee bumped by a child spending its anchor {outpoint}")
            }
        }
    }

    /// The fee an anchored transaction pays on its own, if that is below `target` for its size:
    /// the transaction a child should bump. `None` when it has no anchor or pays enough.
    pub fn below(&self, tx: &Transaction, target: FeeRate) -> Option<Amount> {
        let BumpStatus::Anchor {
            fee_sat: Some(fee), ..
        } = self
        else {
            return None;
        };
        let fee = Amount::from_sat(*fee);
        let wanted = target.fee_vb(tx.weight().to_vbytes_ceil())?;
        (fee < wanted).then_some(fee)
    }
}

/// `tx`'s anchor, if it has one. `fee` is what it pays, if known (its inputs' values).
pub fn bump_status(tx: &Transaction, fee: Option<Amount>) -> BumpStatus {
    match find_anchor(tx) {
        Some((outpoint, output)) => BumpStatus::Anchor {
            outpoint,
            value_sat: output.value.to_sat(),
            fee_sat: fee.map(Amount::to_sat),
        },
        None => BumpStatus::Fixed {
            fee_sat: fee.map(Amount::to_sat),
        },
    }
}

/// Builds a signed child that spends a parent's anchor so that the parent, paying `parent_fee`
/// on its own, and the child together pay `fee_rate`. The child goes out with its parent, as a
/// package where the parent cannot relay alone.
pub trait AnchorBumper {
    fn bump(
        &self,
        parent: &Transaction,
        parent_fee: Amount,
        fee_rate: FeeRate,
    ) -> Result<Transaction, String>;
}

/// One confirmed coin of the player's, spendable by a single key, that pays for CPFP children,
/// with the change going to an address of theirs. The coin's output is looked up on chain, not
/// taken from the player, and the key must be the one it pays: a P2WPKH output for the key, or a
/// P2TR output for the key with no script tree (BIP 86), as single-key wallets make them. One coin
/// pays for one child.
pub struct FeeCoin {
    outpoint: OutPoint,
    prevout: TxOut,
    key: PrivateKey,
    change: ScriptBuf,
}

impl FeeCoin {
    pub fn new(
        outpoint: OutPoint,
        prevout: TxOut,
        key: PrivateKey,
        change: ScriptBuf,
    ) -> Result<Self, String> {
        let coin = Self {
            outpoint,
            prevout,
            key,
            change,
        };
        coin.input_weight()?;
        Ok(coin)
    }

    pub fn outpoint(&self) -> OutPoint {
        self.outpoint
    }

    fn input_weight(&self) -> Result<InputWeightPrediction, String> {
        let secp = Secp256k1::new();
        let script = &self.prevout.script_pubkey;
        if *script == self.p2wpkh_script()? {
            Ok(InputWeightPrediction::P2WPKH_MAX)
        } else if *script == ScriptBuf::new_p2tr(&secp, self.keypair().x_only_public_key().0, None)
        {
            Ok(InputWeightPrediction::P2TR_KEY_DEFAULT_SIGHASH)
        } else {
            Err(format!(
                "the fee key does not spend {}: it must pay the key's P2WPKH or single-key P2TR address",
                self.outpoint
            ))
        }
    }

    fn p2wpkh_script(&self) -> Result<ScriptBuf, String> {
        let pubkey = CompressedPublicKey::from_private_key(&Secp256k1::new(), &self.key)
            .map_err(|_| "the fee key must be a compressed key".to_owned())?;
        Ok(ScriptBuf::new_p2wpkh(&pubkey.wpubkey_hash()))
    }

    fn keypair(&self) -> Keypair {
        Keypair::from_secret_key(&Secp256k1::new(), &self.key.inner)
    }

    /// Sign the coin's input, input 1 of `child`; input 0 is the anchor.
    fn sign(&self, child: &mut Transaction, anchor_output: TxOut) -> Result<(), String> {
        let secp = Secp256k1::new();
        let prevouts = [anchor_output, self.prevout.clone()];
        let mut cache = SighashCache::new(&*child);
        let witness = if self.prevout.script_pubkey.is_p2wpkh() {
            let sighash = cache
                .p2wpkh_signature_hash(
                    1,
                    &self.prevout.script_pubkey,
                    self.prevout.value,
                    EcdsaSighashType::All,
                )
                .map_err(|e| e.to_string())?;
            let signature = ecdsa::Signature::sighash_all(
                secp.sign_ecdsa(&Message::from(sighash), &self.key.inner),
            );
            Witness::p2wpkh(&signature, &self.key.public_key(&secp).inner)
        } else {
            let sighash = cache
                .taproot_key_spend_signature_hash(
                    1,
                    &Prevouts::All(&prevouts),
                    TapSighashType::Default,
                )
                .map_err(|e| e.to_string())?;
            let tweaked = self.keypair().tap_tweak(&secp, None).to_keypair();
            let signature = taproot::Signature {
                signature: secp.sign_schnorr_no_aux_rand(&Message::from(sighash), &tweaked),
                sighash_type: TapSighashType::Default,
            };
            Witness::p2tr_key_spend(&signature)
        };
        child.input[1].witness = witness;
        Ok(())
    }
}

impl AnchorBumper for FeeCoin {
    fn bump(
        &self,
        parent: &Transaction,
        parent_fee: Amount,
        fee_rate: FeeRate,
    ) -> Result<Transaction, String> {
        let (_, anchor_output) =
            find_anchor(parent).ok_or("the transaction has no anchor to spend")?;
        let anchor_output = anchor_output.clone();
        let mut child = cpfp_child_template(
            parent,
            parent_fee,
            &[CpfpFundingInput {
                outpoint: self.outpoint,
                prevout: self.prevout.clone(),
                weight: self.input_weight()?,
            }],
            self.change.clone(),
            fee_rate,
        )
        .map_err(|e| format!("cannot pay for the child from {}: {e}", self.outpoint))?;
        // A TRUC (version 3) parent, as Arkade's virtual transactions are, takes only a
        // version 3 child.
        if parent.version == Version(3) {
            child.version = Version(3);
        }
        self.sign(&mut child, anchor_output)?;
        Ok(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::transaction::Version;

    #[test]
    fn finds_a_p2a_anchor_or_reports_a_fixed_fee() {
        let mut tx = Transaction {
            version: Version(3),
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        assert_eq!(
            bump_status(&tx, Some(Amount::from_sat(300))),
            BumpStatus::Fixed { fee_sat: Some(300) }
        );
        tx.output.push(TxOut {
            value: Amount::from_sat(240),
            script_pubkey: p2a_script(),
        });
        assert!(matches!(
            bump_status(&tx, None),
            BumpStatus::Anchor { outpoint, value_sat: 240, fee_sat: None } if outpoint.vout == 1
        ));
    }
}
