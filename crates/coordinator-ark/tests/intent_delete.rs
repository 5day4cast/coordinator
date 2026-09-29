//! The delete proofs this crate builds are the ones the verifier authorizes, and arkd takes.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::schnorr;
use coordinator_ark::{
    delete_escrow_intent, delete_pool_intent, BoxError, EscrowSigner, KeypairSigner,
    SigningPurpose, SigningRequest,
};
use coordinator_escrow::ark::{
    check_intent_delete_fresh, intent_delete_digests, psbt_hex, ArkEscrowSpend,
};

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

#[tokio::test]
async fn every_player_signs_only_what_the_verifier_derives_for_a_delete() {
    let fixture = Fixture::new();
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysThePool,
    ));
    let players = Recording::new(fixture.player_signer());

    let deleted = delete_pool_intent(
        arkd.as_ref(),
        &fixture.pool,
        &players,
        &fixture.coordinator_signer(),
    )
    .await
    .unwrap();
    assert!(!deleted, "nothing was queued");
    assert_eq!(arkd.state.lock().unwrap().delete_proofs, 1);

    let requests = players.requests.lock().unwrap().clone();
    // The message input, locked like the first escrow, and each escrow.
    assert_eq!(requests.len(), fixture.pool.inputs().len() + 1);
    for input in fixture.pool.inputs() {
        let theirs = requests
            .iter()
            .filter(|request| request.key == input.escrow.terms().player)
            .collect::<Vec<_>>();
        let SigningPurpose::DeleteIntent { message } = &theirs[0].purpose else {
            panic!("a delete proof is signed as one");
        };
        let spend = ArkEscrowSpend::DeleteIntent {
            proof_psbt: psbt_hex(&theirs[0].psbt),
            message: message.clone(),
        };
        check_intent_delete_fresh(&spend, now()).unwrap();
        let digests = intent_delete_digests(&input.escrow, &spend).unwrap();
        assert_eq!(digests.len(), theirs.len());
        for request in theirs {
            assert!(digests.contains(&(request.input_index, request.sighash.to_byte_array())));
        }
    }
}

#[tokio::test]
async fn one_escrow_deletes_the_whole_intent_holding_it() {
    let fixture = Fixture::new();
    let arkd = Arc::new(MockArkd::offchain(&fixture.info));
    let signers = fixture
        .pool
        .inputs()
        .iter()
        .map(|input| {
            let terms = input.escrow.terms();
            (input.outpoint, [terms.player, terms.coordinator])
        })
        .collect();
    arkd.queue_intent("intent-41", signers);
    let escrow = &fixture.pool.inputs()[1];
    let player = Recording::new(KeypairSigner::new([fixture.players[1]]));

    let deleted = delete_escrow_intent(
        arkd.as_ref(),
        escrow,
        &player,
        &fixture.coordinator_signer(),
    )
    .await
    .unwrap();

    assert!(deleted);
    assert!(arkd.queued().is_empty());
    assert_eq!(arkd.state.lock().unwrap().deleted, vec!["intent-41"]);
    // Only this player signed: the message input and their own escrow.
    {
        let requests = player.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.input_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(requests
            .iter()
            .all(|request| request.escrow == escrow.outpoint));
    }

    // Nothing is left to delete.
    let again = delete_escrow_intent(
        arkd.as_ref(),
        escrow,
        &player,
        &fixture.coordinator_signer(),
    )
    .await
    .unwrap();
    assert!(!again);
}
