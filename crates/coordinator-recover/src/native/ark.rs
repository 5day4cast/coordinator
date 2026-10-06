//! Taking an escrow back through Arkade, or without it.
//!
//! - [`escrow_state`]: what arkd says about the escrow VTXO.
//! - [`refund`]: from `T` on, the refund leaf with the server's signature moves the whole escrow
//!   offchain to the player's Ark address: the same two transactions (Ark and checkpoint) the
//!   coordinator's refunds use, signed with the derived entry key instead of Keymeld. Once the
//!   VTXO has expired, the server only pays it out in a recovery batch
//!   (`coordinator_ark::recover_escrow_into`).
//! - [`unroll`]: without the server's cooperation, the escrow's virtual transactions go on chain
//!   one by one, and the unilateral refund leaf then pays the player alone.
//!
//! What is missing: an on-chain destination for a live VTXO (offboarding spends it in a batch,
//! which needs a forfeit signed through the refund leaf; refund to an Ark address and offboard
//! from any Arkade wallet instead), and the CPFP child each unrolled virtual transaction needs,
//! since they pay no fee themselves (see [`crate::fees::AnchorBumper`]). Unrolling also needs
//! arkd's indexer for the virtual transactions: the records do not carry them.

use std::time::{SystemTime, UNIX_EPOCH};

use ark_core::send::{
    build_offchain_transactions, sign_ark_transaction, sign_checkpoint_transaction, SendReceiver,
    VtxoInput,
};
use ark_core::server::VirtualTxOutPoint;
use ark_core::unilateral_exit::{finalize_unilateral_exit_tree, UnilateralExitTree};
use ark_core::{build_unilateral_exit_tree_txids, ArkAddress};
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::{schnorr, Message};
use bitcoin::{psbt, FeeRate, Psbt, ScriptBuf, Transaction, Txid, XOnlyPublicKey};
use coordinator_ark::{
    recover_escrow_into, ArkServer, ArkTransport, EscrowInput, KeypairSigner, KickoffConfig,
};
use coordinator_ark_escrow::{EscrowPath, RelativeTimelock};

use super::esplora::Esplora;
use crate::escrow::{unilateral_refund_tx, Escrow};
use crate::fees::{bump_status, AnchorBumper, BumpStatus};
use crate::inspect::{utc, EscrowState};
use crate::EntryKey;

/// The escrow's VTXO on `server`, if it lists it.
async fn listed(server: &ArkServer, escrow: &Escrow) -> Result<Option<VirtualTxOutPoint>, String> {
    let vtxos = server
        .escrow_vtxos(std::slice::from_ref(&escrow.script))
        .await
        .map_err(|e| format!("arkd will not list the escrow: {e}"))?;
    Ok(vtxos
        .into_iter()
        .find(|vtxo| vtxo.outpoint == escrow.outpoint))
}

pub async fn escrow_state(url: &str, escrow: &Escrow) -> EscrowState {
    let state = async {
        let server = ArkServer::connect(url)
            .await
            .map_err(|e| format!("cannot reach arkd at {url}: {e}"))?;
        let Some(vtxo) = listed(&server, escrow).await? else {
            return Err("arkd does not list the escrow VTXO".to_owned());
        };
        Ok(state_of(&vtxo))
    };
    state
        .await
        .unwrap_or_else(|reason| EscrowState::Unreachable { reason })
}

