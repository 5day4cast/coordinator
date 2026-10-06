//! Fee-bumping a contract's pre-signed transactions through their anchors.
//!
//! A contract built with anchors (`dlc_anchor_settings`) carries a pay-to-anchor output on its
//! outcome, expiry and split transactions. When one of them has waited `cpfp_after_secs`
//! without confirming and pays less than the current estimate for `cpfp_conf_target` blocks,
//! the coordinator spends the anchor together with one confirmed coin of its LND wallet in a
//! child that lifts the pair to the estimate. A transaction is bumped again only once the
//! estimate has risen by a quarter, so the new child can replace the old one. Contracts built
//! without anchors are left as they are.

use super::*;
use bitcoin::Witness;
use dlctix::{anchor, CpfpFundingInput, SignedContract};

/// A child spending the anchor and one wallet coin into one change output is about 150 vB; the
/// coin must cover that, the parent's shortfall, and leave change.
const CPFP_CHILD_VBYTES: u64 = 200;

/// The weight of a wallet coin's input once LND signs it.
fn wallet_input_weight(script_pubkey: &ScriptBuf) -> Result<InputWeightPrediction, anyhow::Error> {
    if script_pubkey.is_p2tr() {
        Ok(InputWeightPrediction::P2TR_KEY_DEFAULT_SIGHASH)
    } else if script_pubkey.is_p2wpkh() {
        Ok(InputWeightPrediction::P2WPKH_MAX)
    } else if script_pubkey.is_p2sh() {
        Ok(InputWeightPrediction::NESTED_P2WPKH_MAX)
    } else {
        Err(anyhow!("Cannot size a wallet input paying {script_pubkey}"))
    }
}

/// Whether a parent bumped at `previous` should be bumped again for `target`: once the
/// estimate is a quarter higher, enough for the new child to replace the old one.
fn rebump_due(previous: Option<FeeRate>, target: FeeRate) -> bool {
    previous.is_none_or(|previous| {
        target.to_sat_per_kwu() >= previous.to_sat_per_kwu().saturating_mul(5) / 4
    })
}

impl Coordinator {
    pub fn with_dlc_anchors(mut self, settings: crate::config::DlcAnchorSettings) -> Self {
        self.dlc_anchors = settings;
        self
    }

    /// Bump `parent`, an outcome, expiry or split transaction of `signed_contract` broadcast at
    /// `broadcast_at` that has not confirmed, if it has an anchor and waited long enough. Best
    /// effort: a failure is logged and settlement carries on, since the transaction still pays
    /// its own pre-signed fee.
    pub(super) async fn bump_unconfirmed_presigned_tx(
        &self,
        competition_id: Uuid,
        signed_contract: &SignedContract,
        parent: &Transaction,
        broadcast_at: OffsetDateTime,
    ) {
        if !self.dlc_anchors.cpfp_enabled || anchor::find_anchor(parent).is_none() {
            return;
        }
        let waited = OffsetDateTime::now_utc() - broadcast_at;
        if waited < time::Duration::seconds(self.dlc_anchors.cpfp_after_secs as i64) {
            return;
        }
        let parent_txid = parent.compute_txid();
        match self.bump_presigned_tx(signed_contract, parent).await {
            Ok(Some(child)) => info!(
                "Competition {} bumped unconfirmed tx {} with CPFP child {}",
                competition_id, parent_txid, child
            ),
            Ok(None) => {}
            Err(error) => warn!(
                "Competition {} could not bump unconfirmed tx {}: {}",
                competition_id, parent_txid, error
            ),
        }
    }

    /// Bump the outcome's split transaction once it was broadcast and has not confirmed. The
    /// split transaction is read back from the chain backend, witness included.
    pub(super) async fn bump_unconfirmed_split_tx(
        &self,
        competition: &Competition,
        signed_contract: &SignedContract,
        outcome: &Outcome,
    ) {
        let (Some(broadcast_at), Some(split_tx)) = (
            competition.delta_broadcasted_at,
            signed_contract.unsigned_split_tx(outcome),
        ) else {
            return;
        };
        if !self.dlc_anchors.cpfp_enabled || anchor::find_anchor(split_tx).is_none() {
            return;
        }
        let split_txid = split_tx.compute_txid();
        match self.bitcoin.get_tx_confirmation_height(&split_txid).await {
            Ok(None) => {}
            Ok(Some(_)) => return,
            Err(error) => {
                warn!(
                    "Competition {} could not check split tx {}: {}",
                    competition.id, split_txid, error
                );
                return;
            }
        }
        match self.bitcoin.get_raw_transaction(&split_txid).await {
            Ok(split_tx) => {
                self.bump_unconfirmed_presigned_tx(
                    competition.id,
                    signed_contract,
                    &split_tx,
                    broadcast_at,
                )
                .await
            }
            Err(error) => warn!(
                "Competition {} could not read split tx {}: {}",
                competition.id, split_txid, error
            ),
        }
    }

