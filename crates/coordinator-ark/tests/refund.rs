//! The transactions this crate builds for a refund must be the ones the verifier authorizes.
//!
//! Keymeld signs a refund only if the Coordinator verifier recomputes the same transactions from
//! the player's consented policy. Building with one set of rules and checking with another is how
//! a refund silently becomes unsignable, so both run here against the same escrow.

use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Amount, OutPoint, Txid};
use coordinator_ark::{build_refund, escrow_terms, server_rules, testing};
use coordinator_ark_escrow::{EntryEscrow, RefundSwap, RelativeTimelock, SwapTerms};
use coordinator_escrow::ark::{psbt_hex, ArkEscrowSpend, RefundPurpose};
use coordinator_escrow::authorization::ArkEscrowPolicy;

const VALUE: Amount = Amount::from_sat(50_000);
const REFUND_AT: u32 = 1_790_000_000;

fn keypair(byte: u8) -> Keypair {
    Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_slice(&[byte; 32]).unwrap(),
    )
}

fn escrow(info: &ark_core::server::Info) -> EntryEscrow {
    let rules = server_rules(info).unwrap();
    let terms = escrow_terms(
        &rules,
        keypair(14).x_only_public_key().0,
        keypair(18).x_only_public_key().0,
        REFUND_AT,
        REFUND_AT - 86_400,
    )
    .unwrap();
    EntryEscrow::new(terms).unwrap()
}

/// The swap the refund pays.
fn swap(info: &ark_core::server::Info) -> RefundSwap {
    let deadline = bitcoin::absolute::LockTime::from_consensus(REFUND_AT + 3_600);
    let exit_delay = RelativeTimelock::Seconds(2048);
    RefundSwap::new(SwapTerms {
        player: keypair(14).x_only_public_key().0,
        swapper: keypair(30).x_only_public_key().0,
        server: server_rules(info).unwrap().signer,
        payment_hash: [5u8; 32],
        deadline,
        exit_delay,
        unilateral_reclaim_delay: SwapTerms::unilateral_reclaim_delay_for(
            deadline,
            exit_delay,
            REFUND_AT,
        )
        .unwrap(),
    })
    .unwrap()
}

fn policy(escrow: &EntryEscrow, info: &ark_core::server::Info) -> ArkEscrowPolicy {
    ArkEscrowPolicy {
        escrow_tap_tree: hex::encode(escrow.vtxo_script().encode_tap_tree()),
        max_fee_sats: 500,
        max_refund_fee_sats: 100,
        checkpoint_exit_script: hex::encode(info.checkpoint_tapscript.as_bytes()),
    }
}

#[test]
fn the_verifier_authorizes_the_refund_this_crate_builds() {
    let server = keypair(21);
    let info = testing::mock_info(&server);
    let escrow = escrow(&info);
    let swap = swap(&info);

    let refund = build_refund(
        &info,
        &escrow,
        OutPoint::new(Txid::from_raw_hash(bitcoin::hashes::Hash::all_zeros()), 0),
        VALUE,
        &swap,
    )
    .unwrap();

    // Both transactions wait for the escrow's refund locktime, and neither is final.
    for psbt in [&refund.ark, &refund.checkpoint] {
        assert_eq!(
            psbt.unsigned_tx.lock_time,
            bitcoin::absolute::LockTime::from_consensus(REFUND_AT)
        );
        assert!(!psbt.unsigned_tx.input[0].sequence.is_final());
    }

    let spend = |purpose| ArkEscrowSpend::Refund {
        purpose,
        ark_psbt: psbt_hex(&refund.ark),
        checkpoint_psbt: psbt_hex(&refund.checkpoint),
        swap_tap_tree: hex::encode(swap.vtxo_script().encode_tap_tree()),
    };
    let policy = policy(&escrow, &info);
    let signed = |purpose| {
        coordinator_escrow::ark::refund_from(&escrow, &policy, &spend(purpose))
            .map(|(_, refund)| refund)
    };

    let ark = signed(RefundPurpose::ArkTransaction).expect("the Ark transaction is authorized");
    let checkpoint = signed(RefundPurpose::Checkpoint).expect("the checkpoint is authorized");
    assert_eq!(ark.value_sats, VALUE.to_sat());
    assert_eq!(checkpoint.value_sats, VALUE.to_sat());
    assert_ne!(ark.digest, checkpoint.digest);
}

#[test]
fn a_refund_to_another_swap_is_not_authorized() {
    let server = keypair(21);
    let info = testing::mock_info(&server);
    let escrow = escrow(&info);
    let swap = swap(&info);
    // Built to pay a swap for another invoice, which this one cannot be claimed with.
    let elsewhere = RefundSwap::new(SwapTerms {
        payment_hash: [6u8; 32],
        ..*swap.terms()
    })
    .unwrap();
    let refund = build_refund(
        &info,
        &escrow,
        OutPoint::new(Txid::from_raw_hash(bitcoin::hashes::Hash::all_zeros()), 0),
        VALUE,
        &elsewhere,
    )
    .unwrap();
    let spend = ArkEscrowSpend::Refund {
        purpose: RefundPurpose::ArkTransaction,
        ark_psbt: psbt_hex(&refund.ark),
        checkpoint_psbt: psbt_hex(&refund.checkpoint),
        swap_tap_tree: hex::encode(swap.vtxo_script().encode_tap_tree()),
    };
    assert!(coordinator_escrow::ark::refund_from(&escrow, &policy(&escrow, &info), &spend).is_err());
}
