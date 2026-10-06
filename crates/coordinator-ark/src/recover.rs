//! Recovery: one Arkade batch gives an expired escrow's value back, as the refund's swap.
//!
//! A VTXO lives until the batch it descends from expires. The server then sweeps its coins, and
//! refuses to spend it offchain (`VTXO_RECOVERABLE`), so [`crate::build_refund`] no longer
//! applies. The server still owes the value, and pays it out in a batch to an intent that proves
//! it owns the VTXO:
//!
//! 1. Register an intent. Its proof signs the escrow's refund leaf as the player, and its only
//!    output is the refund's swap, off chain, for the escrow's whole value.
//! 2. When a batch selects the intent, confirm it and collect the batch's VTXO tree.
//! 3. Check that the tree pays the swap, and that it spends from the batch's commitment
//!    transaction. Then cosign it: an intent that receives a VTXO lists a cosigner key, which
//!    signs every tree transaction on the way to that VTXO with the server.
//! 4. Return once the server finalizes the batch.
//!
//! Nothing is forfeited: the server already holds a swept VTXO's coins, so the recovery needs no
//! connector and no signature after the tree's. The cosigner key is made for the one batch and
//! dropped with it, like ark-client's: the tree's transactions are signed once and never again.
//!
//! Like a kickoff's, the intent holds the escrow in arkd's queue until a batch confirms it. So a
//! recovery first deletes any intent an earlier attempt left, and deletes its own if it fails
//! before the tree is signed. A batch that selected the intent has taken it out of the queue,
//! and arkd puts it back only if it was never confirmed: the next attempt's first step deletes
//! that one.
//!
//! Refusing to sign a tree, as one that does not pay the swap, fails the batch, and arkd then
//! bans the escrow's script for its ban duration, as it does for a missing forfeit.

use std::collections::HashMap;
use std::sync::Arc;

use ark_core::batch::{aggregate_nonces, generate_nonce_tree, sign_batch_tree_tx, NonceKps};
use ark_core::intent::{self, make_intent, Intent, IntentMessage};
use ark_core::server::{BatchTreeEventType, Info, PartialSigTree, StreamEvent};
use ark_core::TxGraph;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::DisplayHex;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::taproot::LeafVersion;
use bitcoin::{OutPoint, Psbt, ScriptBuf, Sequence, TapLeafHash, TxOut, Txid};
use coordinator_ark_escrow::{EscrowPath, RefundSwap};
use futures::StreamExt;
use tokio::time::{timeout_at, Instant};

use crate::kickoff::unix_now;
use crate::signer::{collect_signatures, insert_signature};
use crate::{
    delete_escrow_intent, script_spend_sighash, ArkTransport, Error, EscrowInput, EscrowSigner,
    EventStream, KickoffConfig, SigningPurpose, SigningRequest,
};

/// An expired escrow, recovered into its refund's swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    pub intent_id: String,
    pub batch_id: String,
    pub commitment_txid: Txid,
    /// The swap's new VTXO, a leaf of the batch's VTXO tree.
    pub swap_vtxo: OutPoint,
}

/// Recover `input`, an escrow whose VTXO the server swept, into `swap` in the next batch that
/// selects it. See the module documentation for the steps.
///
/// `player` signs the intent proof over the escrow's refund leaf, which opens at its refund
/// locktime. `player` and `coordinator` sign the proofs that delete a leftover intent, over its
/// funding leaf, as for a kickoff. The server takes no fee from the swap: it receives the
/// escrow's whole value, which is what a refund's verifier signs for.
pub async fn recover_escrow<T: ArkTransport + ?Sized>(
    transport: &T,
    info: &Info,
    input: &EscrowInput,
    swap: &RefundSwap,
    player: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
    config: &KickoffConfig,
) -> Result<Recovery, Error> {
    recover_escrow_into(
        transport,
        info,
        input,
        swap.script_pubkey(),
        player,
        coordinator,
        config,
    )
    .await
}

