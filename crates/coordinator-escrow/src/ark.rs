//! Rules for spending an entry's Arkade escrow VTXO into its pool's contract.
//!
//! Keymeld holds the entry key and signs escrow spends only through the Coordinator verifier.
//! The verifier derives every digest from the transactions here, never from the caller.
//!
//! - An intent proof may spend only this escrow, through its funding leaf.
//!   It may pay only the pool's funding output and a capped coordinator fee.
//!   It moves nothing itself, since the batch still needs every forfeit.
//! - A forfeit gives the escrow to the Arkade server once the batch's commitment transaction confirms.
//!   It must spend a connector that descends from that commitment transaction.
//!   The commitment transaction must pay the funding output.
//!   The pool's contract must already be completely signed for that funding outpoint, expiry transaction included.
//!
//! In an Arkade-funded pool, the contract is bound before the batch, with a null funding outpoint.
//! Each step that needs the real outpoint then presents the commitment transaction as [`ArkFunding`].

use coordinator_ark_escrow::{EntryEscrow, EscrowPath, VtxoScript};
use dlctix::bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use dlctix::bitcoin::hashes::Hash;
use dlctix::bitcoin::sighash::{Prevouts, SighashCache};
use dlctix::bitcoin::taproot::LeafVersion;
pub use dlctix::bitcoin::XOnlyPublicKey;
use dlctix::bitcoin::{OutPoint, Psbt, TapLeafHash, TapSighashType, Transaction, TxOut};
use dlctix::{ContractSignatures, Outcome};
use serde::{Deserialize, Serialize};

use crate::authorization::ArkEscrowPolicy;
use crate::payout::{self, ContractCommitment};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ArkError(String);

fn reject<T>(message: impl Into<String>) -> Result<T, ArkError> {
    Err(ArkError(message.into()))
}

/// The commitment transaction that funds an Arkade pool, and its funding output's index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArkFunding {
    /// Consensus hex of the batch's commitment transaction.
    pub commitment_tx: String,
    pub vout: u32,
}

impl ArkFunding {
    pub fn new(commitment_tx: &Transaction, vout: u32) -> Self {
        Self {
            commitment_tx: serialize_hex(commitment_tx),
            vout,
        }
    }
}

/// An escrow spend to sign as the entry's player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArkEscrowSpend {
    /// The intent proof registering the pool's escrows for a batch. Hex-encoded PSBT.
    IntentProof { proof_psbt: String },
    /// A forfeit of this escrow into the batch that funds the pool.
    Forfeit {
        /// Hex-encoded PSBT.
        forfeit_psbt: String,
        funding: ArkFunding,
        /// Consensus hex of the connector transactions, from the forfeit's connector to the one spending the commitment transaction.
        connector_txs: Vec<String>,
        /// The pool contract's complete signature set, as JSON.
        contract_signatures: String,
    },
}

/// The entry's escrow, from its consented tap tree.
///
/// Its player key must be `participant`, the deposited entry key.
/// Its coordinator key must be `market_maker`, so only the pool's market maker can fund with it.
pub fn escrow(
    policy: &ArkEscrowPolicy,
    participant: XOnlyPublicKey,
    market_maker: XOnlyPublicKey,
) -> Result<EntryEscrow, ArkError> {
    let tap_tree = hex::decode(&policy.escrow_tap_tree).map_err(|e| ArkError(e.to_string()))?;
    let vtxo = VtxoScript::decode_tap_tree(&tap_tree).map_err(|e| ArkError(e.to_string()))?;
    let escrow = EntryEscrow::from_vtxo_script(&vtxo).map_err(|e| ArkError(e.to_string()))?;
    if escrow.terms().player != participant {
        return reject("the escrow's player key is not the deposited entry key");
    }
    if escrow.terms().coordinator != market_maker {
        return reject("the escrow's coordinator key is not the pool's market maker");
    }
    Ok(escrow)
}

