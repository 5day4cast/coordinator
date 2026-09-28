use super::*;
use crate::{
    config::KeymeldSettings,
    infra::{
        bitcoin::{PayoutOutputStatus, SendOptions, WalletBalance, WalletUtxo, ECONOMY_FEE_TARGET},
        db::{DBConnection, DatabasePoolConfig, DatabaseType},
        keymeld::KeymeldService,
        lightning_mock::MockLnClient,
        lnurl_mock::MockLnurlPay,
        oracle_mock::MockOracle,
    },
};
use dlctix::{EventLockingConditions, MarketMaker, SignedContract};
use std::sync::Mutex;

fn signed_contract() -> (SignedContract, Scalar, [Scalar; 3]) {
    signed_contract_funded(Amount::from_sat(100_000))
}

/// A contract between the market maker and three players who share the one outcome's payout.
fn signed_contract_funded(funding_value: Amount) -> (SignedContract, Scalar, [Scalar; 3]) {
    let market_maker = Scalar::from_slice(&[7; 32]).unwrap();
    let players = [1, 3, 5].map(|key| Scalar::from_slice(&[key; 32]).unwrap());
    let params = ContractParameters {
        market_maker: MarketMaker {
            pubkey: market_maker.base_point_mul(),
        },
        players: players
            .iter()
            .enumerate()
            .map(|(index, key)| Player {
                pubkey: key.base_point_mul(),
                ticket_hash: dlctix::hashlock::sha256(&[index as u8 + 10; 32]),
                payout_hash: dlctix::hashlock::sha256(&[index as u8 + 20; 32]),
            })
            .collect(),
        event: EventLockingConditions {
            locking_points: vec![Scalar::from_slice(&[10; 32])
                .unwrap()
                .base_point_mul()
                .into()],
            expiry: None,
        },
        outcome_payouts: BTreeMap::from([(
            Outcome::Attestation(0),
            PayoutWeights::from([(0, 1), (1, 1), (2, 1)]),
        )]),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value,
        relative_locktime_block_delta: 72,
    };
    let dlc = TicketedDLC::new(params, OutPoint::null()).unwrap();
    let mut rng = ChaCha20Rng::from_seed([42; 32]);
    let mut sessions: BTreeMap<_, _> = std::iter::once(market_maker)
        .chain(players)
        .map(|key| {
            (
                key.base_point_mul(),
                SigningSession::new(dlc.clone(), &mut rng, key).unwrap(),
            )
        })
        .collect();
    let nonces = sessions
        .iter()
        .map(|(key, session)| (*key, session.our_public_nonces().clone()))
        .collect();
    let coordinator = sessions
        .remove(&market_maker.base_point_mul())
        .unwrap()
        .aggregate_nonces_and_compute_partial_signatures(nonces)
        .unwrap();
    let signatures = sessions
        .into_iter()
        .map(|(key, session)| {
            let contributor = session
                .compute_partial_signatures(coordinator.aggregated_nonces().clone())
                .unwrap();
            (key, contributor.our_partial_signatures().clone())
        })
        .collect();
    (
        coordinator.aggregate_all_signatures(signatures).unwrap(),
        market_maker,
        players,
    )
}

#[test]
fn split_reclaims_sign_the_sweep_input_for_every_parent_output() {
    let (contract, market_maker, players) = signed_contract();
    let mut spent_vouts = std::collections::BTreeSet::new();
    for player_index in 0..players.len() {
        let win_condition = WinCondition {
            outcome: Outcome::Attestation(0),
            player_index,
        };
        let (input, prevout) = contract
            .split_reclaim_tx_input_and_prevout(&win_condition)
            .unwrap();
        let (mut transaction, input_index) = simple_sweep_tx(
            contract.params().market_maker.pubkey,
            input.clone(),
            contract.split_reclaim_tx_input_weight(),
            prevout.value,
            FeeRate::from_sat_per_vb_u32(1),
        )
        .unwrap();
        let vout = input.previous_output.vout;
        spent_vouts.insert(vout);

        // This was the deployed failure for the second and third split outputs:
        // their parent vout is not a position in this single-input transaction.
        if vout > 0 {
            let error = contract
                .sign_split_reclaim_tx_input(
                    &win_condition,
                    &mut transaction.clone(),
                    vout as usize,
                    &Prevouts::All(std::slice::from_ref(prevout)),
                    market_maker,
                )
                .unwrap_err();
            assert!(error.to_string().contains("input index out of bounds"));
        }
        contract
            .sign_split_reclaim_tx_input(
                &win_condition,
                &mut transaction,
                input_index,
                &Prevouts::All(&[prevout]),
                market_maker,
            )
            .unwrap();

        assert_eq!(transaction.input.len(), 1);
        assert_eq!(transaction.input[0].previous_output, input.previous_output);
        assert_eq!(transaction.input[0].sequence, input.sequence);
        assert!(!transaction.input[0].witness.is_empty());
    }
    assert_eq!(spent_vouts, [0, 1, 2].into_iter().collect());
}