/// [`recover_escrow`] into any VTXO script: the player's own Ark address, when the player
/// recovers an escrow without the coordinator. Leftover intents can then only be deleted if
/// `coordinator` still signs; the player's recovery goes ahead without that.
pub async fn recover_escrow_into<T: ArkTransport + ?Sized>(
    transport: &T,
    info: &Info,
    input: &EscrowInput,
    script_pubkey: ScriptBuf,
    player: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
    config: &KickoffConfig,
) -> Result<Recovery, Error> {
    let outpoint = input.outpoint;
    // An earlier attempt, interrupted before its batch, may have left its intent queued. arkd
    // would refuse this one for spending the same escrow. If it cannot be deleted, registering
    // says why.
    match delete_escrow_intent(transport, input, player, coordinator).await {
        Ok(true) => {
            log::warn!("deleted a batch intent an earlier attempt left on escrow {outpoint}")
        }
        Ok(false) => {}
        Err(error) => {
            log::warn!("cannot delete any batch intent left on escrow {outpoint}: {error}")
        }
    }
    let deadline = Instant::now() + config.timeout;
    let escrow = &input.escrow;
    let paid = TxOut {
        value: input.amount,
        script_pubkey,
    };
    let cosigner = Keypair::new(&Secp256k1::new(), &mut OsRng);

    let now = unix_now()?;
    let message = IntentMessage::Register {
        // None: the swap is paid as a VTXO.
        onchain_output_indexes: Vec::new(),
        valid_at: now,
        expire_at: now + config.intent_lifetime.as_secs(),
        own_cosigner_pks: vec![cosigner.public_key()],
    };
    let encoded = message.encode()?;
    let refund_leaf = (
        escrow.script(EscrowPath::Refund).clone(),
        escrow.control_block(EscrowPath::Refund),
    );
    let vtxo_input = intent::Input::new(
        outpoint,
        // The refund leaf checks its locktime, which a final sequence would disable.
        Sequence::ENABLE_LOCKTIME_NO_RBF,
        Some(escrow.terms().refund_locktime),
        TxOut {
            value: input.amount,
            script_pubkey: escrow.script_pubkey(),
        },
        escrow.vtxo_script().scripts().to_vec(),
        refund_leaf,
        false,
        true,
        Vec::new(),
    );
    let mut intent = make_intent(
        |_, _| Ok(Vec::new()),
        |_, _| Err(ark_core::Error::ad_hoc("an escrow has no on-chain inputs")),
        vec![vtxo_input],
        vec![intent::Output::Offchain(paid.clone())],
        message,
    )?;
    sign_refund_intent(&mut intent, input, encoded, player, coordinator).await?;

    // The batch's events for this intent come by the VTXO it spends and by its cosigner key.
    let topics = vec![
        outpoint.to_string(),
        cosigner.public_key().serialize().to_lower_hex_string(),
    ];
    // Subscribe before registering, so the batch that selects the intent cannot be missed.
    let mut events = transport.event_stream(topics).await?;
    let intent_id = transport.register_intent(intent).await?;
    log::info!("registered intent {intent_id} to recover escrow {outpoint}");

    let mut signed = false;
    let result = run_batch(
        transport,
        info,
        &paid,
        &cosigner,
        &mut events,
        deadline,
        intent_id.clone(),
        &mut signed,
    )
    .await;
    // Until the tree is signed the batch cannot finish, and the intent would hold the escrow in
    // arkd's queue. If a batch has selected it, it is out of the queue and this deletes nothing.
    if let Err(error) = &result {
        if !signed {
            if let Err(delete) = delete_escrow_intent(transport, input, player, coordinator).await {
                log::warn!(
                    "intent {intent_id} may still hold escrow {outpoint}, since deleting it \
                     failed ({delete}) after its recovery failed: {error}"
                );
            }
        }
    }
    result
}

