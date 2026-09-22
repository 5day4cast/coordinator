//! The checkpoint output every Arkade offchain spend passes through.
//!
//! Sending a VTXO offchain takes two transactions. A checkpoint transaction spends the VTXO
//! through one of its leaves and pays a checkpoint output; an Ark transaction then spends that
//! output and pays the receivers. The owner signs both, and the server co-signs between them.
//!
//! The checkpoint output is a taproot of exactly two leaves: the leaf the VTXO was spent through,
//! so its owner can move on, and an exit script the server chooses, so a stalled checkpoint is
//! not stuck forever. Anything that authorizes such a spend must recompute this script, or it is
//! trusting whoever proposed the transaction not to redirect the money.
//!
//! This mirrors `CheckpointSpendInfo` in `ark-core`'s `send` module, and the Ark transaction
//! spends the checkpoint output through the same VTXO leaf.

use bitcoin::opcodes::all::OP_PUSHNUM_1;
use bitcoin::script::Builder;
use bitcoin::taproot::{TaprootBuilder, TaprootSpendInfo};
use bitcoin::{ScriptBuf, XOnlyPublicKey};

use crate::{Error, UNSPENDABLE_INTERNAL_KEY};

/// The ephemeral anchor every offchain transaction carries as its last output, so it can be
/// fee-bumped. Pay-to-anchor, as `ark-core` builds it.
pub fn anchor_script_pubkey() -> ScriptBuf {
    Builder::new()
        .push_opcode(OP_PUSHNUM_1)
        .push_slice([0x4e, 0x73])
        .into_script()
}

/// The checkpoint output for spending a VTXO through `spend_leaf`, on a server whose exit script
/// is `exit_script`.
pub fn checkpoint_script_pubkey(
    spend_leaf: &ScriptBuf,
    exit_script: &ScriptBuf,
) -> Result<ScriptBuf, Error> {
    let spend_info = checkpoint_spend_info(spend_leaf, exit_script)?;
    Ok(ScriptBuf::new_p2tr_tweaked(spend_info.output_key()))
}

fn checkpoint_spend_info(
    spend_leaf: &ScriptBuf,
    exit_script: &ScriptBuf,
) -> Result<TaprootSpendInfo, Error> {
    let secp = bitcoin::key::Secp256k1::verification_only();
    let internal_key = XOnlyPublicKey::from_slice(&UNSPENDABLE_INTERNAL_KEY)
        .expect("the BIP341 H point is a valid x-only key");
    // Both leaves sit at depth 1, in this order, as ark-core adds them.
    TaprootBuilder::new()
        .add_leaf(1, spend_leaf.clone())
        .and_then(|builder| builder.add_leaf(1, exit_script.clone()))
        .map_err(|error| Error::Taproot(error.to_string()))?
        .finalize(&secp, internal_key)
        .map_err(|_| Error::Taproot("the checkpoint tree cannot be finalized".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::opcodes::all::OP_CHECKSIG;

    fn leaf(byte: u8) -> ScriptBuf {
        Builder::new()
            .push_slice([byte; 32])
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    #[test]
    fn the_checkpoint_commits_to_both_leaves() {
        let (spend, exit) = (leaf(1), leaf(2));
        let checkpoint = checkpoint_script_pubkey(&spend, &exit).unwrap();
        assert!(checkpoint.is_p2tr());
        // Either leaf decides the output, so a proposed spend cannot substitute its own.
        assert_ne!(checkpoint, checkpoint_script_pubkey(&leaf(3), &exit).unwrap());
        assert_ne!(checkpoint, checkpoint_script_pubkey(&spend, &leaf(3)).unwrap());
        // BIP341 sorts siblings by hash, so the two leaves may be given in either order.
        assert_eq!(checkpoint, checkpoint_script_pubkey(&exit, &spend).unwrap());
    }

    #[test]
    fn the_anchor_is_pay_to_anchor() {
        assert_eq!(anchor_script_pubkey().to_hex_string(), "51024e73");
    }
}