fn state_of(vtxo: &VirtualTxOutPoint) -> EscrowState {
    if vtxo.is_spent {
        EscrowState::Spent {
            by: vtxo.ark_txid.or(vtxo.spent_by),
        }
    } else if vtxo.is_unrolled {
        EscrowState::Unrolled
    } else if vtxo.is_swept || vtxo.expires_at <= now() as i64 {
        EscrowState::Swept
    } else {
        EscrowState::Live {
            expires_at: vtxo.expires_at,
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn signer(
    keypair: Keypair,
) -> impl Fn(
    &mut psbt::Input,
    Message,
) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
    move |_, message| {
        let signature = Secp256k1::new().sign_schnorr_no_aux_rand(&message, &keypair);
        Ok(vec![(signature, keypair.x_only_public_key().0)])
    }
}

/// Refund the escrow to the Ark address `destination`, through the refund leaf with arkd.
/// Returns what was done, line by line.
pub async fn refund(
    url: &str,
    escrow: &Escrow,
    key: &EntryKey,
    destination: &str,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let destination = ArkAddress::decode(destination).map_err(|e| {
        format!(
            "{destination} is not an Ark address ({e}). A live escrow refunds offchain: give an \
             Ark address, and offboard from that wallet"
        )
    })?;
    let server = ArkServer::connect(url)
        .await
        .map_err(|e| format!("cannot reach arkd at {url}: {e}"))?;
    if server.rules().signer != escrow.script.terms().server {
        return Err(format!(
            "arkd at {url} signs with another key than the escrow's server"
        ));
    }
    let vtxo = listed(&server, escrow)
        .await?
        .ok_or("arkd does not list the escrow VTXO")?;
    let now = now();
    if now < escrow.refund_at() {
        return Err(format!(
            "the refund leaf opens at {}",
            utc(escrow.refund_at())
        ));
    }
    match state_of(&vtxo) {
        EscrowState::Spent { by } => {
            return Err(format!(
                "the escrow is already spent{}",
                by.map(|txid| format!(" by {txid}")).unwrap_or_default()
            ))
        }
        EscrowState::Unrolled => {
            return Err("the escrow is unrolled on chain: use unroll to spend it".into())
        }
        EscrowState::Swept => {
            return recover(&server, escrow, key, &destination, dry_run).await;
        }
        EscrowState::Live { .. } | EscrowState::Unreachable { .. } => {}
    }

    let script = &escrow.script;
    let input = VtxoInput::new(
        script.script(EscrowPath::Refund).clone(),
        Some(script.terms().refund_locktime),
        script.control_block(EscrowPath::Refund),
        script.vtxo_script().scripts().to_vec(),
        script.script_pubkey(),
        vtxo.amount,
        vtxo.outpoint,
        Vec::new(),
    );
    let built = build_offchain_transactions(
        &[SendReceiver::bitcoin(destination, vtxo.amount)],
        &destination,
        &[input],
        server.info(),
    )
    .map_err(|e| format!("cannot build the refund: {e}"))?;
    let mut ark_tx = built.ark_tx;
    sign_ark_transaction(signer(key.keypair()), &mut ark_tx, 0)
        .map_err(|e| format!("cannot sign the refund: {e}"))?;
    let ark_txid = ark_tx.unsigned_tx.compute_txid();
    if dry_run {
        let mut lines = vec![format!(
            "Would refund {} to {}: Ark transaction {ark_txid}",
            vtxo.amount,
            destination.encode()
        )];
        lines.push(format!("Ark transaction (PSBT): {ark_tx}"));
        for checkpoint in &built.checkpoint_txs {
            lines.push(format!("Checkpoint (PSBT, unsigned): {checkpoint}"));
        }
        return Ok(lines);
    }
    let submitted = match server
        .client()
        .submit_offchain(ark_tx, built.checkpoint_txs)
        .await
    {
        Err(e) if e.is_vtxo_recoverable() => {
            return recover(&server, escrow, key, &destination, dry_run).await;
        }
        submitted => submitted.map_err(|e| format!("arkd refused the refund: {e}"))?,
    };
    let mut checkpoints: Vec<Psbt> = submitted.checkpoints;
    for checkpoint in &mut checkpoints {
        sign_checkpoint_transaction(signer(key.keypair()), checkpoint)
            .map_err(|e| format!("cannot sign the refund's checkpoint: {e}"))?;
    }
    server
        .client()
        .finalize_offchain(ark_txid, checkpoints)
        .await
        .map_err(|e| format!("arkd will not finalize the refund: {e}"))?;
    Ok(vec![format!(
        "Refunded {} to {} in Ark transaction {ark_txid}",
        vtxo.amount,
        destination.encode()
    )])
}

/// An expired escrow, paid to `destination` in the next Arkade batch that takes the intent.
async fn recover(
    server: &ArkServer,
    escrow: &Escrow,
    key: &EntryKey,
    destination: &ArkAddress,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    if dry_run {
        return Ok(vec![format!(
            "The escrow VTXO expired: would register a recovery intent paying {} to {} and \
             sign the batch that takes it",
            escrow.amount,
            destination.encode()
        )]);
    }
    let input = EscrowInput {
        escrow: escrow.script.clone(),
        outpoint: escrow.outpoint,
        amount: escrow.amount,
    };
    let player = KeypairSigner::new([key.keypair()]);
    // The coordinator is gone: an intent an earlier attempt left cannot be deleted, and the
    // recovery goes ahead without that.
    let no_coordinator = KeypairSigner::new(Vec::new());
    let recovered = recover_escrow_into(
        server.client(),
        server.info(),
        &input,
        destination.to_p2tr_script_pubkey(),
        &player,
        &no_coordinator,
        &KickoffConfig::for_server(server.info()),
    )
    .await
    .map_err(|e| format!("the recovery batch failed: {e}"))?;
    Ok(vec![format!(
        "Recovered {} into {} (batch {}, commitment {})",
        escrow.amount, recovered.swap_vtxo, recovered.batch_id, recovered.commitment_txid
    )])
}

/// Take the escrow on chain without the server's cooperation, then spend it alone.
#[allow(clippy::too_many_arguments)]
pub async fn unroll(
    url: Option<&str>,
    escrow: &Escrow,
    key: &EntryKey,
    esplora: &Esplora,
    destination: Option<ScriptBuf>,
    fee_rate: FeeRate,
    bumper: Option<&dyn AnchorBumper>,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    // Once the escrow output itself is on chain, only the player's leaf is left.
    if let Some(status) = esplora.tx_status(escrow.outpoint.txid).await? {
        let Some(height) = status.confirmed_height else {
            lines.push(format!(
                "The escrow output {} is in the mempool; wait for it to confirm",
                escrow.outpoint
            ));
            return Ok(lines);
        };
        let spent = esplora
            .outspend(escrow.outpoint.txid, escrow.outpoint.vout)
            .await?;
        if let Some(by) = spent.spent_by {
            lines.push(format!("The escrow output was spent by {by}"));
            return Ok(lines);
        }
        let (tip, tip_mtp) = esplora.tip().await?;
        let ready = match escrow.unilateral_refund_delay() {
            RelativeTimelock::Blocks(blocks) => {
                let opens = height + u32::from(blocks);
                if tip + 1 < opens {
                    lines.push(format!("The player's leaf opens at block {opens}"));
                }
                tip + 1 >= opens
            }
            RelativeTimelock::Seconds(seconds) => {
                // BIP68 counts from the median time past of the block before the output's.
                let start = esplora.median_time_at(height.saturating_sub(1)).await?;
                let opens = start + u64::from(seconds);
                if tip_mtp < opens {
                    lines.push(format!(
                        "The player's leaf opens once the chain's median time passes {}",
                        utc(opens)
                    ));
                }
                tip_mtp >= opens
            }
        };
        if !ready {
            return Ok(lines);
        }
        let Some(destination) = destination else {
            lines.push("The player's leaf is open: pass --to <address> to sweep it".into());
            return Ok(lines);
        };
        let tx =
            unilateral_refund_tx(escrow, key, destination, fee_rate).map_err(|e| e.to_string())?;
        if dry_run {
            lines.push(format!("Sweep: {}", serialize_hex(&tx)));
        } else {
            let txid = esplora.broadcast(&tx).await?;
            lines.push(format!("Broadcast the sweep {txid}"));
        }
        return Ok(lines);
    }

    let url =
        url.ok_or("unrolling needs arkd's indexer for the virtual transactions: pass --arkd")?;
    let branches = exit_branches(url, escrow, esplora).await?;
    for branch in branches {
        for tx in branch {
            let txid = tx.compute_txid();
            if esplora.tx_status(txid).await?.is_some() {
                lines.push(format!("Already on chain: {txid}"));
                continue;
            }
            match bump_status(&tx, None) {
                BumpStatus::Anchor {
                    outpoint,
                    value_sat,
                } => {
                    lines.push(format!(
                        "Next: {txid}, which pays no fee and needs a child spending its anchor {outpoint} ({value_sat} sat)"
                    ));
                    lines.push(format!("Transaction: {}", serialize_hex(&tx)));
                    match bumper {
                        Some(bumper) if !dry_run => {
                            let anchor_output = &tx.output[outpoint.vout as usize];
                            let child = bumper.bump(&tx, outpoint, anchor_output, fee_rate)?;
                            lines.push(format!(
                                "Child: {} (broadcast both together as a package)",
                                serialize_hex(&child)
                            ));
                        }
                        _ => lines.push(
                            "No CPFP wallet is built in yet. Spend the anchor in a child that pays \
                             for both from your own wallet, then submit the two as a package \
                             (bitcoin-cli submitpackage '[\"<transaction>\",\"<child>\"]'). Run \
                             unroll again after each confirms."
                                .into(),
                        ),
                    }
                }
                BumpStatus::Fixed { .. } => {
                    lines.push(format!("Next: {txid}: {}", serialize_hex(&tx)));
                    if !dry_run {
                        let txid = esplora.broadcast(&tx).await?;
                        lines.push(format!("Broadcast {txid}"));
                    }
                }
            }
            return Ok(lines);
        }
    }
    lines.push(format!(
        "Every virtual transaction is on chain; once {} confirms, run unroll again to sweep it",
        escrow.outpoint.txid
    ));
    Ok(lines)
}

/// The finalized virtual transactions from the batch's commitment to the escrow, in order.
async fn exit_branches(
    url: &str,
    escrow: &Escrow,
    esplora: &Esplora,
) -> Result<Vec<Vec<Transaction>>, String> {
    let server = ArkServer::connect(url)
        .await
        .map_err(|e| format!("cannot reach arkd at {url}: {e}"))?;
    let vtxo = listed(&server, escrow)
        .await?
        .ok_or("arkd does not list the escrow VTXO")?;
    let grpc = server.client().grpc();
    let chain = grpc
        .get_vtxo_chain(Some(escrow.outpoint), None, None, None)
        .await
        .map_err(|e| format!("arkd will not give the escrow's ancestry: {e}"))?;
    let paths = build_unilateral_exit_tree_txids(&chain.chains, escrow.outpoint.txid)
        .map_err(|e| e.to_string())?;
    let mut txids: Vec<Txid> = paths.concat();
    txids.sort();
    txids.dedup();
    let virtual_txs = grpc
        .get_virtual_txs(txids.iter().map(Txid::to_string).collect(), None, None)
        .await
        .map_err(|e| format!("arkd will not give the virtual transactions: {e}"))?
        .txs;
    let paths = paths
        .into_iter()
        .map(|path| {
            path.into_iter()
                .map(|txid| {
                    virtual_txs
                        .iter()
                        .find(|psbt| psbt.unsigned_tx.compute_txid() == txid)
                        .cloned()
                        .ok_or_else(|| format!("arkd did not return virtual transaction {txid}"))
                })
                .collect::<Result<Vec<Psbt>, String>>()
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut commitments = Vec::new();
    for txid in &vtxo.commitment_txids {
        commitments.push(
            esplora
                .transaction(*txid)
                .await?
                .ok_or_else(|| format!("commitment transaction {txid} is not on chain"))?,
        );
    }
    let tree = UnilateralExitTree::new(vtxo.commitment_txids.clone(), paths);
    finalize_unilateral_exit_tree(&tree, &commitments).map_err(|e| e.to_string())
}
