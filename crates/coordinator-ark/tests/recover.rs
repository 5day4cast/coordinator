//! Recovering an expired escrow against the scripted arkd in `common`: the batch that gives its
//! value back pays the refund's swap, and the player signs only what the verifier authorizes.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::schnorr;
use coordinator_ark::{
    address_hrp, recover_escrow, BoxError, Error, EscrowInput, EscrowSigner, KeypairSigner,
    KickoffConfig, SigningPurpose, SigningRequest,
};
use coordinator_ark_escrow::{RefundSwap, RelativeTimelock, SwapTerms};
use coordinator_escrow::ark::{check_refund_intent_fresh, psbt_hex, refund_from, ArkEscrowSpend};
use coordinator_escrow::authorization::ArkEscrowPolicy;

use common::*;

/// Signs with its keys, and keeps what it was asked, as a verifier would see it.
struct Recording {
    keys: KeypairSigner,
    requests: Mutex<Vec<SigningRequest>>,
}

impl Recording {
    fn new(keys: KeypairSigner) -> Self {
        Self {
            keys,
            requests: Mutex::default(),
        }
    }

    /// The requests to sign a refund's intent proof, and the message they prove.
    fn refund_intent(&self) -> (Vec<SigningRequest>, String) {
        let requests = self.requests.lock().unwrap();
        let signed: Vec<SigningRequest> = requests
            .iter()
            .filter(|request| matches!(request.purpose, SigningPurpose::RefundIntent { .. }))
            .cloned()
            .collect();
        let SigningPurpose::RefundIntent { message } = &signed[0].purpose else {
            unreachable!()
        };
        (signed.clone(), message.clone())
    }
}

