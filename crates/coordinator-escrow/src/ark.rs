//! Rules for spending an entry's Arkade escrow VTXO into its pool's contract.
//!
//! Keymeld holds the entry key and signs escrow spends only through the Coordinator verifier.
//! The verifier derives every digest from the transactions here, never from the caller.
//!
//! - An intent proof may spend only this escrow, through its funding leaf.
//!   It may pay only the pool's funding output and a capped coordinator fee.
//!   It moves nothing itself, since the batch still needs every forfeit.
//! - A refund may spend only this escrow, through its refund leaf, once its locktime has passed.
//!   It may pay only the swap that sends the player's own money to their Lightning Address.
//! - A forfeit gives the escrow to the Arkade server once the batch's commitment transaction confirms.
//!   It must spend a connector that descends from that commitment transaction.
//!   The commitment transaction must pay the funding output.
//!   The pool's contract must already be completely signed for that funding outpoint, expiry transaction included.
//!
//! In an Arkade-funded pool, the contract is bound before the batch, with a null funding outpoint.
//! Each step that needs the real outpoint then presents the commitment transaction as [`ArkFunding`].

use coordinator_ark_escrow::{EntryEscrow, EscrowPath, RefundSwap, VtxoScript};
use dlctix::bitcoin::absolute::LockTime;
use dlctix::bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use dlctix::bitcoin::hashes::Hash;
use dlctix::bitcoin::sighash::{Prevouts, SighashCache};
use dlctix::bitcoin::taproot::LeafVersion;
pub use dlctix::bitcoin::XOnlyPublicKey;
use dlctix::bitcoin::{
    OutPoint, Psbt, ScriptBuf, TapLeafHash, TapSighashType, Transaction, TxOut,
};
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

/// A refund's spend of the escrow, once its transaction is known to be well formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundSpend {
    /// The BIP340 digest the player's entry key signs.
    pub digest: [u8; 32],
    /// The escrow's value, all of which the swap receives.
    pub value_sats: u64,
}

/// An escrow spend to sign as the entry's player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArkEscrowSpend {
    /// The intent proof registering the pool's escrows for a batch. Hex-encoded PSBT.
    IntentProof { proof_psbt: String },
    /// A refund of this escrow, after its locktime, into the swap that pays the player's
    /// Lightning Address. An offchain spend takes two transactions, both hex-encoded PSBTs, and
    /// the owner signs each in turn; `purpose` says which one this signature covers. The swap's
    /// `TapTree` field is hex.
    Refund {
        purpose: RefundPurpose,
        ark_psbt: String,
        checkpoint_psbt: String,
        swap_tap_tree: String,
    },
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