/// The funded contract: the bound contract, at the outpoint `funding` pays.
///
/// The bound funding outpoint must be null, meaning the Arkade batch sets it.
pub fn funded_contract(
    bound: &ContractCommitment,
    funding: &ArkFunding,
) -> Result<ContractCommitment, ArkError> {
    if bound.funding_outpoint != OutPoint::null() {
        return reject("an Arkade-funded contract is bound without a funding outpoint");
    }
    let commitment: Transaction =
        deserialize_hex(&funding.commitment_tx).map_err(|e| ArkError(e.to_string()))?;
    let funding_output = bound
        .contract_parameters
        .funding_output()
        .map_err(|e| ArkError(e.to_string()))?;
    if commitment.output.get(funding.vout as usize) != Some(&funding_output) {
        return reject("the commitment transaction does not pay the funding output there");
    }
    Ok(ContractCommitment {
        contract_parameters: bound.contract_parameters.clone(),
        funding_outpoint: OutPoint::new(commitment.compute_txid(), funding.vout),
    })
}

/// The inputs this escrow's player must sign for `spend`, each with its sighash.
pub fn spend_digests(
    escrow: &EntryEscrow,
    policy: &ArkEscrowPolicy,
    bound: &ContractCommitment,
    spend: &ArkEscrowSpend,
) -> Result<Vec<(usize, [u8; 32])>, ArkError> {
    match spend {
        ArkEscrowSpend::IntentProof { proof_psbt } => {
            let proof = psbt(proof_psbt)?;
            let funding_output = bound
                .contract_parameters
                .funding_output()
                .map_err(|e| ArkError(e.to_string()))?;
            intent_proof_digests(escrow, policy.max_fee_sats, &funding_output, &proof)
        }
        ArkEscrowSpend::Forfeit {
            forfeit_psbt,
            funding,
            connector_txs,
            contract_signatures,
        } => {
            let contract = funded_contract(bound, funding)?;
            let signatures: ContractSignatures =
                serde_json::from_str(contract_signatures).map_err(|e| ArkError(e.to_string()))?;
            let expires = contract
                .contract_parameters
                .outcome_payouts
                .contains_key(&Outcome::Expiry);
            if expires && signatures.expiry_tx_signature.is_none() {
                return reject("the contract's expiry transaction is not signed");
            }
            payout::verify_completed_contract(&contract, &signatures)
                .map_err(|e| ArkError(e.to_string()))?;
            let connectors = connector_txs
                .iter()
                .map(|tx| deserialize_hex::<Transaction>(tx).map_err(|e| ArkError(e.to_string())))
                .collect::<Result<Vec<_>, _>>()?;
            let forfeit = psbt(forfeit_psbt)?;
            Ok(vec![(
                1,
                forfeit_digest(
                    escrow,
                    &forfeit,
                    &connectors,
                    contract.funding_outpoint.txid,
                )?,
            )])
        }
    }
}

/// This escrow's inputs in an intent proof, each with its sighash.
///
/// Input 0 is the BIP322 message input, locked like the first escrow, so it may be this escrow's too.
pub fn intent_proof_digests(
    escrow: &EntryEscrow,
    max_fee_sats: u64,
    funding_output: &TxOut,
    proof: &Psbt,
) -> Result<Vec<(usize, [u8; 32])>, ArkError> {
    let outputs = &proof.unsigned_tx.output;
    let escrows = proof.inputs.len().saturating_sub(1) as u64;
    match outputs.as_slice() {
        [funding] if funding == funding_output => {}
        [funding, fee]
            if funding == funding_output
                && fee.script_pubkey != funding_output.script_pubkey
                && fee.value.to_sat() <= max_fee_sats.saturating_mul(escrows) => {}
        _ => return reject("the intent pays something other than the pool and its fee"),
    }
    let digests = own_inputs(escrow, proof)?
        .into_iter()
        .map(|index| Ok((index, sighash(escrow, proof, index)?)))
        .collect::<Result<Vec<_>, _>>()?;
    if digests.is_empty() {
        return reject("the intent proof does not spend this escrow");
    }
    Ok(digests)
}

