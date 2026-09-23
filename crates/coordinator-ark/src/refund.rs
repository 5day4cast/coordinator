//! Refunding an entry escrow whose competition never kicked off.
//!
//! The escrow moves offchain to a swap VTXO, which pays the player's Lightning Address. That
//! takes two transactions, as every Arkade offchain spend does: a checkpoint transaction spends
//! the escrow through its refund leaf, and an Ark transaction spends the checkpoint output and
//! pays the swap. Keymeld signs the Ark transaction, the server co-signs it, and Keymeld then
//! signs the checkpoint.
//!
//! Nothing is withheld here. The swap receives the whole VTXO, and the swap service takes its
//! fee on the Lightning side by paying an invoice smaller than the VTXO it claims.

use ark_core::send::{
    build_offchain_transactions, sign_ark_transaction, sign_checkpoint_transaction, SendReceiver,
    VtxoInput,
};
use ark_core::ArkAddress;
use bitcoin::{Amount, OutPoint, Psbt, XOnlyPublicKey};
use coordinator_ark_escrow::{EntryEscrow, EscrowPath, RefundSwap};

use crate::Error;

/// A refund's unsigned transactions, in the order they are signed.
#[derive(Debug, Clone)]
pub struct RefundTransactions {
    /// Spends the checkpoint output and pays the swap. Signed first, then co-signed by the server.
    pub ark: Psbt,
    /// Spends the escrow. Signed once the server has co-signed the Ark transaction.
    pub checkpoint: Psbt,
}

/// Put the player's signature on a refund's Ark transaction, where the server expects it.
///
/// Keymeld signs over the digest the verifier derived, so the signature is made before this and
/// only placed here. ark-core builds the sighash again to key it; the value it computes is the
/// one Keymeld signed, because both read the same transaction.
pub fn sign_refund_ark_tx(
    ark_tx: &mut Psbt,
    player: XOnlyPublicKey,
    signature: [u8; 64],
) -> Result<(), Error> {
    let signed = schnorr(signature, player)?;
    sign_ark_transaction(|_, _| Ok(signed.clone()), ark_tx, 0)
        .map_err(|error| Error::InvalidPool(format!("cannot place the refund signature: {error}")))
}

/// The same, for the checkpoint transaction the server hands back.
pub fn sign_refund_checkpoint(
    checkpoint: &mut Psbt,
    player: XOnlyPublicKey,
    signature: [u8; 64],
) -> Result<(), Error> {
    let signed = schnorr(signature, player)?;
    sign_checkpoint_transaction(|_, _| Ok(signed.clone()), checkpoint)
        .map_err(|error| Error::InvalidPool(format!("cannot place the refund signature: {error}")))
}

fn schnorr(
    signature: [u8; 64],
    player: XOnlyPublicKey,
) -> Result<Vec<(bitcoin::secp256k1::schnorr::Signature, XOnlyPublicKey)>, Error> {
    let signature = bitcoin::secp256k1::schnorr::Signature::from_slice(&signature)
        .map_err(|error| Error::InvalidPool(format!("invalid refund signature: {error}")))?;
    Ok(vec![(signature, player)])
}

/// Build the transactions that refund `escrow`'s VTXO into `swap`.
///
/// `outpoint` and `amount` come from the escrow's VTXO on the server. The refund can only be
/// spent from the escrow's refund locktime, which fixes both transactions' locktimes. The whole
/// VTXO goes to the swap, which is the only payment a verifier will authorize.
pub fn build_refund(
    server: &ark_core::server::Info,
    escrow: &EntryEscrow,
    outpoint: OutPoint,
    amount: Amount,
    swap: &RefundSwap,
) -> Result<RefundTransactions, Error> {
    let swap = &ArkAddress::new(
        server.network,
        escrow.terms().server,
        swap.vtxo_script().output_key(),
    );
    let input = VtxoInput::new(
        escrow.script(EscrowPath::Refund).clone(),
        Some(escrow.terms().refund_locktime),
        escrow.control_block(EscrowPath::Refund),
        escrow.vtxo_script().scripts().to_vec(),
        escrow.script_pubkey(),
        amount,
        outpoint,
        Vec::new(),
    );
    // The swap takes the whole VTXO, so there is no change; the address is required regardless.
    let transactions = build_offchain_transactions(
        &[SendReceiver::bitcoin(*swap, amount)],
        swap,
        &[input],
        server,
    )
    .map_err(|error| Error::InvalidPool(format!("cannot build the refund: {error}")))?;
    let [checkpoint] = transactions.checkpoint_txs.as_slice() else {
        return Err(Error::InvalidPool(
            "a refund spends one escrow, so it has one checkpoint".into(),
        ));
    };
    Ok(RefundTransactions {
        ark: transactions.ark_tx,
        checkpoint: checkpoint.clone(),
    })
}