#[test]
fn split_closes_sign_the_sweep_input_for_every_parent_output() {
    let (contract, market_maker, players) = signed_contract();
    let mut spent_vouts = std::collections::BTreeSet::new();
    for (player_index, player) in players.into_iter().enumerate() {
        let win_condition = WinCondition {
            outcome: Outcome::Attestation(0),
            player_index,
        };
        let (input, prevout) = contract
            .split_close_tx_input_and_prevout(&win_condition)
            .unwrap();
        let (mut transaction, input_index) = simple_sweep_tx(
            contract.params().market_maker.pubkey,
            input.clone(),
            contract.close_tx_input_weight(),
            prevout.value,
            FeeRate::from_sat_per_vb_u32(1),
        )
        .unwrap();
        spent_vouts.insert(input.previous_output.vout);
        contract
            .sign_split_close_tx_input(
                &win_condition,
                &mut transaction,
                input_index,
                &Prevouts::All(&[prevout]),
                market_maker,
                player,
            )
            .unwrap();

        assert_eq!(transaction.input.len(), 1);
        assert_eq!(transaction.input[0].previous_output, input.previous_output);
        assert!(!transaction.input[0].witness.is_empty());
    }
    assert_eq!(spent_vouts, [0, 1, 2].into_iter().collect());
}

#[test]
fn unified_close_signs_the_sweep_input_with_all_winners() {
    let (contract, market_maker, players) = signed_contract();
    let outcome = Outcome::Attestation(0);
    let (input, prevout) = contract
        .outcome_close_tx_input_and_prevout(&outcome)
        .unwrap();
    let (mut transaction, input_index) = simple_sweep_tx(
        contract.params().market_maker.pubkey,
        input.clone(),
        contract.close_tx_input_weight(),
        prevout.value,
        FeeRate::from_sat_per_vb_u32(1),
    )
    .unwrap();
    let winner_keys = players
        .into_iter()
        .map(|key| (key.base_point_mul(), key))
        .collect();
    contract
        .sign_outcome_close_tx_input(
            &outcome,
            &mut transaction,
            input_index,
            &Prevouts::All(&[prevout]),
            market_maker,
            &winner_keys,
        )
        .unwrap();

    assert_eq!(transaction.input.len(), 1);
    assert_eq!(transaction.input[0].previous_output, input.previous_output);
    assert!(!transaction.input[0].witness.is_empty());
}

/// A sweep whose fee would leave less than the dust limit is refused instead of built: the
/// network rejects the dust output, and a fee above the output's value used to panic.
#[test]
fn sweeps_that_would_leave_dust_are_refused() {
    let (contract, _, _) = signed_contract();
    let win_condition = WinCondition {
        outcome: Outcome::Attestation(0),
        player_index: 0,
    };
    let (input, _) = contract
        .split_reclaim_tx_input_and_prevout(&win_condition)
        .unwrap();
    let sweep = |value: Amount, fee_rate: FeeRate| {
        simple_sweep_tx(
            contract.params().market_maker.pubkey,
            input.clone(),
            contract.split_reclaim_tx_input_weight(),
            value,
            fee_rate,
        )
    };
    let fee_rate = FeeRate::from_sat_per_vb_u32(2);
    let (transaction, _) = sweep(Amount::from_sat(100_000), fee_rate).unwrap();
    let fee = Amount::from_sat(100_000) - transaction.output[0].value;
    let dust_limit = Amount::from_sat(330);
    assert_eq!(
        transaction.output[0].script_pubkey.minimal_non_dust(),
        dust_limit
    );

    // Leaving exactly the dust limit is a valid sweep.
    let (transaction, _) = sweep(fee + dust_limit, fee_rate).unwrap();
    assert_eq!(transaction.output[0].value, dust_limit);

    // Less than that, down to less than the fee itself, is refused. It is refused for good only
    // when LND's fee rate floor would leave dust too; otherwise a sweep may succeed later.
    let floor_fee = FeeRate::from_sat_per_kwu(253)
        .checked_mul_by_weight(predict_weight(
            [contract.split_reclaim_tx_input_weight()],
            [transaction.output[0].script_pubkey.len()],
        ))
        .unwrap();
    for (value, permanent) in [
        (fee + dust_limit - Amount::ONE_SAT, false),
        (floor_fee + dust_limit, false),
        (floor_fee + dust_limit - Amount::ONE_SAT, true),
        (fee, true),
        (fee - Amount::ONE_SAT, true),
        (Amount::ZERO, true),
    ] {
        assert_eq!(
            sweep(value, fee_rate).unwrap_err(),
            UneconomicSweep {
                value,
                fee,
                fee_rate,
                dust_limit,
                permanent,
            }
        );
    }

    // At LND's 253 sat/kWU floor the same output is worth sweeping: rounding the floor up to
    // 2 sat/vB is what turned it into dust.
    let (transaction, _) = sweep(
        fee + dust_limit - Amount::ONE_SAT,
        FeeRate::from_sat_per_kwu(253),
    )
    .unwrap();
    assert!(transaction.output[0].value > dust_limit);

    // A fee too large to count is refused too.
    assert_eq!(
        sweep(Amount::from_sat(100_000), FeeRate::MAX)
            .unwrap_err()
            .fee,
        Amount::MAX
    );
}

