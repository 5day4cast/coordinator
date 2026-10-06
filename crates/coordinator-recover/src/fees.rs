//! Which recovery transactions can be fee bumped.
//!
//! Outcome, expiry and split transactions are signed at a fixed fee rate. The win transaction
//! cannot pay for them: it waits `delta` blocks behind the split. Contracts built without anchors
//! (every contract from before dlctix 0.2.0) cannot be bumped, so a recovery that does not
//! confirm in a fee spike can only wait. Contracts built with dlctix 0.2.0's anchors put a
//! pay-to-anchor (P2A) output last on each of those transactions: anyone can spend it in a child
//! transaction that pays for both (CPFP). [`bump_status`] finds it with dlctix's
//! `anchor::find_anchor`; an [`AnchorBumper`] builds the child, for example with dlctix's
//! `SignedContract::cpfp_child_template` and the bumper's own coins. None is implemented here
//! yet.

use bitcoin::{Amount, FeeRate, OutPoint, ScriptBuf, Transaction, TxOut};
use serde::Serialize;

/// The pay-to-anchor output script: `OP_1 <0x4e73>`.
pub fn p2a_script() -> ScriptBuf {
    dlctix::anchor::anchor_script_pubkey()
}

/// Whether a transaction can be fee bumped, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BumpStatus {
    /// No anchor: the transaction pays what it was signed with, and cannot be bumped.
    Fixed { fee_sat: Option<u64> },
    /// A child spending this anchor can pay for the transaction.
    Anchor { outpoint: OutPoint, value_sat: u64 },
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
            BumpStatus::Anchor { outpoint, .. } => {
                format!("can be fee bumped by a child spending its anchor {outpoint}")
            }
        }
    }
}

/// `tx`'s anchor, if it has one. `fee` is what it pays, if known (its inputs' values).
pub fn bump_status(tx: &Transaction, fee: Option<Amount>) -> BumpStatus {
    match dlctix::anchor::find_anchor(tx) {
        Some((outpoint, output)) => BumpStatus::Anchor {
            outpoint,
            value_sat: output.value.to_sat(),
        },
        None => BumpStatus::Fixed {
            fee_sat: fee.map(Amount::to_sat),
        },
    }
}

/// Builds a child that spends a parent's anchor so the pair pays `fee_rate`: the hook a wallet
/// (or the anchors work) implements. The child must be broadcast with its parent as a package.
pub trait AnchorBumper {
    fn bump(
        &self,
        parent: &Transaction,
        anchor: OutPoint,
        anchor_output: &TxOut,
        fee_rate: FeeRate,
    ) -> Result<Transaction, String>;
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
            BumpStatus::Anchor { outpoint, value_sat: 240 } if outpoint.vout == 1
        ));
    }
}