    /// Build, sign and broadcast a CPFP child for `parent` if it pays less than the current
    /// estimate. Returns the child's txid, or `None` when no bump is due.
    async fn bump_presigned_tx(
        &self,
        signed_contract: &SignedContract,
        parent: &Transaction,
    ) -> Result<Option<Txid>, anyhow::Error> {
        let parent_txid = parent.compute_txid();
        let (_, anchor_output) =
            anchor::find_anchor(parent).ok_or_else(|| anyhow!("Transaction has no anchor"))?;
        let anchor_output = anchor_output.clone();
        let parent_fee = signed_contract
            .presigned_tx_fee(parent)
            .ok_or_else(|| anyhow!("Transaction is not part of the contract"))?;
        let parent_vsize = parent.weight().to_vbytes_ceil();

        let sat_per_vb = self
            .bitcoin
            .estimate_fee(self.dlc_anchors.cpfp_conf_target)
            .await?;
        if !sat_per_vb.is_finite() || sat_per_vb <= 0.0 {
            return Err(anyhow!("Invalid fee estimate: {sat_per_vb} sat/vB"));
        }
        let target = FeeRate::from_sat_per_kwu((sat_per_vb * 250.0).ceil() as u64);
        let parent_target_fee = target
            .fee_vb(parent_vsize)
            .ok_or_else(|| anyhow!("Fee overflows"))?;
        if parent_fee >= parent_target_fee {
            return Ok(None);
        }
        let previous = self.cpfp_bumps.lock().unwrap().get(&parent_txid).copied();
        if !rebump_due(previous, target) {
            return Ok(None);
        }
        // Recorded before trying, so a child the network refuses is not retried every step.
        self.cpfp_bumps.lock().unwrap().insert(parent_txid, target);

        let needed = target
            .fee_vb(parent_vsize + CPFP_CHILD_VBYTES)
            .and_then(|fee| fee.checked_add(Amount::from_sat(1_000)))
            .ok_or_else(|| anyhow!("Fee overflows"))?;
        let coin = self.bitcoin.get_spendable_utxo(needed.to_sat()).await?;
        let change = self.bitcoin.get_next_address().await?.script_pubkey();
        let child = signed_contract.cpfp_child_template(
            parent,
            &[CpfpFundingInput {
                outpoint: coin.outpoint,
                prevout: coin.txout.clone(),
                weight: wallet_input_weight(&coin.txout.script_pubkey)?,
            }],
            change,
            target,
        )?;

        let mut psbt = Psbt::from_unsigned_tx(child)?;
        // The anchor needs no signature: its final witness is empty. LND signs and finalizes
        // the wallet coin, and needs every prevout to compute a taproot sighash.
        psbt.inputs[0].witness_utxo = Some(anchor_output);
        psbt.inputs[0].final_script_witness = Some(Witness::new());
        psbt.inputs[1].witness_utxo = Some(coin.txout);
        if !self.bitcoin.sign_psbt(&mut psbt).await? {
            return Err(anyhow!("LND did not sign the CPFP child's wallet input"));
        }
        let child = psbt.extract_tx()?;
        self.broadcast_or_known(&child).await?;
        Ok(Some(child.compute_txid()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebumps_wait_for_a_quarter_higher_estimate() {
        let rate = FeeRate::from_sat_per_vb_u32;
        assert!(rebump_due(None, rate(5)));
        assert!(!rebump_due(Some(rate(20)), rate(20)));
        assert!(!rebump_due(Some(rate(20)), rate(24)));
        assert!(rebump_due(Some(rate(20)), rate(25)));
    }

    #[test]
    fn wallet_inputs_are_sized_by_script() {
        let key = Scalar::from_slice(&[3; 32]).unwrap().base_point_mul();
        let p2tr = ScriptBuf::new_p2tr_tweaked(TweakedPublicKey::dangerous_assume_tweaked(
            dlctix::convert_point(key),
        ));
        assert_eq!(
            wallet_input_weight(&p2tr).unwrap().weight(),
            InputWeightPrediction::P2TR_KEY_DEFAULT_SIGHASH.weight()
        );
        assert!(wallet_input_weight(&ScriptBuf::new()).is_err());
    }
}