mockall::mock! {
    Chain {}
    #[async_trait::async_trait]
    impl Bitcoin for Chain {
        fn get_network(&self) -> bitcoin::Network;
        async fn sign_psbt_with_escrow_support(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error>;
        async fn finalize_psbt_with_escrow_support(
            &self,
            psbt: &mut Psbt,
        ) -> Result<bool, anyhow::Error>;
        async fn build_psbt(
            &self,
            script_pubkey: ScriptBuf,
            amount: Amount,
            fee_rate: FeeRate,
            selected_utxos: Vec<OutPoint>,
            foreign_utxos: Vec<ForeignUtxo>,
        ) -> Result<Psbt, anyhow::Error>;
        async fn reserve_psbt_inputs_until(
            &self,
            psbt: &Psbt,
            deadline: u64,
        ) -> Result<(), anyhow::Error>;
        async fn release_psbt_inputs(&self, psbt: &Psbt) -> Result<(), anyhow::Error>;
        async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<WalletUtxo, anyhow::Error>;
        async fn get_current_height(&self) -> Result<u32, anyhow::Error>;
        async fn get_confirmed_blockchain_time(&self, blocks: usize) -> Result<u64, anyhow::Error>;
        async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error>;
        async fn estimate_fee(&self, conf_target: u16) -> Result<f64, anyhow::Error>;
        async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error>;
        async fn payout_output_status(
            &self,
            outpoint: OutPoint,
            output: TxOut,
        ) -> Result<PayoutOutputStatus, anyhow::Error>;
        async fn broadcast(&self, transaction: &Transaction) -> Result<(), anyhow::Error>;
        async fn get_next_address(&self) -> Result<bitcoin::Address, anyhow::Error>;
        async fn get_public_key(&self) -> Result<BitcoinPublicKey, anyhow::Error>;
        async fn get_derived_private_key(&self) -> Result<Scalar, anyhow::Error>;
        async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error>;
        async fn sign_psbt(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error>;
        async fn list_utxos(&self) -> Vec<WalletUtxo>;
        async fn sync(&self) -> Result<(), anyhow::Error>;
        async fn get_balance(&self) -> Result<WalletBalance, anyhow::Error>;
        async fn get_outputs(&self) -> Result<Vec<WalletUtxo>, anyhow::Error>;
        async fn send_to_address(
            &self,
            send_options: SendOptions,
            selected_utxos: Vec<OutPoint>,
        ) -> Result<Txid, anyhow::Error>;
    }
}

/// LND's floor of 253 sat/kWU, in sat/vB as the client reads LND's estimates.
const LND_FLOOR_SAT_PER_VB: f64 = 253.0 * 4.0 / 1_000.0;

/// A competition past its split outputs' reclaim delay whose three winners were never paid.
struct UnpaidWinners {
    _directory: tempfile::TempDir,
    database: DBConnection,
    coordinator: Coordinator,
    competition: Competition,
    contract: SignedContract,
    broadcasts: Arc<Mutex<Vec<Transaction>>>,
}

impl UnpaidWinners {
    async fn new(funding_value: Amount, fee_rates: HashMap<u16, f64>) -> Self {
        let (contract, market_maker, players) = signed_contract_funded(funding_value);
        let broadcasts = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MockChain::new();
        chain
            .expect_get_derived_private_key()
            .returning(move || Ok(market_maker));
        chain.expect_get_current_height().returning(|| Ok(1_000));
        // The outcome and split transactions confirmed long ago.
        chain
            .expect_get_tx_confirmation_height()
            .returning(|_| Ok(Some(100)));
        chain
            .expect_get_estimated_fee_rates()
            .returning(move || Ok(fee_rates.clone()));
        let record = broadcasts.clone();
        chain.expect_broadcast().returning(move |transaction| {
            record.lock().unwrap().push(transaction.clone());
            Ok(())
        });

        let directory = tempfile::tempdir().unwrap();
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database.clone()),
            Arc::new(chain),
            Arc::new(MockLnClient::new()),
            Arc::new(MockLnurlPay::new(bitcoin::Network::Regtest)),
            Arc::new(
                KeymeldService::new(KeymeldSettings::default(), Uuid::now_v7(), &[1; 32]).unwrap(),
            ),
            None,
            72,
            1,
            "settlement-sweep-test".into(),
            false,
            1,
        )
        .await
        .unwrap();

        let now = OffsetDateTime::now_utc();
        let mut competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: now - time::Duration::hours(5),
            start_observation_date: now - time::Duration::hours(7),
            end_observation_date: now - time::Duration::hours(6),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 3,
            total_allowed_entries: 3,
            entry_fee: 1_000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
            total_competition_pool: funding_value.to_sat() as usize,
            relative_locktime_block_delta: Some(72),
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
        });
        coordinator
            .competition_store
            .add_competition_with_tickets(competition.clone(), vec![])
            .await
            .unwrap();
        let event_id = competition.id;
        for player in players {
            let (entry_id, ticket_id) = (Uuid::now_v7(), Uuid::now_v7());
            let submission = serde_json::to_string(&AddEventEntry {
                id: entry_id,
                event_id,
                expected_observations: vec![],
            })
            .unwrap();
            let ephemeral_pubkey = hex::encode(player.base_point_mul().serialize());
            database
                .execute_write(move |pool| async move {
                    sqlx::query(
                        "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, paid_at)
                         VALUES (?, ?, 'unused', ?, datetime('now'))",
                    )
                    .bind(ticket_id.to_string())
                    .bind(event_id.to_string())
                    .bind(format!("ticket-{ticket_id}"))
                    .execute(&pool)
                    .await?;
                    sqlx::query(
                        "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey,
                             payout_hash, entry_submission)
                         VALUES (?, ?, ?, 'player', ?, ?, ?)",
                    )
                    .bind(entry_id.to_string())
                    .bind(event_id.to_string())
                    .bind(ticket_id.to_string())
                    .bind(ephemeral_pubkey)
                    .bind(format!("payout-{entry_id}"))
                    .bind(submission)
                    .execute(&pool)
                    .await?;
                    Ok(())
                })
                .await
                .unwrap();
        }

        competition.event_announcement = Some(contract.params().event.clone());
        competition.attestation = Some(Scalar::from_slice(&[10; 32]).unwrap().into());
        competition.signed_contract = Some(contract.clone());
        competition.outcome_transaction = Some(Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![],
        });
        competition.outcome_broadcasted_at = Some(now - time::Duration::hours(4));
        competition.delta_broadcasted_at = Some(now - time::Duration::hours(3));

        Self {
            _directory: directory,
            database,
            coordinator,
            competition,
            contract,
            broadcasts,
        }
    }

    async fn entries(&self) -> Vec<UserEntry> {
        self.coordinator
            .competition_store
            .get_competition_entries(self.competition.id, vec![EntryStatus::Paid])
            .await
            .unwrap()
    }

    fn broadcasts(&self) -> Vec<Transaction> {
        self.broadcasts.lock().unwrap().clone()
    }
}

