//! A pool's ticketed DLC, funded from Arkade escrows in one batch.
//!
//! The scripted arkd in `common` plays the batch.
//! The contract is signed in the batch, before any forfeit, by a local signer standing in for Keymeld.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bitcoin::{Amount, FeeRate, OutPoint, Psbt};
use coordinator_ark::{
    fund_pool, BoxError, ContractSigner, DlcKickoff, Error, LocalContractSigner, PoolFunding,
};
use dlctix::secp::Scalar;
use dlctix::{
    attestation_locking_point, hashlock, ContractParameters, ContractSignatures,
    EventLockingConditions, MarketMaker, Outcome, Player, SignedContract, TicketedDLC,
};

use common::*;

/// The fixture's keys as dlctix scalars. Each entry key is both the escrow key and the DLC key.
fn scalar(keypair: &bitcoin::key::Keypair) -> Scalar {
    Scalar::from_slice(&keypair.secret_bytes()).unwrap()
}

/// Three players, each winning one outcome, and an expiry that splits the pot evenly.
fn contract(fixture: &Fixture, expiry: bool) -> ContractParameters {
    let oracle = Scalar::from_slice(&[0x0a; 32]).unwrap().base_point_mul();
    let nonce = Scalar::from_slice(&[0x0b; 32]).unwrap().base_point_mul();
    let players: Vec<Player> = fixture
        .players
        .iter()
        .enumerate()
        .map(|(index, keypair)| Player {
            pubkey: scalar(keypair).base_point_mul(),
            ticket_hash: hashlock::sha256(&[index as u8 + 0x10; 32]),
            payout_hash: hashlock::sha256(&[index as u8 + 0x20; 32]),
        })
        .collect();
    let mut outcome_payouts: BTreeMap<Outcome, _> = (0..players.len())
        .map(|index| (Outcome::Attestation(index), BTreeMap::from([(index, 1)])))
        .collect();
    if expiry {
        outcome_payouts.insert(
            Outcome::Expiry,
            (0..players.len()).map(|index| (index, 1)).collect(),
        );
    }
    ContractParameters {
        market_maker: MarketMaker {
            pubkey: scalar(&fixture.coordinator).base_point_mul(),
        },
        event: EventLockingConditions {
            locking_points: (0..players.len())
                .map(|index| attestation_locking_point(oracle, nonce, format!("player {index}")))
                .collect(),
            expiry: expiry.then_some(3_500_000),
        },
        players,
        outcome_payouts,
        fee_rate: FeeRate::from_sat_per_vb_u32(2),
        funding_value: Amount::from_sat(3 * ESCROW_SATS),
        relative_locktime_block_delta: 144,
    }
}

fn local_signer(fixture: &Fixture) -> LocalContractSigner {
    LocalContractSigner::new(
        scalar(&fixture.coordinator),
        fixture.players.iter().map(scalar),
    )
}

/// Point the fixture's pool at the contract's funding output.
fn dlc_pool(fixture: &mut Fixture, funding: bitcoin::TxOut) {
    fixture.pool = PoolFunding::new(
        fixture.pool.inputs().to_vec(),
        funding,
        &fixture.rules,
        fixture.info.dust,
    )
    .unwrap();
}

#[tokio::test]
async fn funds_and_signs_a_pool_contract_in_one_batch() {
    let mut fixture = Fixture::new();
    let hooks = DlcKickoff::new(contract(&fixture, true), local_signer(&fixture)).unwrap();
    dlc_pool(&mut fixture, hooks.funding_output().clone());
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysThePool,
    ));

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

    let contract = hooks.signed_contract().expect("signed before the forfeits");
    assert_eq!(contract.dlc().funding_outpoint(), kickoff.funding);
    let expiry = contract.expiry_tx().expect("a signed expiry transaction");
    assert_eq!(expiry.input[0].previous_output, kickoff.funding);
    assert_eq!(expiry.input[0].witness.len(), 1, "the aggregate signature");
    assert_eq!(arkd.forfeits().len(), 3);
}

#[test]
fn a_contract_without_an_expiry_path_is_refused() {
    let fixture = Fixture::new();
    let result = DlcKickoff::new(contract(&fixture, false), local_signer(&fixture));
    assert!(matches!(result, Err(Error::InvalidPool(_))));
}

/// Signs everything but the expiry transaction.
struct NoExpirySigner(LocalContractSigner);

#[async_trait]
impl ContractSigner for NoExpirySigner {
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        commitment_tx: &Psbt,
    ) -> Result<ContractSignatures, BoxError> {
        let mut signatures = self.0.sign_contract(dlc, commitment_tx).await?;
        signatures.expiry_tx_signature = None;
        Ok(signatures)
    }
}

#[tokio::test]
async fn no_forfeits_without_a_signed_expiry_transaction() {
    let mut fixture = Fixture::new();
    let hooks = DlcKickoff::new(
        contract(&fixture, true),
        NoExpirySigner(local_signer(&fixture)),
    )
    .unwrap();
    dlc_pool(&mut fixture, hooks.funding_output().clone());
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysThePool,
    ));

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
    assert!(hooks.signed_contract().is_none());
    assert!(arkd.forfeits().is_empty());
}

/// Records what the contract signer is asked to keep, and whether any forfeit existed by then.
struct KeepingSigner {
    inner: LocalContractSigner,
    arkd: Arc<MockArkd>,
    kept: std::sync::Mutex<Vec<(OutPoint, usize)>>,
}

#[async_trait]
impl ContractSigner for KeepingSigner {
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        commitment_tx: &Psbt,
    ) -> Result<ContractSignatures, BoxError> {
        self.inner.sign_contract(dlc, commitment_tx).await
    }

    async fn keep(&self, contract: &SignedContract, commitment_tx: &Psbt) -> Result<(), BoxError> {
        let funding = contract.dlc().funding_outpoint();
        assert_eq!(funding.txid, commitment_tx.unsigned_tx.compute_txid());
        let forfeits = self.arkd.forfeits().len();
        self.kept.lock().unwrap().push((funding, forfeits));
        Ok(())
    }
}

#[tokio::test]
async fn the_contract_is_kept_before_any_forfeit() {
    let mut fixture = Fixture::new();
    let params = contract(&fixture, true);
    let funding_output = params.funding_output().unwrap();
    dlc_pool(&mut fixture, funding_output);
    let arkd = Arc::new(MockArkd::new(
        &fixture.pool,
        &fixture.info,
        Commitment::PaysThePool,
    ));
    let signer = KeepingSigner {
        inner: local_signer(&fixture),
        arkd: arkd.clone(),
        kept: Default::default(),
    };
    let hooks = DlcKickoff::new(params, signer).unwrap();

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

    assert_eq!(
        hooks_kept(&hooks),
        vec![(kickoff.funding, 0)],
        "kept once, before the forfeits"
    );
}

fn hooks_kept(hooks: &DlcKickoff<KeepingSigner>) -> Vec<(OutPoint, usize)> {
    hooks.signer().kept.lock().unwrap().clone()
}