/// Sign both inputs of a refund's intent proof, as the player, through the escrow's refund leaf.
///
/// As in BIP322, input 0 spends a message-only output locked like the escrow, and input 1 the
/// escrow. The server adds its own signature.
async fn sign_refund_intent(
    intent: &mut Intent,
    input: &EscrowInput,
    message: String,
    player: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
) -> Result<(), Error> {
    let psbt = Arc::new(intent.proof.clone());
    if psbt.inputs.len() != 2 {
        return Err(Error::Protocol(
            "the intent proof has the wrong inputs".into(),
        ));
    }
    let escrow = &input.escrow;
    let leaf_hash =
        TapLeafHash::from_script(escrow.script(EscrowPath::Refund), LeafVersion::TapScript);
    let requests = (0..psbt.inputs.len())
        .map(|input_index| {
            Ok(SigningRequest {
                purpose: SigningPurpose::RefundIntent {
                    message: message.clone(),
                },
                escrow: input.outpoint,
                key: escrow.terms().player,
                psbt: psbt.clone(),
                input_index,
                leaf_hash,
                sighash: script_spend_sighash(&psbt, input_index, leaf_hash)?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    // Every request is the player's, so the coordinator's signer is asked for nothing.
    for (request, signature) in
        collect_signatures(requests, escrow.terms().coordinator, player, coordinator).await?
    {
        insert_signature(&mut intent.proof, &request, signature);
    }
    Ok(())
}

/// The batch's VTXO tree as this intent sees it, once it is being signed.
struct Tree {
    graph: TxGraph,
    commitment: Psbt,
    /// This cosigner's nonces, one per tree transaction. Each is used once.
    nonces: NonceKps,
    swap_vtxo: OutPoint,
}

/// Follow the batch from the intent's registration to its finalization. `signed` is set once
/// the tree's signatures are with the server.
#[allow(clippy::too_many_arguments)]
async fn run_batch<T: ArkTransport + ?Sized>(
    transport: &T,
    info: &Info,
    paid: &TxOut,
    cosigner: &Keypair,
    events: &mut EventStream<'_>,
    deadline: Instant,
    intent_id: String,
    signed: &mut bool,
) -> Result<Recovery, Error> {
    let intent_hash = sha256::Hash::hash(intent_id.as_bytes())
        .to_byte_array()
        .to_lower_hex_string();
    let own = cosigner.public_key();
    let own_xonly = own.x_only_public_key().0;
    // The server's key in the sweep leaf of every tree output, as ark-client takes it.
    let server = info.forfeit_pk.x_only_public_key().0;

    // The batch that selected the intent, and when its tree expires.
    let mut batch: Option<(String, Sequence)> = None;
    let mut chunks = Vec::new();
    let mut tree: Option<Tree> = None;
    // The cosigners' aggregated nonce for each tree transaction.
    let mut aggregated = HashMap::new();
    loop {
        let waiting_for = match (&batch, &tree, *signed) {
            (None, ..) => "waiting for a batch to select the recovery intent",
            (Some(_), None, _) => "waiting for the batch's VTXO tree",
            (Some(_), Some(_), false) => "waiting for the cosigners' nonces",
            (Some(_), Some(_), true) => "waiting for the batch to finalize",
        };
        let event = match timeout_at(deadline, events.next()).await {
            Err(_) => return Err(Error::Timeout(waiting_for)),
            Ok(None) => return Err(Error::Protocol("the event stream ended".into())),
            Ok(Some(event)) => event?,
        };
        let in_batch = |id: &str| batch.as_ref().is_some_and(|(batch_id, _)| batch_id == id);
        match event {
            StreamEvent::BatchStarted(event)
                if batch.is_none() && event.intent_id_hashes.contains(&intent_hash) =>
            {
                transport.confirm_registration(intent_id.clone()).await?;
                log::info!("batch {} selected recovery intent {intent_id}", event.id);
                batch = Some((event.id, event.batch_expiry));
            }
            StreamEvent::TreeTx(event)
                if in_batch(&event.id)
                    && tree.is_none()
                    && matches!(event.batch_tree_event_type, BatchTreeEventType::Vtxo) =>
            {
                chunks.push(event.tx_graph_chunk);
            }
            StreamEvent::TreeSigningStarted(event) if in_batch(&event.id) && tree.is_none() => {
                if !event.cosigners_pubkeys.contains(&own) {
                    return Err(Error::Protocol(
                        "the batch does not ask this intent's cosigner to sign its VTXO tree"
                            .into(),
                    ));
                }
                let graph = TxGraph::new(std::mem::take(&mut chunks))?;
                let commitment = event.unsigned_commitment_tx;
                // Nothing is signed for a tree that does not pay the swap.
                let swap_vtxo = paid_vtxo(&graph, &commitment, paid)?;
                let nonces = generate_nonce_tree(&mut OsRng, &graph, own, &commitment)?;
                transport
                    .submit_tree_nonces(&event.id, own, nonces.to_nonce_pks())
                    .await?;
                tree = Some(Tree {
                    graph,
                    commitment,
                    nonces,
                    swap_vtxo,
                });
            }
            StreamEvent::TreeNonces(event) if in_batch(&event.id) && !*signed => {
                let (Some((_, expiry)), Some(tree)) = (&batch, &mut tree) else {
                    continue;
                };
                // Only the transactions this cosigner signs carry its nonce.
                if !event.nonces.0.contains_key(&own_xonly) {
                    continue;
                }
                aggregated.insert(event.txid, aggregate_nonces(event.nonces));
                if aggregated.len() < tree.graph.nb_of_nodes() {
                    continue;
                }
                let txids: Vec<Txid> = tree.graph.as_map().into_keys().collect();
                let mut signatures = PartialSigTree::default();
                for txid in txids {
                    let nonce = aggregated.get(&txid).ok_or_else(|| {
                        Error::Protocol(format!(
                            "the cosigners sent no nonce for tree transaction {txid}"
                        ))
                    })?;
                    let partial = sign_batch_tree_tx(
                        txid,
                        *expiry,
                        server,
                        cosigner,
                        *nonce,
                        &tree.graph,
                        &tree.commitment,
                        &mut tree.nonces,
                    )?;
                    signatures.0.extend(partial.0);
                }
                transport
                    .submit_tree_signatures(&event.id, own, signatures)
                    .await?;
                *signed = true;
            }
            StreamEvent::BatchFinalized(event) if in_batch(&event.id) => {
                let Some(tree) = tree.as_ref().filter(|_| *signed) else {
                    return Err(Error::Protocol(format!(
                        "batch {} finalized before this intent's VTXO tree was signed",
                        event.id
                    )));
                };
                let commitment_txid = tree.commitment.unsigned_tx.compute_txid();
                if event.commitment_txid != commitment_txid {
                    return Err(Error::Protocol(format!(
                        "batch {} finalized {}, not {commitment_txid}",
                        event.id, event.commitment_txid
                    )));
                }
                return Ok(Recovery {
                    intent_id,
                    batch_id: event.id,
                    commitment_txid,
                    swap_vtxo: tree.swap_vtxo,
                });
            }
            StreamEvent::BatchFailed(event) if in_batch(&event.id) => {
                return Err(Error::BatchFailed {
                    id: event.id,
                    reason: event.reason,
                });
            }
            // A swept VTXO needs no forfeit, so finalization asks nothing of this intent.
            _ => {}
        }
    }
}

/// The VTXO paying exactly `paid` among the leaves of a tree that spends from `commitment`.
fn paid_vtxo(graph: &TxGraph, commitment: &Psbt, paid: &TxOut) -> Result<OutPoint, Error> {
    let commitment_txid = commitment.unsigned_tx.compute_txid();
    let root_inputs = &graph.root().unsigned_tx.input;
    if root_inputs.len() != 1 || root_inputs[0].previous_output.txid != commitment_txid {
        return Err(Error::Protocol(
            "the VTXO tree does not spend from the commitment transaction".into(),
        ));
    }
    let mut paying = Vec::new();
    for leaf in graph.leaves() {
        let txid = leaf.unsigned_tx.compute_txid();
        for (vout, output) in leaf.unsigned_tx.output.iter().enumerate() {
            if output == paid {
                paying.push(OutPoint::new(txid, vout as u32));
            }
        }
    }
    match paying.as_slice() {
        [vtxo] => Ok(*vtxo),
        [] => Err(Error::Protocol(
            "the batch's VTXO tree does not pay the swap, so nothing was signed".into(),
        )),
        _ => Err(Error::Protocol(
            "the batch's VTXO tree pays the swap twice, so nothing was signed".into(),
        )),
    }
}