/// While fees make sweeping an unpaid winner's split output leave dust, but a lower fee would
/// not, the coordinator neither broadcasts nor records anything: the competition stays open, so
/// the reclaim is tried again later instead of being given up.
#[tokio::test]
async fn split_reclaims_wait_while_fees_leave_dust() {
    // A small pot while fees are high even for a day's confirmation.
    let mut settlement = UnpaidWinners::new(
        Amount::from_sat(10_000),
        HashMap::from([(1, 60.0), (ECONOMY_FEE_TARGET, 50.0)]),
    )
    .await;

    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "delta_broadcasted");
    assert!(settlement.broadcasts().is_empty());
    for entry in settlement.entries().await {
        assert!(entry.reclaimed_broadcasted_at.is_none());
        assert!(entry.sweep_uneconomic_at.is_none());
    }

    settlement
        .coordinator
        .publish_delta2_transactions(&mut settlement.competition)
        .await
        .unwrap();
    assert!(settlement.competition.completed_at.is_none());
    settlement.database.close().await.unwrap();
}

/// When an unpaid winner's split output would be dust after any fee, even LND's floor, the
/// coordinator leaves it on chain and records that on the entry, once, so the competition
/// completes instead of retrying a broadcast the network rejects every minute.
#[tokio::test]
async fn split_reclaims_dust_at_any_fee_are_recorded_once_and_the_competition_completes() {
    // Split outputs of about 400 sats: less than the dust limit plus the fee at LND's floor.
    let mut settlement = UnpaidWinners::new(
        Amount::from_sat(1_700),
        HashMap::from([(1, 60.0), (ECONOMY_FEE_TARGET, LND_FLOOR_SAT_PER_VB)]),
    )
    .await;
    for player_index in 0..3 {
        let win_condition = WinCondition {
            outcome: Outcome::Attestation(0),
            player_index,
        };
        let (input, prevout) = settlement
            .contract
            .split_reclaim_tx_input_and_prevout(&win_condition)
            .unwrap();
        let refused = simple_sweep_tx(
            settlement.contract.params().market_maker.pubkey,
            input,
            settlement.contract.split_reclaim_tx_input_weight(),
            prevout.value,
            FeeRate::from_sat_per_kwu(253),
        )
        .unwrap_err();
        assert!(
            refused.permanent,
            "the fixture's split outputs must be dust at any fee"
        );
    }

    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "completed");
    assert!(settlement.broadcasts().is_empty());
    let entries = settlement.entries().await;
    assert_eq!(entries.len(), 3);
    let recorded: Vec<_> = entries
        .iter()
        .map(|entry| {
            assert!(entry.reclaimed_broadcasted_at.is_none());
            entry
                .sweep_uneconomic_at
                .expect("the skipped reclaim is recorded")
        })
        .collect();

    // Settling again sees the outputs as handled: no broadcast, and the record is unchanged.
    settlement
        .coordinator
        .publish_delta2_transactions(&mut settlement.competition)
        .await
        .unwrap();
    assert!(settlement.competition.completed_at.is_some());
    assert!(settlement.broadcasts().is_empty());
    let again: Vec<_> = settlement
        .entries()
        .await
        .iter()
        .map(|entry| entry.sweep_uneconomic_at.unwrap())
        .collect();
    assert_eq!(again, recorded);
    settlement.database.close().await.unwrap();
}