/// The entry's escrow as the player's wallet accepts it: [`escrow`], refundable by `latest_refund`.
///
/// `latest_refund` is a UNIX time, normally the contract's expiry, since an unfunded pool
/// must not hold a buy-in longer than a funded one would.
pub fn consented_escrow(
    policy: &ArkEscrowPolicy,
    participant: XOnlyPublicKey,
    market_maker: XOnlyPublicKey,
    latest_refund: Option<u32>,
) -> Result<EntryEscrow, ArkError> {
    let escrow = escrow(policy, participant, market_maker)?;
    let LockTime::Seconds(refund_at) = escrow.terms().refund_locktime else {
        return reject("the escrow's refund time is not a timestamp");
    };
    if latest_refund.is_some_and(|latest| refund_at.to_consensus_u32() > latest) {
        return reject("the escrow refunds after the contract expires");
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
        ArkEscrowSpend::Refund { .. } => {
            Ok(vec![(0, refund_from(escrow, policy, spend)?.1.digest)])
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

/// An Arkade offchain spend takes two transactions, and the owner signs both.
///
/// A checkpoint transaction spends the VTXO through one of its leaves into a checkpoint output;
/// an Ark transaction then spends that output and pays the receivers. The server co-signs
/// between the two, so the owner signs the Ark transaction first and the checkpoint second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefundPurpose {
    /// Spends the checkpoint output and pays the swap. Signed before the server co-signs.
    ArkTransaction,
    /// Spends the escrow. Signed after the server co-signs the Ark transaction.
    Checkpoint,
}

/// Check a refund's transactions, and give the digest that `purpose` signs.
///
/// A refund moves the whole escrow to the swap: the service takes its fee on the Lightning side,
/// by paying an invoice smaller than the VTXO it claims, so nothing may be withheld here. Both
/// transactions are checked whichever one is being signed, since the first signature is useless
/// unless the second pays where it should.
///
/// The checkpoint output is recomputed rather than read from the transaction. It is a taproot of
/// the leaf being spent and the server's exit script, which the player consented to; a proposed
/// refund that names any other output would be redirecting the money.
pub fn refund_spend(
    escrow: &EntryEscrow,
    swap: &RefundSwap,
    policy: &ArkEscrowPolicy,
    refund: &RefundTransactions,
    purpose: RefundPurpose,
) -> Result<RefundSpend, ArkError> {
    if swap.terms().player != escrow.terms().player {
        return reject("the swap belongs to another player");
    }
    if swap.terms().server != escrow.terms().server {
        return reject("the swap names another Arkade server");
    }
    let leaf = escrow.script(EscrowPath::Refund);
    let exit_script = hex::decode(&policy.checkpoint_exit_script)
        .map(ScriptBuf::from_bytes)
        .map_err(|e| ArkError(e.to_string()))?;
    let checkpoint_output = coordinator_ark_escrow::checkpoint_script_pubkey(leaf, &exit_script)
        .map_err(|e| ArkError(e.to_string()))?;

    // The checkpoint transaction spends the escrow, and only it.
    let checkpoint = &refund.checkpoint;
    if checkpoint.inputs.len() != 1 || checkpoint.unsigned_tx.input.len() != 1 {
        return reject("a refund's checkpoint transaction spends this escrow alone");
    }
    if own_inputs_through(escrow, checkpoint, EscrowPath::Refund)? != [0] {
        return reject("the checkpoint's only input must be this escrow, through its refund leaf");
    }
    let value = checkpoint.inputs[0]
        .witness_utxo
        .as_ref()
        .ok_or_else(|| ArkError("every input needs its witness UTXO".into()))?
        .value;
    open_at_refund_time(escrow, checkpoint)?;
    match checkpoint.unsigned_tx.output.as_slice() {
        [paid, anchor]
            if paid.script_pubkey == checkpoint_output
                && paid.value == value
                && is_anchor(anchor) => {}
        _ => return reject("the checkpoint pays something other than the whole escrow onward"),
    }

    // The Ark transaction spends that checkpoint output, and pays the swap.
    let ark = &refund.ark;
    if ark.inputs.len() != 1 || ark.unsigned_tx.input.len() != 1 {
        return reject("a refund's Ark transaction spends its checkpoint alone");
    }
    let spent = ark.unsigned_tx.input[0].previous_output;
    if spent.txid != checkpoint.unsigned_tx.compute_txid() || spent.vout != 0 {
        return reject("the Ark transaction does not spend this refund's checkpoint");
    }
    match ark.inputs[0].witness_utxo.as_ref() {
        Some(utxo) if *utxo == checkpoint.unsigned_tx.output[0] => {}
        _ => return reject("the Ark transaction's input is not the checkpoint output"),
    }
    // The checkpoint output is spent through the same leaf the escrow was.
    let mut leaves = ark.inputs[0].tap_scripts.values();
    match (leaves.next(), leaves.next()) {
        (Some((script, LeafVersion::TapScript)), None) if script == leaf => {}
        _ => return reject("the Ark transaction does not spend the checkpoint's refund leaf"),
    }
    open_at_refund_time(escrow, ark)?;
    match ark.unsigned_tx.output.as_slice() {
        [paid, anchor]
            if paid.script_pubkey == swap.script_pubkey()
                && paid.value == value
                && is_anchor(anchor) => {}
        _ => return reject("the refund pays something other than the whole escrow to the swap"),
    }

    let signed = match purpose {
        RefundPurpose::ArkTransaction => ark,
        RefundPurpose::Checkpoint => checkpoint,
    };
    Ok(RefundSpend {
        digest: sighash_through(escrow, signed, 0, EscrowPath::Refund)?,
        value_sats: value.to_sat(),
    })
}

/// A refund's transaction may only spend once the escrow's refund locktime has passed, and its
/// input must leave `CHECKLOCKTIMEVERIFY` enabled.
fn open_at_refund_time(escrow: &EntryEscrow, psbt: &Psbt) -> Result<(), ArkError> {
    let open = match (escrow.terms().refund_locktime, psbt.unsigned_tx.lock_time) {
        (LockTime::Seconds(until), LockTime::Seconds(at)) => {
            at.to_consensus_u32() >= until.to_consensus_u32()
        }
        (LockTime::Blocks(until), LockTime::Blocks(at)) => {
            at.to_consensus_u32() >= until.to_consensus_u32()
        }
        _ => return reject("the refund's locktime is not in the escrow's unit"),
    };
    if !open {
        return reject("the refund's locktime is before the escrow's");
    }
    // A final sequence disables CHECKLOCKTIMEVERIFY, which would spend the escrow at any time.
    if psbt.unsigned_tx.input[0].sequence.is_final() {
        return reject("the refund's input must not be final");
    }
    Ok(())
}

fn is_anchor(output: &TxOut) -> bool {
    output.script_pubkey == coordinator_ark_escrow::anchor_script_pubkey()
}

/// What the swap must pay the player over Lightning: the escrow, less the service's fee.
///
/// The player consented to the cap when entering, so a service that wants more is refused here
/// rather than after it has the money.
pub fn refund_invoice_sats(
    spend: &RefundSpend,
    fee_sats: u64,
    policy: &ArkEscrowPolicy,
) -> Result<u64, ArkError> {
    if fee_sats > policy.max_refund_fee_sats {
        return reject("the swap keeps more than the refund fee the player allowed");
    }
    spend
        .value_sats
        .checked_sub(fee_sats)
        .filter(|paid| *paid > 0)
        .ok_or_else(|| ArkError("the swap's fee leaves the player nothing".into()))
}

/// A refund's two transactions, as the coordinator built them.
pub struct RefundTransactions {
    pub ark: Psbt,
    pub checkpoint: Psbt,
}

/// The refund an [`ArkEscrowSpend::Refund`] carries: the swap it pays and its spend of the escrow.
pub fn refund_from(
    escrow: &EntryEscrow,
    policy: &ArkEscrowPolicy,
    spend: &ArkEscrowSpend,
) -> Result<(RefundSwap, RefundSpend), ArkError> {
    let ArkEscrowSpend::Refund {
        purpose,
        ark_psbt,
        checkpoint_psbt,
        swap_tap_tree,
    } = spend
    else {
        return reject("this spend is not a refund");
    };
    let tap_tree = hex::decode(swap_tap_tree).map_err(|e| ArkError(e.to_string()))?;
    let vtxo = VtxoScript::decode_tap_tree(&tap_tree).map_err(|e| ArkError(e.to_string()))?;
    let swap = RefundSwap::from_vtxo_script(&vtxo).map_err(|e| ArkError(e.to_string()))?;
    let transactions = RefundTransactions {
        ark: psbt(ark_psbt)?,
        checkpoint: psbt(checkpoint_psbt)?,
    };
    let spend = refund_spend(escrow, &swap, policy, &transactions, *purpose)?;
    Ok((swap, spend))
}

/// The soonest a refund swap's deadline may fall: the service needs time to pay the invoice.
pub const MIN_REFUND_DEADLINE_SECS: u32 = 15 * 60;
/// The latest: after this the player's own money is held too long for a payment that failed.
pub const MAX_REFUND_DEADLINE_SECS: u32 = 24 * 60 * 60;

/// Check the swap commits to the invoice the verifier itself resolved.
///
/// This is what makes the swap atomic: the service claims the VTXO only by revealing the
/// preimage of an invoice drawn from the player's own Lightning Address. Every later step
/// rechecks it against the prepared invoice, without resolving the address again.
pub fn check_refund_invoice(swap: &RefundSwap, payment_hash: [u8; 32]) -> Result<(), ArkError> {
    if swap.terms().payment_hash != payment_hash {
        return reject("the swap does not commit to the invoice the player is paid with");
    }
    Ok(())
}

/// Check how long the swap may hold the player's money, when the refund is prepared.
///
/// A payment that never happens leaves the money until the deadline, so a refund prepared now
/// must not name one far out. Later steps do not recheck this: by then the deadline is fixed.
pub fn check_refund_deadline(swap: &RefundSwap, now: u32) -> Result<(), ArkError> {
    let LockTime::Seconds(deadline) = swap.terms().deadline else {
        return reject("the swap's deadline must be a timestamp");
    };
    let deadline = deadline.to_consensus_u32();
    let (soonest, latest) = (
        now.saturating_add(MIN_REFUND_DEADLINE_SECS),
        now.saturating_add(MAX_REFUND_DEADLINE_SECS),
    );
    if deadline < soonest || deadline > latest {
        return reject("the swap's deadline is outside the window a refund may hold funds");
    }
    Ok(())
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
    own_inputs_through(escrow, psbt, EscrowPath::Funding)
}

/// The inputs of `psbt` that spend this escrow, each through `path`.
fn own_inputs_through(
    escrow: &EntryEscrow,
    psbt: &Psbt,
    path: EscrowPath,
) -> Result<Vec<usize>, ArkError> {
    let script_pubkey = escrow.script_pubkey();
    let leaf = escrow.script(path);
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
            (Some((script, LeafVersion::TapScript)), None) if script == leaf => own.push(index),
            _ => return reject("an escrow input is not spent through the expected leaf"),
        }
    }
    Ok(own)
}

