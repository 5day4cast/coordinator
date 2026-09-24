//! The kickoff against the scripted arkd in `common`.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::{schnorr, Message};
use bitcoin::{Amount, OutPoint, ScriptBuf, TxOut};
use coordinator_ark::{
    escrow_terms, fund_pool, BoxError, Error, EscrowInput, EscrowSigner, PoolFunding,
    SigningRequest,
};
use coordinator_ark_escrow::EntryEscrow;

use common::*;

#[tokio::test]
async fn funds_a_pool_in_one_batch() {
    let fixture = Fixture::new();
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysThePool,
    ));
    let hooks = Hooks::new(&arkd, false);

    let kickoff = fund_pool(
        arkd.as_ref(),
        &fixture.info,
        &fixture.pool,
        &fixture.player_signer(),
        &fixture.coordinator_signer(),
        &hooks,
        &Fixture::config(),
    )
    .await
    .unwrap();

    assert_eq!(kickoff.intent_id, INTENT_ID);
    assert_eq!(kickoff.batch_id, BATCH);
    assert_eq!(
        Some(kickoff.commitment_txid),
        arkd.state.lock().unwrap().commitment_txid
    );
    assert_eq!(kickoff.funding, OutPoint::new(kickoff.commitment_txid, 0));
    assert_eq!(kickoff.coordinator_fee, None);
    // The hook ran once, for the funding output, before any forfeit.
    assert_eq!(*hooks.calls.lock().unwrap(), vec![(kickoff.funding, 0)]);
    assert_eq!(arkd.forfeits().len(), 3);
}

#[tokio::test]
async fn pays_the_coordinator_fee_in_the_same_batch() {
    let mut fixture = Fixture::new();
    let fee = 1_000;
    let funding_output = TxOut {
        value: Amount::from_sat(3 * (ESCROW_SATS - fee)),
        script_pubkey: p2tr(50),
    };
    let fee_output = TxOut {
        value: Amount::from_sat(3 * fee),
        script_pubkey: p2tr(60),
    };
    fixture.pool = PoolFunding::new(
        fixture.pool.inputs().to_vec(),
        funding_output,
        &fixture.rules,
        fixture.info.dust,
    )
    .unwrap()
    .with_coordinator_fee(fee_output, fixture.info.dust)
    .unwrap();
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysThePool,
    ));
    let hooks = Hooks::new(&arkd, false);

    let kickoff = fund_pool(
        arkd.as_ref(),
        &fixture.info,
        &fixture.pool,
        &fixture.player_signer(),
        &fixture.coordinator_signer(),
        &hooks,
        &Fixture::config(),
    )
    .await
    .unwrap();

    assert_eq!(kickoff.funding, OutPoint::new(kickoff.commitment_txid, 0));
    assert_eq!(
        kickoff.coordinator_fee,
        Some(OutPoint::new(kickoff.commitment_txid, 1))
    );
    assert_eq!(arkd.forfeits().len(), 3);
}

#[tokio::test]
async fn a_batch_that_does_not_pay_the_pool_gets_no_forfeits() {
    let fixture = Fixture::new();
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysSomeoneElse,
    ));
    let hooks = Hooks::new(&arkd, false);

    let result = fund_pool(
        arkd.as_ref(),
        &fixture.info,
        &fixture.pool,
        &fixture.player_signer(),
        &fixture.coordinator_signer(),
        &hooks,
        &Fixture::config(),
    )
    .await;

    assert!(matches!(result, Err(Error::Unfunded(_))), "{result:?}");
    assert!(hooks.calls.lock().unwrap().is_empty());
    assert!(arkd.forfeits().is_empty());
}

#[tokio::test]
async fn a_failed_hook_forfeits_nothing() {
    let fixture = Fixture::new();
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysThePool,
    ));
    let hooks = Hooks::new(&arkd, true);

    let result = fund_pool(
        arkd.as_ref(),
        &fixture.info,
        &fixture.pool,
        &fixture.player_signer(),
        &fixture.coordinator_signer(),
        &hooks,
        &Fixture::config(),
    )
    .await;

    assert!(matches!(result, Err(Error::Hook(_))), "{result:?}");
    assert_eq!(hooks.calls.lock().unwrap().len(), 1);
    assert!(arkd.forfeits().is_empty());
}

/// Signs with the right key over the wrong message.
struct WrongMessageSigner(Keypair);

#[async_trait]
impl EscrowSigner for WrongMessageSigner {
    async fn sign(&self, requests: &[SigningRequest]) -> Result<Vec<schnorr::Signature>, BoxError> {
        let secp = Secp256k1::new();
        let message = Message::from_digest([7; 32]);
        Ok(requests
            .iter()
            .map(|_| secp.sign_schnorr(&message, &self.0))
            .collect())
    }
}

#[tokio::test]
async fn a_bad_signature_stops_the_kickoff_before_registration() {
    let fixture = Fixture::new();
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysThePool,
    ));
    let hooks = Hooks::new(&arkd, false);

    let result = fund_pool(
        arkd.as_ref(),
        &fixture.info,
        &fixture.pool,
        &fixture.player_signer(),
        &WrongMessageSigner(fixture.coordinator),
        &hooks,
        &Fixture::config(),
    )
    .await;

    assert!(
        matches!(result, Err(Error::BadSignature { key, .. }) if key == xonly(&fixture.coordinator)),
        "{result:?}"
    );
    assert!(arkd.state.lock().unwrap().outputs.is_none());
}

#[test]
fn pools_are_checked_before_kickoff() {
    let fixture = Fixture::new();
    let inputs = fixture.pool.inputs().to_vec();
    let output = fixture.pool.funding_output().clone();
    let dust = fixture.info.dust;
    let pool = |inputs: Vec<EscrowInput>, output: TxOut| {
        PoolFunding::new(inputs, output, &fixture.rules, dust)
    };

    assert!(pool(Vec::new(), output.clone()).is_err());

    let too_much = TxOut {
        value: output.value + Amount::from_sat(1),
        ..output.clone()
    };
    assert!(pool(inputs.clone(), too_much).is_err());

    let mut twice = inputs.clone();
    twice.push(inputs[0].clone());
    assert!(pool(twice, output.clone()).is_err());

    let mut dusty = inputs.clone();
    dusty[1].amount = Amount::from_sat(329);
    assert!(pool(dusty, output.clone()).is_err());

    let other_coordinator = escrow_terms(
        &fixture.rules,
        xonly(&keypair(4)),
        xonly(&keypair(5)),
        REFUND_AT,
        CREATED_AT,
    )
    .unwrap();
    let mut mixed = inputs.clone();
    mixed[2].escrow = EntryEscrow::new(other_coordinator).unwrap();
    assert!(pool(mixed, output.clone()).is_err());

    // The funding output may leave the rest to the server as fees.
    let smaller = TxOut {
        value: output.value - Amount::from_sat(500),
        ..output.clone()
    };
    assert!(pool(inputs.clone(), smaller.clone()).is_ok());

    // The coordinator's fee must fit in what the funding output leaves.
    let fee = |sats: u64, script_pubkey: ScriptBuf| {
        pool(inputs.clone(), smaller.clone())
            .unwrap()
            .with_coordinator_fee(
                TxOut {
                    value: Amount::from_sat(sats),
                    script_pubkey,
                },
                dust,
            )
    };
    assert!(fee(500, p2tr(60)).is_ok());
    assert!(fee(501, p2tr(60)).is_err());
    assert!(fee(329, p2tr(60)).is_err());
    assert!(fee(400, output.script_pubkey.clone()).is_err());
}