/// Split-reclaims are priced for confirmation within a day at LND's precision. The next-block
/// estimate here would make every reclaim dust; the day's estimate, LND's floor, does not.
#[tokio::test]
async fn split_reclaims_pay_the_economy_fee_rate() {
    let settlement = UnpaidWinners::new(
        Amount::from_sat(10_000),
        HashMap::from([(1, 60.0), (ECONOMY_FEE_TARGET, LND_FLOOR_SAT_PER_VB)]),
    )
    .await;

    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "completed");
    let broadcasts = settlement.broadcasts();
    assert_eq!(broadcasts.len(), 3);
    for (player_index, transaction) in broadcasts.iter().enumerate() {
        let win_condition = WinCondition {
            outcome: Outcome::Attestation(0),
            player_index,
        };
        let (input, prevout) = settlement
            .contract
            .split_reclaim_tx_input_and_prevout(&win_condition)
            .unwrap();
        assert_eq!(transaction.input[0].previous_output, input.previous_output);
        let fee = prevout.value - transaction.output[0].value;
        let predicted = predict_weight(
            [settlement.contract.split_reclaim_tx_input_weight()],
            [transaction.output[0].script_pubkey.len()],
        );
        assert_eq!(
            fee,
            FeeRate::from_sat_per_kwu(253)
                .checked_mul_by_weight(predicted)
                .unwrap()
        );
        assert!(
            fee.to_sat() >= transaction.vsize() as u64,
            "the signed reclaim pays the 1 sat/vB relay minimum"
        );
    }
    for entry in settlement.entries().await {
        assert!(entry.reclaimed_broadcasted_at.is_some());
        assert!(entry.sweep_uneconomic_at.is_none());
    }
    settlement.database.close().await.unwrap();
}