/// The BIP341 script-path sighash, `SIGHASH_DEFAULT`, for spending `index` through the funding leaf.
fn sighash(escrow: &EntryEscrow, psbt: &Psbt, index: usize) -> Result<[u8; 32], ArkError> {
    sighash_through(escrow, psbt, index, EscrowPath::Funding)
}

/// The BIP341 script-path sighash, `SIGHASH_DEFAULT`, for spending `index` through `path`.
fn sighash_through(
    escrow: &EntryEscrow,
    psbt: &Psbt,
    index: usize,
    path: EscrowPath,
) -> Result<[u8; 32], ArkError> {
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
    let leaf_hash = TapLeafHash::from_script(escrow.script(path), LeafVersion::TapScript);
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

#[cfg(test)]
mod refund_tests {
    use super::*;
    use crate::authorization::ArkEscrowPolicy;
    use coordinator_ark_escrow::{EscrowTerms, RelativeTimelock, SwapTerms};
    use dlctix::bitcoin::transaction::Version;
    use dlctix::bitcoin::{Amount, Sequence, TxIn, Txid};

    const REFUND_AT: u32 = 1_790_000_000;
    const VALUE: Amount = Amount::from_sat(50_000);
    /// Stands in for the Arkade server's checkpoint exit script.
    const EXIT_SCRIPT: &str = "20aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac";

    fn xonly(byte: u8) -> XOnlyPublicKey {
        let secp = dlctix::bitcoin::key::Secp256k1::new();
        dlctix::bitcoin::secp256k1::SecretKey::from_slice(&[byte; 32])
            .unwrap()
            .x_only_public_key(&secp)
            .0
    }

    fn escrow() -> EntryEscrow {
        EntryEscrow::new(EscrowTerms {
            player: xonly(14),
            coordinator: xonly(18),
            server: xonly(21),
            refund_locktime: LockTime::from_consensus(REFUND_AT),
            exit_delay: RelativeTimelock::Seconds(2048),
            unilateral_refund_delay: RelativeTimelock::Seconds(2048 + 512 * 100),
        })
        .unwrap()
    }

    fn swap_for(player: u8) -> RefundSwap {
        RefundSwap::new(SwapTerms {
            player: xonly(player),
            swapper: xonly(30),
            server: xonly(21),
            payment_hash: [5u8; 32],
            deadline: LockTime::from_consensus(REFUND_AT + 3_600),
            exit_delay: RelativeTimelock::Seconds(2048),
            unilateral_reclaim_delay: RelativeTimelock::Seconds(2048 + 512 * 100),
        })
        .unwrap()
    }

    fn policy(max_refund_fee_sats: u64) -> ArkEscrowPolicy {
        ArkEscrowPolicy {
            escrow_tap_tree: hex::encode(escrow().vtxo_script().encode_tap_tree()),
            max_fee_sats: 500,
            max_refund_fee_sats,
            checkpoint_exit_script: EXIT_SCRIPT.into(),
        }
    }

    fn exit_script() -> ScriptBuf {
        ScriptBuf::from_bytes(hex::decode(EXIT_SCRIPT).unwrap())
    }

    fn anchor() -> TxOut {
        TxOut {
            value: Amount::ZERO,
            script_pubkey: coordinator_ark_escrow::anchor_script_pubkey(),
        }
    }

    /// One transaction of an offchain spend: `prevout` in, `outputs` out, spending `leaf`.
    fn offchain_tx(
        escrow: &EntryEscrow,
        prevout: (OutPoint, TxOut),
        leaf: EscrowPath,
        outputs: Vec<TxOut>,
        lock_time: LockTime,
    ) -> Psbt {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            version: Version::non_standard(3),
            lock_time,
            input: vec![TxIn {
                previous_output: prevout.0,
                sequence: Sequence::ENABLE_LOCKTIME_NO_RBF,
                ..Default::default()
            }],
            output: outputs,
        })
        .unwrap();
        psbt.inputs[0].witness_utxo = Some(prevout.1);
        psbt.inputs[0].tap_scripts.insert(
            escrow.control_block(leaf),
            (escrow.script(leaf).clone(), LeafVersion::TapScript),
        );
        psbt
    }

    /// A refund of the whole escrow into `swap`, as ark-core would build it.
    fn refund(escrow: &EntryEscrow, swap: &RefundSwap) -> RefundTransactions {
        refund_paying(
            escrow,
            vec![
                TxOut {
                    value: VALUE,
                    script_pubkey: swap.script_pubkey(),
                },
                anchor(),
            ],
        )
    }

    /// A refund whose Ark transaction pays `outputs`.
    fn refund_paying(escrow: &EntryEscrow, outputs: Vec<TxOut>) -> RefundTransactions {
        let checkpoint_output = TxOut {
            value: VALUE,
            script_pubkey: coordinator_ark_escrow::checkpoint_script_pubkey(
                escrow.script(EscrowPath::Refund),
                &exit_script(),
            )
            .unwrap(),
        };
        let checkpoint = offchain_tx(
            escrow,
            (
                OutPoint::new(Txid::from_byte_array([7u8; 32]), 0),
                TxOut {
                    value: VALUE,
                    script_pubkey: escrow.script_pubkey(),
                },
            ),
            EscrowPath::Refund,
            vec![checkpoint_output.clone(), anchor()],
            LockTime::from_consensus(REFUND_AT),
        );
        let ark = offchain_tx(
            escrow,
            (
                OutPoint::new(checkpoint.unsigned_tx.compute_txid(), 0),
                checkpoint_output,
            ),
            EscrowPath::Refund,
            outputs,
            LockTime::from_consensus(REFUND_AT),
        );
        RefundTransactions { ark, checkpoint }
    }

    fn spend(
        escrow: &EntryEscrow,
        swap: &RefundSwap,
        refund: &RefundTransactions,
        purpose: RefundPurpose,
    ) -> Result<RefundSpend, ArkError> {
        refund_spend(escrow, swap, &policy(100), refund, purpose)
    }

    #[test]
    fn each_transaction_of_the_refund_signs_its_own_digest() {
        let (escrow, swap) = (escrow(), swap_for(14));
        let refund = refund(&escrow, &swap);
        let checkpoint = spend(&escrow, &swap, &refund, RefundPurpose::Checkpoint).unwrap();
        let ark = spend(&escrow, &swap, &refund, RefundPurpose::ArkTransaction).unwrap();
        assert_eq!(checkpoint.value_sats, VALUE.to_sat());
        assert_eq!(ark.value_sats, VALUE.to_sat());
        assert_ne!(checkpoint.digest, ark.digest);
    }

    #[test]
    fn the_checkpoint_must_pay_the_server_script_the_player_consented_to() {
        let (escrow, swap) = (escrow(), swap_for(14));
        let mut elsewhere = refund(&escrow, &swap);
        elsewhere.checkpoint.unsigned_tx.output[0].script_pubkey = escrow.script_pubkey();
        assert!(spend(&escrow, &swap, &elsewhere, RefundPurpose::Checkpoint).is_err());
        // A checkpoint built for another server's exit script is a different output.
        let other_server = ArkEscrowPolicy {
            checkpoint_exit_script: "51".into(),
            ..policy(100)
        };
        assert!(refund_spend(
            &escrow,
            &swap,
            &other_server,
            &refund(&escrow, &swap),
            RefundPurpose::Checkpoint,
        )
        .is_err());
    }

    #[test]
    fn the_ark_transaction_must_spend_this_refunds_checkpoint() {
        let (escrow, swap) = (escrow(), swap_for(14));
        let mut detached = refund(&escrow, &swap);
        detached.ark.unsigned_tx.input[0].previous_output =
            OutPoint::new(Txid::from_byte_array([8u8; 32]), 0);
        assert!(spend(&escrow, &swap, &detached, RefundPurpose::ArkTransaction).is_err());
        assert!(spend(&escrow, &swap, &detached, RefundPurpose::Checkpoint).is_err());
    }

    #[test]
    fn both_transactions_wait_for_the_escrow_locktime() {
        let (escrow, swap) = (escrow(), swap_for(14));
        for early in [RefundPurpose::ArkTransaction, RefundPurpose::Checkpoint] {
            let mut refund = refund(&escrow, &swap);
            let at = LockTime::from_consensus(REFUND_AT - 1);
            match early {
                RefundPurpose::ArkTransaction => refund.ark.unsigned_tx.lock_time = at,
                RefundPurpose::Checkpoint => refund.checkpoint.unsigned_tx.lock_time = at,
            }
            assert!(spend(&escrow, &swap, &refund, RefundPurpose::Checkpoint).is_err());
        }
    }

    #[test]
    fn a_final_input_would_disable_the_locktime() {
        let (escrow, swap) = (escrow(), swap_for(14));
        let mut spendable_now = refund(&escrow, &swap);
        spendable_now.checkpoint.unsigned_tx.input[0].sequence = Sequence::MAX;
        assert!(spend(&escrow, &swap, &spendable_now, RefundPurpose::Checkpoint).is_err());
    }

    #[test]
    fn nothing_may_be_withheld_or_paid_elsewhere() {
        let (escrow, swap) = (escrow(), swap_for(14));
        let short = refund_paying(
            &escrow,
            vec![
                TxOut {
                    value: VALUE - Amount::from_sat(1_000),
                    script_pubkey: swap.script_pubkey(),
                },
                anchor(),
            ],
        );
        assert!(spend(&escrow, &swap, &short, RefundPurpose::ArkTransaction).is_err());

        let elsewhere = refund_paying(
            &escrow,
            vec![
                TxOut {
                    value: VALUE,
                    script_pubkey: escrow.script_pubkey(),
                },
                anchor(),
            ],
        );
        assert!(spend(&escrow, &swap, &elsewhere, RefundPurpose::ArkTransaction).is_err());

        // Change back to the coordinator, alongside the swap, is still withholding.
        let split = refund_paying(
            &escrow,
            vec![
                TxOut {
                    value: VALUE - Amount::from_sat(1_000),
                    script_pubkey: swap.script_pubkey(),
                },
                TxOut {
                    value: Amount::from_sat(1_000),
                    script_pubkey: escrow.script_pubkey(),
                },
                anchor(),
            ],
        );
        assert!(spend(&escrow, &swap, &split, RefundPurpose::ArkTransaction).is_err());
    }

    #[test]
    fn the_swap_must_be_this_players() {
        let escrow = escrow();
        let other = swap_for(15);
        assert!(spend(
            &escrow,
            &other,
            &refund(&escrow, &other),
            RefundPurpose::Checkpoint
        )
        .is_err());
    }

    #[test]
    fn a_refund_spends_this_escrow_alone_through_its_refund_leaf() {
        let (escrow, swap) = (escrow(), swap_for(14));
        let mut funding_leaf = refund(&escrow, &swap);
        funding_leaf.checkpoint.inputs[0].tap_scripts.clear();
        funding_leaf.checkpoint.inputs[0].tap_scripts.insert(
            escrow.control_block(EscrowPath::Funding),
            (
                escrow.script(EscrowPath::Funding).clone(),
                LeafVersion::TapScript,
            ),
        );
        assert!(spend(&escrow, &swap, &funding_leaf, RefundPurpose::Checkpoint).is_err());

        let mut two_inputs = refund(&escrow, &swap);
        two_inputs.checkpoint.unsigned_tx.input.push(TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([9u8; 32]), 0),
            sequence: Sequence::ENABLE_LOCKTIME_NO_RBF,
            ..Default::default()
        });
        two_inputs.checkpoint.inputs.push(Default::default());
        assert!(spend(&escrow, &swap, &two_inputs, RefundPurpose::Checkpoint).is_err());
    }

    #[test]
    fn the_swap_keeps_no_more_than_the_player_allowed() {
        let (escrow, swap) = (escrow(), swap_for(14));
        let refund = refund(&escrow, &swap);
        let spend = spend(&escrow, &swap, &refund, RefundPurpose::Checkpoint).unwrap();
        assert_eq!(
            refund_invoice_sats(&spend, 100, &policy(100)).unwrap(),
            VALUE.to_sat() - 100
        );
        assert!(refund_invoice_sats(&spend, 101, &policy(100)).is_err());
        // A fee that leaves the player nothing is refused even under a generous cap.
        assert!(refund_invoice_sats(&spend, VALUE.to_sat(), &policy(u64::MAX)).is_err());
    }

    #[test]
    fn the_swap_commits_to_the_invoice_the_player_is_paid_with() {
        let swap = swap_for(14);
        assert!(check_refund_invoice(&swap, [5u8; 32]).is_ok());
        assert!(check_refund_invoice(&swap, [6u8; 32]).is_err());
    }

    #[test]
    fn a_refund_holds_the_money_only_inside_its_window() {
        let swap = swap_for(14);
        // The swap's deadline is an hour after REFUND_AT, so move `now` to put that hour
        // outside each end of the window.
        let deadline_too_soon = REFUND_AT + 3_600 - MIN_REFUND_DEADLINE_SECS + 1;
        assert!(check_refund_deadline(&swap, deadline_too_soon).is_err());
        let deadline_too_far_out = REFUND_AT + 3_600 - MAX_REFUND_DEADLINE_SECS - 1;
        assert!(check_refund_deadline(&swap, deadline_too_far_out).is_err());
        assert!(check_refund_deadline(&swap, REFUND_AT).is_ok());
    }
}