#[async_trait]
impl EscrowSigner for Recording {
    async fn sign(&self, requests: &[SigningRequest]) -> Result<Vec<schnorr::Signature>, BoxError> {
        self.requests.lock().unwrap().extend_from_slice(requests);
        self.keys.sign(requests).await
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The swap the refund of `fixture`'s first escrow pays.
fn swap(fixture: &Fixture) -> RefundSwap {
    let deadline = bitcoin::absolute::LockTime::from_consensus(now() as u32 + 3_600);
    let exit_delay = RelativeTimelock::Seconds(2048);
    RefundSwap::new(SwapTerms {
        player: xonly(&fixture.players[0]),
        swapper: xonly(&keypair(30)),
        server: fixture.rules.signer,
        payment_hash: [5u8; 32],
        deadline,
        exit_delay,
        unilateral_reclaim_delay: SwapTerms::unilateral_reclaim_delay_for(
            deadline,
            exit_delay,
            now() as u32,
        )
        .unwrap(),
    })
    .unwrap()
}

/// A server that lists `fixture`'s first escrow as a VTXO it swept a minute ago.
fn arkd_with_swept_escrow(fixture: &Fixture) -> (MockArkd, EscrowInput) {
    let input = fixture.pool.inputs()[0].clone();
    let arkd = MockArkd::offchain(&fixture.info);
    let address = input
        .escrow
        .address(address_hrp(fixture.info.network))
        .unwrap()
        .encode();
    let created_at = now() as i64 - 7 * 24 * 60 * 60;
    arkd.add_vtxo(&address, input.outpoint, input.amount, created_at, false);
    arkd.expire_vtxo(input.outpoint, now() as i64 - 60, true);
    (arkd, input)
}

fn player(fixture: &Fixture) -> Recording {
    Recording::new(KeypairSigner::new([fixture.players[0]]))
}

#[tokio::test]
async fn a_swept_escrow_is_recovered_into_its_swap_in_one_batch() {
    let fixture = Fixture::new();
    let (arkd, input) = arkd_with_swept_escrow(&fixture);
    let swap = swap(&fixture);
    let player = player(&fixture);

    let recovery = recover_escrow(
        &arkd,
        &fixture.info,
        &input,
        &swap,
        &player,
        &fixture.coordinator_signer(),
        &Fixture::config(),
    )
    .await
    .unwrap();

    assert_eq!(recovery.intent_id, INTENT_ID);
    assert_eq!(recovery.batch_id, BATCH);
    {
        let state = arkd.state.lock().unwrap();
        assert_eq!(Some(recovery.commitment_txid), state.commitment_txid);
        assert_eq!(state.recovered, vec![(input.outpoint, recovery.swap_vtxo)]);
        // Nothing was queued before, and the batch took the intent.
        assert_eq!(state.delete_proofs, 1);
        assert!(state.queued.is_empty() && state.deleted.is_empty());
        // The server lists the escrow as settled, and the swap's new VTXO with its whole value.
        let escrow = state
            .vtxos
            .iter()
            .find(|vtxo| vtxo.outpoint == input.outpoint)
            .unwrap();
        assert!(escrow.is_spent);
        assert_eq!(escrow.settled_by, Some(recovery.commitment_txid));
        let paid = state
            .vtxos
            .iter()
            .find(|vtxo| vtxo.outpoint == recovery.swap_vtxo)
            .unwrap();
        assert_eq!(paid.script, swap.script_pubkey());
        assert_eq!(paid.amount, input.amount);
        assert!(!paid.is_spent);
    }

    // The player signed the proof's two inputs, and exactly what the verifier derives for a
    // refund of this escrow into this swap.
    let (signed, message) = player.refund_intent();
    assert_eq!(
        signed
            .iter()
            .map(|request| request.input_index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    let spend = ArkEscrowSpend::RefundIntent {
        proof_psbt: psbt_hex(&signed[0].psbt),
        message,
        swap_tap_tree: hex::encode(swap.vtxo_script().encode_tap_tree()),
    };
    check_refund_intent_fresh(&input.escrow, &spend, now()).unwrap();
    let policy = ArkEscrowPolicy {
        escrow_tap_tree: hex::encode(input.escrow.vtxo_script().encode_tap_tree()),
        max_fee_sats: 500,
        max_refund_fee_sats: 100,
        checkpoint_exit_script: hex::encode(fixture.info.checkpoint_tapscript.as_bytes()),
    };
    let (_, authorized) = refund_from(&input.escrow, &policy, &spend).unwrap();
    assert_eq!(authorized.value_sats, input.amount.to_sat());
    assert_eq!(
        authorized.digests,
        signed
            .iter()
            .map(|request| (request.input_index, request.sighash.to_byte_array()))
            .collect::<Vec<_>>()
    );
    // The proof waits for the escrow's refund locktime, and cannot serve before it.
    assert_eq!(
        signed[0].psbt.unsigned_tx.lock_time,
        input.escrow.terms().refund_locktime
    );
}

#[tokio::test]
async fn a_tree_that_does_not_pay_the_swap_is_not_signed() {
    let fixture = Fixture::new();
    let (mut arkd, input) = arkd_with_swept_escrow(&fixture);
    arkd.commitment = Commitment::PaysSomeoneElse;

    let result = recover_escrow(
        &arkd,
        &fixture.info,
        &input,
        &swap(&fixture),
        &player(&fixture),
        &fixture.coordinator_signer(),
        &Fixture::config(),
    )
    .await;

    assert!(
        matches!(&result, Err(Error::Protocol(reason)) if reason.contains("does not pay the swap")),
        "{result:?}"
    );
    let state = arkd.state.lock().unwrap();
    assert!(state.recovered.is_empty());
    assert!(state.vtxos.iter().all(|vtxo| !vtxo.is_spent));
}

#[tokio::test]
async fn a_recovery_no_batch_selects_deletes_its_intent() {
    let fixture = Fixture::new();
    let (arkd, input) = arkd_with_swept_escrow(&fixture);
    let arkd = arkd.never_selecting();

    let result = recover_escrow(
        &arkd,
        &fixture.info,
        &input,
        &swap(&fixture),
        &player(&fixture),
        &fixture.coordinator_signer(),
        &KickoffConfig {
            intent_lifetime: Duration::from_secs(120),
            timeout: Duration::from_millis(200),
        },
    )
    .await;

    assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    // Left queued, the intent would hold the escrow against the next attempt.
    assert!(arkd.queued().is_empty());
    let state = arkd.state.lock().unwrap();
    assert_eq!(state.deleted, vec![INTENT_ID]);
    assert!(state.recovered.is_empty());
}

#[tokio::test]
async fn a_recovery_first_deletes_an_intent_an_earlier_attempt_left() {
    let fixture = Fixture::new();
    let (arkd, input) = arkd_with_swept_escrow(&fixture);
    let terms = input.escrow.terms();
    arkd.queue_intent(
        "intent-41",
        HashMap::from([(input.outpoint, [terms.player, terms.coordinator])]),
    );
    let arkd = Arc::new(arkd);

    let recovery = recover_escrow(
        arkd.as_ref(),
        &fixture.info,
        &input,
        &swap(&fixture),
        &player(&fixture),
        &fixture.coordinator_signer(),
        &Fixture::config(),
    )
    .await
    .unwrap();

    let state = arkd.state.lock().unwrap();
    assert_eq!(state.deleted, vec!["intent-41"]);
    assert_eq!(state.recovered, vec![(input.outpoint, recovery.swap_vtxo)]);
}