/// The digest for this escrow's input in a forfeit.
///
/// Input 0 must spend a connector whose chain of connector transactions ends at the commitment transaction.
/// The forfeit is therefore void unless that commitment transaction confirms.
pub fn forfeit_digest(
    escrow: &EntryEscrow,
    forfeit: &Psbt,
    connectors: &[Transaction],
    commitment_txid: dlctix::bitcoin::Txid,
) -> Result<[u8; 32], ArkError> {
    let inputs = &forfeit.unsigned_tx.input;
    if inputs.len() != 2 || forfeit.inputs.len() != 2 {
        return reject("a forfeit spends a connector and one escrow");
    }
    let Some(leaf) = connectors.first() else {
        return reject("a forfeit needs its connector transactions");
    };
    if inputs[0].previous_output.txid != leaf.compute_txid() {
        return reject("the forfeit does not spend the given connector");
    }
    for pair in connectors.windows(2) {
        if pair[0].input.len() != 1
            || pair[0].input[0].previous_output.txid != pair[1].compute_txid()
        {
            return reject("the connector transactions do not form a chain");
        }
    }
    let root = connectors.last().expect("checked non-empty");
    if root.input.len() != 1 || root.input[0].previous_output.txid != commitment_txid {
        return reject("the connectors do not descend from the commitment transaction");
    }
    if own_inputs(escrow, forfeit)? != [1] {
        return reject("the forfeit's second input must be this escrow, and only it");
    }
    sighash(escrow, forfeit, 1)
}

/// The inputs of `psbt` that spend this escrow, each through its funding leaf.
fn own_inputs(escrow: &EntryEscrow, psbt: &Psbt) -> Result<Vec<usize>, ArkError> {
    let script_pubkey = escrow.script_pubkey();
    let funding_leaf = escrow.script(EscrowPath::Funding);
    let mut own = Vec::new();
    for (index, input) in psbt.inputs.iter().enumerate() {
        let spends_escrow = input
            .witness_utxo
            .as_ref()
            .is_some_and(|utxo| utxo.script_pubkey == script_pubkey);
        if !spends_escrow {
            continue;
        }
        let mut leaves = input.tap_scripts.values();
        match (leaves.next(), leaves.next()) {
            (Some((script, LeafVersion::TapScript)), None) if script == funding_leaf => {
                own.push(index)
            }
            _ => return reject("an escrow input is not spent through its funding leaf"),
        }
    }
    Ok(own)
}

/// The BIP341 script-path sighash, `SIGHASH_DEFAULT`, for spending `index` through the funding leaf.
fn sighash(escrow: &EntryEscrow, psbt: &Psbt, index: usize) -> Result<[u8; 32], ArkError> {
    let prevouts = psbt
        .inputs
        .iter()
        .map(|input| {
            input
                .witness_utxo
                .clone()
                .ok_or_else(|| ArkError("every input needs its witness UTXO".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let leaf_hash =
        TapLeafHash::from_script(escrow.script(EscrowPath::Funding), LeafVersion::TapScript);
    let sighash = SighashCache::new(&psbt.unsigned_tx)
        .taproot_script_spend_signature_hash(
            index,
            &Prevouts::All(&prevouts),
            leaf_hash,
            TapSighashType::Default,
        )
        .map_err(|e| ArkError(e.to_string()))?;
    Ok(sighash.to_byte_array())
}

fn psbt(hex_psbt: &str) -> Result<Psbt, ArkError> {
    let bytes = hex::decode(hex_psbt).map_err(|e| ArkError(e.to_string()))?;
    Psbt::deserialize(&bytes).map_err(|e| ArkError(e.to_string()))
}

/// Hex-encode a PSBT for [`ArkEscrowSpend`].
pub fn psbt_hex(psbt: &Psbt) -> String {
    hex::encode(psbt.serialize())
}
