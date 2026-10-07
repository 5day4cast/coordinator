use super::*;
use crate::domain::{PaymentStatus, PayoutError};
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
    signed_contract_expiring(funding_value, None)
}

/// As [`signed_contract_funded`], with an expiry whose outcome refunds the players equally.
fn signed_contract_expiring(
    funding_value: Amount,
    expiry: Option<u32>,
) -> (SignedContract, Scalar, [Scalar; 3]) {
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
            expiry,
        },
        outcome_payouts: std::iter::once(Outcome::Attestation(0))
            .chain(expiry.map(|_| Outcome::Expiry))
            .map(|outcome| (outcome, PayoutWeights::from([(0, 1), (1, 1), (2, 1)])))
            .collect(),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value,
        relative_locktime_block_delta: 72,
        anchor: None,
        outcome_bound_splits: false,
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
        async fn spending_transaction(
            &self,
            outpoint: OutPoint,
            output: TxOut,
        ) -> Result<Option<Transaction>, anyhow::Error>;
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

/// How the fixture's chain answers.
#[derive(Clone)]
struct ChainSetup {
    tip: u32,
    /// The height each transaction confirmed at, if it did.
    confirmed: Arc<dyn Fn(&Txid) -> Option<u32> + Send + Sync>,
    /// Whether every broadcast is refused, as for a transaction spending a spent output.
    refuse_broadcasts: bool,
    /// What a lookup of a contract output answers.
    output_status: PayoutOutputStatus,
    /// The transaction that spent a contract output, when one did.
    spender: Option<Transaction>,
}

impl Default for ChainSetup {
    /// The outcome and split transactions confirmed long ago, and every output is unspent.
    fn default() -> Self {
        Self {
            tip: 1_000,
            confirmed: Arc::new(|_: &Txid| Some(100)),
            refuse_broadcasts: false,
            output_status: PayoutOutputStatus {
                confirmation_height: Some(100),
                current_height: 1_000,
                unspent: true,
            },
            spender: None,
        }
    }
}

/// A competition past its split outputs' reclaim delay whose three winners were never paid.
struct UnpaidWinners {
    _directory: tempfile::TempDir,
    database: DBConnection,
    coordinator: Coordinator,
    competition: Competition,
    contract: SignedContract,
    players: [Scalar; 3],
    broadcasts: Arc<Mutex<Vec<Transaction>>>,
    ln: MockLnClient,
}

impl UnpaidWinners {
    async fn new(funding_value: Amount, fee_rates: HashMap<u16, f64>) -> Self {
        Self::with_contract(
            signed_contract_funded(funding_value),
            funding_value,
            fee_rates,
        )
        .await
    }

    async fn with_contract(
        contract: (SignedContract, Scalar, [Scalar; 3]),
        funding_value: Amount,
        fee_rates: HashMap<u16, f64>,
    ) -> Self {
        Self::with_chain(contract, funding_value, fee_rates, ChainSetup::default()).await
    }

    async fn with_chain(
        (contract, market_maker, players): (SignedContract, Scalar, [Scalar; 3]),
        funding_value: Amount,
        fee_rates: HashMap<u16, f64>,
        setup: ChainSetup,
    ) -> Self {
        let broadcasts = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MockChain::new();
        chain
            .expect_get_derived_private_key()
            .returning(move || Ok(market_maker));
        let ChainSetup {
            tip,
            confirmed,
            refuse_broadcasts,
            output_status,
            spender,
        } = setup;
        chain.expect_get_current_height().returning(move || Ok(tip));
        chain
            .expect_get_confirmed_blockchain_time()
            .returning(|_| Ok(1_790_000_001));
        chain
            .expect_get_tx_confirmation_height()
            .returning(move |txid| Ok(confirmed(txid)));
        chain
            .expect_get_estimated_fee_rates()
            .returning(move || Ok(fee_rates.clone()));
        let record = broadcasts.clone();
        chain.expect_broadcast().returning(move |transaction| {
            record.lock().unwrap().push(transaction.clone());
            if refuse_broadcasts {
                Err(anyhow!("bad-txns-inputs-missingorspent"))
            } else {
                Ok(())
            }
        });
        chain
            .expect_get_raw_transaction()
            .returning(|txid| Err(anyhow!("transaction {txid} not found")));
        chain
            .expect_payout_output_status()
            .returning(move |_, _| Ok(output_status));
        chain
            .expect_spending_transaction()
            .returning(move |_, _| Ok(spender.clone()));

        let directory = tempfile::tempdir().unwrap();
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let ln = MockLnClient::new();
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database.clone()),
            Arc::new(chain),
            Arc::new(ln.clone()),
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
            max_entries_per_player: 1,
            contract_options: None,
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
            players,
            broadcasts,
            ln,
        }
    }

    /// Record that the winner with entry key `key` was paid over Lightning and released the key.
    async fn pay(&self, key: Scalar) -> Uuid {
        let pubkey = hex::encode(key.base_point_mul().serialize());
        let entry = self
            .entries()
            .await
            .into_iter()
            .find(|entry| entry.ephemeral_pubkey == pubkey)
            .expect("an entry with this key");
        let (entry_id, secret) = (entry.id, hex::encode(key.serialize()));
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        self.database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO payouts (id, entry_id, payout_payment_request,
                         payout_amount_sats, initiated_at, succeed_at)
                     VALUES (?, ?, 'paid', 1, ?, ?)",
                )
                .bind(Uuid::now_v7().to_string())
                .bind(entry_id.to_string())
                .bind(now.clone())
                .bind(now)
                .execute(&pool)
                .await?;
                sqlx::query("UPDATE entries SET ephemeral_privatekey = ? WHERE id = ?")
                    .bind(secret)
                    .bind(entry_id.to_string())
                    .execute(&pool)
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        entry_id
    }

    /// Approve the sweep of every owed winner's output.
    async fn approve_every_sweep(&self) {
        for owed in self.coordinator.owed_winners(false).await.unwrap() {
            self.coordinator
                .approve_owed_winner_sweep(owed.entry_id)
                .await
                .unwrap();
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

/// Unpaid winners past their Lightning window are owed, and their outputs are held: nothing is
/// swept until an operator approves. Approved split-reclaims are priced for confirmation within a
/// day at LND's precision. The next-block estimate here would make every reclaim dust; the day's
/// estimate, LND's floor, does not. The winners stay owed after the sweep.
#[tokio::test]
async fn unpaid_winners_are_held_until_approved_and_then_swept_at_the_economy_fee_rate() {
    let settlement = UnpaidWinners::new(
        Amount::from_sat(10_000),
        HashMap::from([(1, 60.0), (ECONOMY_FEE_TARGET, LND_FLOOR_SAT_PER_VB)]),
    )
    .await;

    let held = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(held.state_name(), "delta_broadcasted");
    assert!(
        settlement.broadcasts().is_empty(),
        "nothing is swept unapproved"
    );
    let owed = settlement.coordinator.owed_winners(false).await.unwrap();
    assert_eq!(owed.len(), 3);
    for winner in &owed {
        assert!(winner.is_owed() && winner.sweep_approved_at.is_none());
        assert_eq!(winner.sweepable_at_height, Some(100 + 144));
        assert!(winner.output_status().starts_with("Held"));
    }
    let counts = settlement
        .coordinator
        .competition_store
        .owed_winner_counts()
        .await
        .unwrap();
    assert_eq!((counts.owed, counts.sweeps_held), (3, 3));
    assert_eq!(
        counts.owed_sats,
        owed.iter()
            .map(|winner| winner.amount_sats as i64)
            .sum::<i64>()
    );
    // Settling again records nothing new.
    settlement.coordinator.process_status(held).await;
    assert_eq!(
        settlement.coordinator.owed_winners(true).await.unwrap(),
        owed
    );

    settlement.approve_every_sweep().await;
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
    let owed = settlement.coordinator.owed_winners(false).await.unwrap();
    assert_eq!(owed.len(), 3, "a swept winner is still owed");
    assert!(owed.iter().all(|winner| winner.swept_at.is_some()));
    let counts = settlement
        .coordinator
        .competition_store
        .owed_winner_counts()
        .await
        .unwrap();
    assert_eq!((counts.owed, counts.sweeps_held), (3, 0));

    // Recording each payment leaves nobody owed.
    for winner in &owed {
        let settled = settlement
            .coordinator
            .settle_owed_winner(winner.entry_id, "paid hash 00ff")
            .await
            .unwrap();
        assert_eq!(settled.settled_note.as_deref(), Some("paid hash 00ff"));
    }
    assert!(settlement
        .coordinator
        .owed_winners(false)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        settlement
            .coordinator
            .settle_owed_winner(owed[0].entry_id, " ")
            .await,
        Err(Error::BadRequest(_))
    ));
    assert!(matches!(
        settlement
            .coordinator
            .approve_owed_winner_sweep(Uuid::now_v7())
            .await,
        Err(Error::NotFound(_))
    ));
    settlement.database.close().await.unwrap();
}

/// A contract the oracle never attested expires: the coordinator broadcast its expiry
/// transaction. Such a competition used to drop out of the runners with the pot on-chain and
/// no player refunded. It is stepped again, settles on the expiry outcome with the expiry
/// transaction as its outcome transaction, and its players are owed their refund shares.
#[tokio::test]
async fn an_expired_contract_settles_on_its_expiry_outcome() {
    let expiry = 1_790_000_000;
    let mut settlement = UnpaidWinners::with_contract(
        signed_contract_expiring(Amount::from_sat(30_000), Some(expiry)),
        Amount::from_sat(30_000),
        HashMap::from([(1, 2.0), (ECONOMY_FEE_TARGET, 1.0)]),
    )
    .await;
    // A row from before the expiry transaction was recorded as the outcome transaction.
    let competition = &mut settlement.competition;
    competition.attestation = None;
    competition.outcome_transaction = None;
    competition.outcome_broadcasted_at = None;
    competition.delta_broadcasted_at = None;
    competition.expiry_broadcasted_at = Some(OffsetDateTime::now_utc() - time::Duration::hours(2));
    let store = &settlement.coordinator.competition_store;
    store
        .update_competitions(vec![competition.clone()])
        .await
        .unwrap();

    assert!(store
        .active_competition_ids()
        .await
        .unwrap()
        .contains(&competition.id));
    assert!(competition.settled_by_expiry());
    assert_eq!(competition.get_current_outcome().unwrap(), Outcome::Expiry);
    let status = CompetitionStatus::from(competition.clone());
    assert_eq!(status.state_name(), "expiry_broadcasted");

    let next = settlement.coordinator.process_status(status).await;
    assert_eq!(next.state_name(), "outcome_broadcasted");
    let settled = next.into_competition();
    let expiry_txid = settlement.contract.expiry_tx().unwrap().compute_txid();
    assert_eq!(
        settled
            .outcome_transaction
            .as_ref()
            .map(Transaction::compute_txid),
        Some(expiry_txid)
    );
    assert_eq!(
        CompetitionStatus::from(settled.clone()).state_name(),
        "outcome_broadcasted",
        "the stored fields reload in the state the step moved to"
    );
    assert_eq!(settled.get_current_outcome().unwrap(), Outcome::Expiry);
    let params = settlement.contract.params();
    for player in &params.players {
        assert_eq!(
            winner_payout_sats(params, &Outcome::Expiry, &player.pubkey).unwrap(),
            10_000
        );
    }

    // An attested contract whose outcome transaction is not the expiry transaction settles on
    // its attestation, even if an expiry broadcast was also recorded.
    let mut attested = settled.clone();
    attested.attestation = Some(Scalar::from_slice(&[10; 32]).unwrap().into());
    attested.outcome_transaction = Some(Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![],
        output: vec![],
    });
    assert!(!attested.settled_by_expiry());
    assert_eq!(
        attested.get_current_outcome().unwrap(),
        Outcome::Attestation(0)
    );
}

/// The mock oracle has no event and returns an error. Expiry remains usable.
#[tokio::test]
async fn an_oracle_error_cannot_disable_a_mature_expiry_refund() {
    let mut fixture = UnpaidWinners::with_contract(
        signed_contract_expiring(Amount::from_sat(30_000), Some(1_790_000_000)),
        Amount::from_sat(30_000),
        HashMap::new(),
    )
    .await;
    fixture.competition.attestation = None;
    fixture.competition.outcome_transaction = None;
    fixture.competition.outcome_broadcasted_at = None;
    fixture
        .coordinator
        .check_oracle_attestation(&mut fixture.competition)
        .await
        .unwrap();
    assert_eq!(
        fixture.competition.get_current_outcome().unwrap(),
        Outcome::Expiry
    );
    assert!(fixture.competition.expiry_broadcasted_at.is_some());
    assert_eq!(fixture.broadcasts.lock().unwrap().len(), 1);
    fixture
        .coordinator
        .check_oracle_attestation(&mut fixture.competition)
        .await
        .unwrap();
    assert_eq!(fixture.broadcasts.lock().unwrap().len(), 1);
    fixture.database.close().await.unwrap();
}

#[tokio::test]
async fn an_oracle_error_before_chain_expiry_preserves_the_failure() {
    let mut fixture = UnpaidWinners::with_contract(
        signed_contract_expiring(Amount::from_sat(30_000), Some(1_790_000_002)),
        Amount::from_sat(30_000),
        HashMap::new(),
    )
    .await;
    fixture.competition.attestation = None;
    fixture.competition.outcome_transaction = None;
    fixture.competition.outcome_broadcasted_at = None;
    assert!(fixture
        .coordinator
        .check_oracle_attestation(&mut fixture.competition)
        .await
        .is_err());
    assert!(fixture.competition.expiry_broadcasted_at.is_none());
    assert!(fixture.broadcasts.lock().unwrap().is_empty());
    fixture.database.close().await.unwrap();
}

/// The step that broadcasts the expiry transaction returns the state the stored fields reload
/// as: the competition settles from the expiry transaction, its outcome transaction. Returning
/// "awaiting attestation" instead made the runner report a state that cannot progress.
#[tokio::test]
async fn the_step_that_broadcasts_expiry_moves_on_to_settling_it() {
    let mut fixture = UnpaidWinners::with_contract(
        signed_contract_expiring(Amount::from_sat(30_000), Some(1_790_000_000)),
        Amount::from_sat(30_000),
        HashMap::new(),
    )
    .await;
    let competition = &mut fixture.competition;
    competition.attestation = None;
    competition.outcome_transaction = None;
    competition.outcome_broadcasted_at = None;
    competition.delta_broadcasted_at = None;
    competition.awaiting_attestation_at = Some(OffsetDateTime::now_utc());
    let status = CompetitionStatus::from(competition.clone());
    assert_eq!(status.state_name(), "awaiting_attestation");

    let next = fixture.coordinator.process_status(status).await;
    assert_eq!(next.state_name(), "outcome_broadcasted");
    let settling = next.into_competition();
    assert!(settling.settled_by_expiry());
    assert_eq!(settling.get_current_outcome().unwrap(), Outcome::Expiry);
    assert_eq!(fixture.broadcasts().len(), 1);
    assert_eq!(
        CompetitionStatus::from(settling).state_name(),
        "outcome_broadcasted"
    );
    fixture.database.close().await.unwrap();
}

/// Past the contract's expiry an attestation is accepted only if it opens one of the event's
/// outcomes, as before it. One that opens none used to be taken for the expiry outcome and
/// stored, and a stored attestation stops the expiry transaction from being broadcast.
#[tokio::test]
async fn an_attestation_past_expiry_must_still_open_an_outcome() {
    let fixture = UnpaidWinners::with_contract(
        signed_contract_expiring(Amount::from_sat(30_000), Some(1_790_000_000)),
        Amount::from_sat(30_000),
        HashMap::new(),
    )
    .await;
    let competition = &fixture.competition;
    let stray = Scalar::from_slice(&[11; 32]).unwrap().into();
    assert!(competition.verify_event_attestation(&stray).is_err());
    let attested = Scalar::from_slice(&[10; 32]).unwrap().into();
    assert_eq!(
        competition.verify_event_attestation(&attested).unwrap(),
        Outcome::Attestation(0)
    );
    fixture.database.close().await.unwrap();
}

/// Settle-only mode stops no settlement: the unpaid winners' split outputs are reclaimed as
/// usual once approved, and the competition completes.
#[tokio::test]
async fn settle_only_mode_still_reclaims_unpaid_winners() {
    let mut settlement = UnpaidWinners::new(
        Amount::from_sat(10_000),
        HashMap::from([(1, 60.0), (ECONOMY_FEE_TARGET, LND_FLOOR_SAT_PER_VB)]),
    )
    .await;
    settlement.coordinator.settle_only = SettleOnly {
        enabled: true,
        unstarted: crate::config::SettleOnlyUnstarted::Refund,
    };
    let held = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(held.state_name(), "delta_broadcasted");
    settlement.approve_every_sweep().await;

    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "completed");
    assert_eq!(settlement.broadcasts().len(), 3);
    for entry in settlement.entries().await {
        assert!(entry.reclaimed_broadcasted_at.is_some());
    }
    settlement.database.close().await.unwrap();
}

/// A coordinator whose chain answers every broadcast with an error, as for a transaction it
/// already holds, and that lists `known` as on chain.
async fn behind_the_chain(
    market_maker: Scalar,
    known: Option<Transaction>,
) -> (tempfile::TempDir, DBConnection, Coordinator) {
    let mut chain = MockChain::new();
    chain
        .expect_get_derived_private_key()
        .returning(move || Ok(market_maker));
    chain
        .expect_broadcast()
        .returning(|_| Err(anyhow!("transaction already in block chain")));
    chain.expect_get_raw_transaction().returning(move |txid| {
        known
            .clone()
            .filter(|tx| tx.compute_txid() == *txid)
            .ok_or_else(|| anyhow!("transaction not found"))
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
        "restore-test".into(),
        false,
        1,
    )
    .await
    .unwrap();
    (directory, database, coordinator)
}

/// An attested competition whose outcome transaction the database has not recorded.
fn attested(contract: &SignedContract) -> Competition {
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
        total_competition_pool: 100_000,
        relative_locktime_block_delta: Some(72),
        unlisted: false,
        scoring_rules: None,
        scoring_fields: None,
        max_entries_per_player: 1,
        contract_options: None,
    });
    competition.event_announcement = Some(contract.params().event.clone());
    competition.attestation = Some(Scalar::from_slice(&[10; 32]).unwrap().into());
    competition.signed_contract = Some(contract.clone());
    competition
}

/// The database is behind the chain: it was restored from before the outcome transaction was
/// broadcast, and the chain already holds it. The broadcast is refused, the chain lists the very
/// transaction, so it is recorded as broadcast and settlement moves on. A transaction the chain
/// does not hold still fails, and nothing is recorded.
#[tokio::test]
async fn an_outcome_transaction_already_on_chain_counts_as_broadcast() {
    let (contract, market_maker, _) = signed_contract();
    let attestation = Scalar::from_slice(&[10; 32]).unwrap();
    let outcome_tx = contract.signed_outcome_tx(0, attestation).unwrap();

    let (_directory, database, coordinator) =
        behind_the_chain(market_maker, Some(outcome_tx.clone())).await;
    let mut competition = attested(&contract);
    coordinator
        .publish_outcome_transaction(&mut competition)
        .await
        .unwrap();
    assert!(competition.outcome_broadcasted_at.is_some());
    assert_eq!(competition.outcome_transaction, Some(outcome_tx.clone()));
    // Once recorded, it is not broadcast again.
    coordinator
        .publish_outcome_transaction(&mut competition)
        .await
        .unwrap();
    database.close().await.unwrap();

    let (_directory, database, coordinator) = behind_the_chain(market_maker, None).await;
    let mut competition = attested(&contract);
    assert!(coordinator
        .publish_outcome_transaction(&mut competition)
        .await
        .is_err());
    assert!(competition.outcome_broadcasted_at.is_none());
    database.close().await.unwrap();
}

/// The unpaid winners as a contract not yet split: their Lightning payouts are still open.
async fn unsplit_winners() -> (UnpaidWinners, u64) {
    let mut settlement = UnpaidWinners::new(
        Amount::from_sat(100_000),
        HashMap::from([(1, 2.0), (ECONOMY_FEE_TARGET, 1.0)]),
    )
    .await;
    settlement.competition.delta_broadcasted_at = None;
    settlement
        .coordinator
        .competition_store
        .update_competitions(vec![settlement.competition.clone()])
        .await
        .unwrap();
    let entry = settlement.entries().await.remove(0);
    let owed = winner_payout_sats(
        settlement.contract.params(),
        &Outcome::Attestation(0),
        &entry.ephemeral_pubkey.parse::<Point>().unwrap(),
    )
    .unwrap();
    (settlement, owed)
}

/// The database is behind LND: it was restored from before a payout was recorded, and LND paid
/// it. The payment's hash is unknown and its amount is what each winner is owed, so every such
/// winner's Lightning payout is held rather than paid again. A payment the database knows holds
/// nothing. Reconciling again changes nothing, and a released hold stays released.
#[tokio::test]
async fn a_payment_lnd_made_that_the_database_lost_holds_the_payouts_it_could_be() {
    let (settlement, owed) = unsplit_winners().await;
    let coordinator = &settlement.coordinator;
    let store = &coordinator.competition_store;
    let entries = settlement.entries().await;

    // A payout the database knows, which LND paid.
    let known = settlement
        .ln
        .add_invoice(owed, 3_600, "known".into(), Uuid::now_v7())
        .await
        .unwrap()
        .payment_request;
    store
        .store_payout_info_pending(
            entries[2].id,
            hex::encode([3; 32]),
            "key".into(),
            known.clone(),
            owed,
        )
        .await
        .unwrap();
    settlement
        .ln
        .send_payment(known, owed, 60, 1_000)
        .await
        .unwrap();
    let found = coordinator.reconcile_after_restore().await.unwrap();
    assert_eq!(found, RestoreReconciliation::default());
    for entry in &entries {
        assert!(!store.payout_held(entry.id).await.unwrap());
    }

    // A payout recorded after the backup was taken: LND paid it, the database has no row.
    let lost = settlement
        .ln
        .add_invoice(owed, 3_600, "lost".into(), Uuid::now_v7())
        .await
        .unwrap()
        .payment_request;
    settlement
        .ln
        .send_payment(lost, owed, 60, 1_000)
        .await
        .unwrap();
    // Only the winners owed exactly that amount are held, and not the one whose payout the
    // database knows is under way.
    let owed_that = |entry: &UserEntry| {
        entry.id != entries[2].id
            && winner_payout_sats(
                settlement.contract.params(),
                &Outcome::Attestation(0),
                &entry.ephemeral_pubkey.parse::<Point>().unwrap(),
            )
            .unwrap()
                == owed
    };
    let matching: Vec<&UserEntry> = entries.iter().filter(|e| owed_that(e)).collect();
    assert!(
        matching.len() >= 2,
        "{} winners owed {owed}",
        matching.len()
    );
    let found = coordinator.reconcile_after_restore().await.unwrap();
    assert_eq!(found.payouts_held, matching.len());
    for entry in &entries {
        assert_eq!(
            store.payout_held(entry.id).await.unwrap(),
            owed_that(entry),
            "{}",
            entry.id
        );
    }
    assert!(matches!(
        coordinator
            .verify_payout_release(
                &entries[0].pubkey,
                settlement.competition.id,
                entries[0].id,
                entries[0].ticket_id,
                "key",
                &hex::encode([1; 32]),
            )
            .await,
        Err(Error::Conflict(_))
    ));

    assert_eq!(
        coordinator.reconcile_after_restore().await.unwrap(),
        RestoreReconciliation::default(),
        "reconciling again holds nothing new"
    );
    assert_eq!(
        coordinator
            .release_payout_hold(entries[0].id)
            .await
            .unwrap(),
        1
    );
    coordinator.reconcile_after_restore().await.unwrap();
    assert!(!store.payout_held(entries[0].id).await.unwrap());
    assert!(store.payout_held(matching[1].id).await.unwrap());
    assert_eq!(
        coordinator.payout_holds(false).await.unwrap().len(),
        matching.len() - 1
    );
    assert_eq!(
        coordinator.payout_holds(true).await.unwrap().len(),
        matching.len()
    );
    settlement.database.close().await.unwrap();
}

/// The database is behind LND: it recorded a payout as failed, and LND paid it. It is marked
/// paid with LND's proof, so the entry is not paid again, and reconciling again changes nothing.
#[tokio::test]
async fn a_failed_payout_lnd_paid_is_marked_paid() {
    let (settlement, owed) = unsplit_winners().await;
    let coordinator = &settlement.coordinator;
    let store = &coordinator.competition_store;
    let entry = settlement.entries().await.remove(0);

    let proof = [4; 32];
    let hash = hex::encode(sha256::Hash::hash(&proof).to_byte_array());
    let invoice = settlement.ln.invoice_for_hash(owed, &hash).unwrap();
    let payout = store
        .store_payout_info_pending(entry.id, hex::encode([1; 32]), "key".into(), invoice, owed)
        .await
        .unwrap();
    assert!(store
        .mark_payout_failed(
            payout,
            OffsetDateTime::now_utc(),
            PayoutError::FailedToPayOut("timed out".into()),
        )
        .await
        .unwrap());
    settlement
        .ln
        .set_payment(&hash, PaymentStatus::Succeeded, Some(hex::encode(proof)))
        .unwrap();

    let found = coordinator.reconcile_after_restore().await.unwrap();
    assert_eq!(found.payouts_marked_paid, 1);
    assert_eq!(found.payouts_held, 0);
    let payout = store.get_payout(payout).await.unwrap().unwrap();
    assert!(payout.succeed_at.is_some() && payout.failed_at.is_none());
    assert_eq!(payout.payment_preimage, Some(hex::encode(proof)));
    let entry = settlement
        .entries()
        .await
        .into_iter()
        .find(|e| e.id == entry.id)
        .unwrap();
    assert!(entry.paid_out_at.is_some());
    assert_eq!(
        coordinator.reconcile_after_restore().await.unwrap(),
        RestoreReconciliation::default()
    );
    settlement.database.close().await.unwrap();
}

/// The txid of the fixture's outcome transaction, as the competition records it.
fn recorded_outcome_txid(settlement: &UnpaidWinners) -> Txid {
    settlement
        .competition
        .outcome_transaction
        .as_ref()
        .unwrap()
        .compute_txid()
}

/// The fixture's split transaction's txid.
fn split_txid(contract: &SignedContract) -> Txid {
    contract
        .unsigned_split_tx(&Outcome::Attestation(0))
        .unwrap()
        .compute_txid()
}

/// A chain at `tip` where the recorded outcome transaction confirmed at `outcome` and the split
/// transaction at `split`, if they did.
async fn settling_at(tip: u32, outcome: u32, split: Option<u32>) -> UnpaidWinners {
    let contract = signed_contract_funded(Amount::from_sat(100_000));
    let split_id = split_txid(&contract.0);
    let empty_outcome = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![],
        output: vec![],
    }
    .compute_txid();
    let setup = ChainSetup {
        tip,
        confirmed: Arc::new(move |txid: &Txid| {
            if *txid == empty_outcome {
                Some(outcome)
            } else if *txid == split_id {
                split
            } else {
                None
            }
        }),
        ..ChainSetup::default()
    };
    let settlement = UnpaidWinners::with_chain(
        contract,
        Amount::from_sat(100_000),
        HashMap::from([(1, 2.0), (ECONOMY_FEE_TARGET, 1.0)]),
        setup,
    )
    .await;
    assert_eq!(recorded_outcome_txid(&settlement), empty_outcome);
    settlement
}

/// An unpaid winner may still be paid over Lightning until 13 blocks before they could claim on
/// chain, `delta` blocks after the split confirmed: until then they are not owed, and nothing
/// about their output is decided.
#[tokio::test]
async fn unpaid_winners_inside_their_lightning_window_are_not_owed_yet() {
    // The split confirmed at 900; the window closes at 900 + 72 - 13 = 959.
    let settlement = settling_at(950, 800, Some(900)).await;
    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "delta_broadcasted");
    assert!(settlement.broadcasts().is_empty());
    assert!(settlement
        .coordinator
        .owed_winners(true)
        .await
        .unwrap()
        .is_empty());
    settlement.database.close().await.unwrap();

    let settlement = settling_at(959, 800, Some(900)).await;
    settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(
        settlement
            .coordinator
            .owed_winners(false)
            .await
            .unwrap()
            .len(),
        3,
        "at the window's close they are owed"
    );
    assert!(
        settlement.broadcasts().is_empty(),
        "the reclaim delay runs on"
    );
    settlement.database.close().await.unwrap();
}

/// A winner paid over Lightning has their split output closed to the market maker with the key
/// the payout released, after the split, while the unpaid ones are held.
#[tokio::test]
async fn a_paid_winner_s_output_closes_after_the_split() {
    let settlement = UnpaidWinners::new(
        Amount::from_sat(100_000),
        HashMap::from([(1, 2.0), (ECONOMY_FEE_TARGET, 1.0)]),
    )
    .await;
    let paid = settlement.pay(settlement.players[0]).await;
    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "delta_broadcasted");
    let broadcasts = settlement.broadcasts();
    assert_eq!(broadcasts.len(), 1);
    let (close_input, _) = settlement
        .contract
        .split_close_tx_input_and_prevout(&WinCondition {
            outcome: Outcome::Attestation(0),
            player_index: 0,
        })
        .unwrap();
    assert_eq!(
        broadcasts[0].input[0].previous_output,
        close_input.previous_output
    );
    let entries = settlement.entries().await;
    let entry = entries.iter().find(|entry| entry.id == paid).unwrap();
    assert!(entry.sellback_broadcasted_at.is_some());
    let owed = settlement.coordinator.owed_winners(false).await.unwrap();
    assert_eq!(owed.len(), 2);
    assert!(owed.iter().all(|winner| winner.entry_id != paid));
    settlement.database.close().await.unwrap();
}

/// A sweep of an output already spent can never confirm. Once the chain shows the output spent
/// by the winner's own claim, the coordinator records it, the winner is no longer owed, and the
/// competition completes instead of retrying the sweep forever.
#[tokio::test]
async fn a_split_output_the_winner_claimed_is_recorded_instead_of_swept_forever() {
    let claim = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![],
        output: vec![TxOut {
            value: Amount::from_sat(33_000),
            script_pubkey: ScriptBuf::new(),
        }],
    };
    let setup = ChainSetup {
        refuse_broadcasts: true,
        output_status: PayoutOutputStatus {
            confirmation_height: Some(100),
            current_height: 1_000,
            unspent: false,
        },
        spender: Some(claim.clone()),
        ..ChainSetup::default()
    };
    let settlement = UnpaidWinners::with_chain(
        signed_contract_funded(Amount::from_sat(100_000)),
        Amount::from_sat(100_000),
        HashMap::from([(1, 2.0), (ECONOMY_FEE_TARGET, 1.0)]),
        setup,
    )
    .await;
    let held = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(held.state_name(), "delta_broadcasted");
    settlement.approve_every_sweep().await;

    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "completed");
    assert_eq!(
        settlement.broadcasts().len(),
        3,
        "each sweep was tried once"
    );
    for entry in settlement.entries().await {
        assert!(entry.split_output_spent_at.is_some());
        assert!(entry.reclaimed_broadcasted_at.is_none());
    }
    assert!(settlement
        .coordinator
        .owed_winners(false)
        .await
        .unwrap()
        .is_empty());
    let resolved = settlement.coordinator.owed_winners(true).await.unwrap();
    assert_eq!(resolved.len(), 3);
    for winner in resolved {
        assert_eq!(
            winner.claim_txid,
            Some(claim.compute_txid().to_string()),
            "the winner's claim is kept"
        );
        assert!(winner.output_status().starts_with("Claimed on chain"));
        assert!(matches!(
            settlement
                .coordinator
                .approve_owed_winner_sweep(winner.entry_id)
                .await,
            Err(Error::BadRequest(_))
        ));
    }
    settlement.database.close().await.unwrap();
}

/// A sweep refused because the coordinator's own sweep of the output, whose record was lost,
/// already confirmed is recorded as that sweep.
#[tokio::test]
async fn a_split_output_already_swept_to_the_market_maker_counts_as_swept() {
    let (contract, market_maker, players) = signed_contract_funded(Amount::from_sat(100_000));
    let ours = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![],
        output: vec![TxOut {
            value: Amount::from_sat(33_000),
            script_pubkey: p2tr_script_pubkey(contract.params().market_maker.pubkey),
        }],
    };
    let setup = ChainSetup {
        refuse_broadcasts: true,
        output_status: PayoutOutputStatus {
            confirmation_height: Some(100),
            current_height: 1_000,
            unspent: false,
        },
        spender: Some(ours),
        ..ChainSetup::default()
    };
    let settlement = UnpaidWinners::with_chain(
        (contract, market_maker, players),
        Amount::from_sat(100_000),
        HashMap::from([(1, 2.0), (ECONOMY_FEE_TARGET, 1.0)]),
        setup,
    )
    .await;
    settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    settlement.approve_every_sweep().await;
    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "completed");
    for entry in settlement.entries().await {
        assert!(entry.reclaimed_broadcasted_at.is_some());
        assert!(entry.split_output_spent_at.is_none());
    }
    assert_eq!(
        settlement
            .coordinator
            .owed_winners(false)
            .await
            .unwrap()
            .len(),
        3,
        "the winners were swept, not paid"
    );
    settlement.database.close().await.unwrap();
}

/// Once every winner is paid, the coordinator closes the whole outcome at once, long before any
/// winner could broadcast the split, instead of waiting until the split is valid and racing them.
#[tokio::test]
async fn every_winner_paid_closes_the_outcome_at_once() {
    // The outcome confirmed 11 blocks ago, far less than delta (72).
    let mut settlement = settling_at(1_000, 990, None).await;
    settlement.competition.delta_broadcasted_at = None;
    for key in settlement.players {
        settlement.pay(key).await;
    }
    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "delta_broadcasted");
    let broadcasts = settlement.broadcasts();
    assert_eq!(broadcasts.len(), 1);
    let (close_input, _) = settlement
        .contract
        .outcome_close_tx_input_and_prevout(&Outcome::Attestation(0))
        .unwrap();
    assert_eq!(
        broadcasts[0].input[0].previous_output,
        close_input.previous_output
    );
    for entry in settlement.entries().await {
        assert!(entry.sellback_broadcasted_at.is_some());
    }
    let completed = settlement.coordinator.process_status(status).await;
    assert_eq!(completed.state_name(), "completed");
    settlement.database.close().await.unwrap();

    // With fewer confirmations than a shallow reorganization could undo, it waits.
    let mut settlement = settling_at(1_000, 997, None).await;
    settlement.competition.delta_broadcasted_at = None;
    for key in settlement.players {
        settlement.pay(key).await;
    }
    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "outcome_broadcasted");
    assert!(settlement.broadcasts().is_empty());
    settlement.database.close().await.unwrap();
}

/// A winner broadcast the split transaction before the coordinator closed the outcome. The
/// close could never confirm; the coordinator follows the split and closes each paid winner's
/// own output instead, before they could also claim it on chain.
#[tokio::test]
async fn a_split_a_winner_broadcast_is_followed_instead_of_closing_the_outcome() {
    let mut settlement = settling_at(1_000, 900, Some(995)).await;
    settlement.competition.delta_broadcasted_at = None;
    for key in settlement.players {
        settlement.pay(key).await;
    }
    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "delta_broadcasted");
    let broadcasts = settlement.broadcasts();
    assert_eq!(broadcasts.len(), 3, "one split-close per paid winner");
    for player_index in 0..3 {
        let (input, _) = settlement
            .contract
            .split_close_tx_input_and_prevout(&WinCondition {
                outcome: Outcome::Attestation(0),
                player_index,
            })
            .unwrap();
        assert!(broadcasts
            .iter()
            .any(|tx| tx.input[0].previous_output == input.previous_output));
    }
    for entry in settlement.entries().await {
        assert!(entry.sellback_broadcasted_at.is_some());
    }
    settlement.database.close().await.unwrap();
}

/// A window an earlier release closed at its fixed cutoff opens again while no winner can claim
/// on chain yet.
#[tokio::test]
async fn a_closed_payout_window_reopens_while_the_outcome_is_unspent() {
    let mut settlement = settling_at(1_000, 990, None).await;
    settlement.competition.delta_broadcasted_at = None;
    let store = &settlement.coordinator.competition_store;
    let id = settlement.competition.id;
    store.enable_automatic_payouts(id).await.unwrap();
    store.close_payout_window(id).await.unwrap();
    assert!(store.payout_window_is_closed(id).await.unwrap());

    let status = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(status.state_name(), "outcome_broadcasted");
    assert!(!store.payout_window_is_closed(id).await.unwrap());
    assert!(store.payout_window_is_open(id).await.unwrap());
    assert!(
        !store.reopen_payout_window(id).await.unwrap(),
        "already open"
    );
    settlement.database.close().await.unwrap();
}

/// A chain where the contract's expiry transaction confirmed at `expiry_height`, the tip is at
/// 1,000, nothing else confirmed, and every broadcast is refused as spending a spent output.
async fn expired_under_us(expiry_height: u32) -> UnpaidWinners {
    let contract = signed_contract_expiring(Amount::from_sat(30_000), Some(1_790_000_000));
    let expiry_txid = contract.0.expiry_tx().unwrap().compute_txid();
    let setup = ChainSetup {
        confirmed: Arc::new(move |txid: &Txid| (*txid == expiry_txid).then_some(expiry_height)),
        refuse_broadcasts: true,
        ..ChainSetup::default()
    };
    let mut settlement = UnpaidWinners::with_chain(
        contract,
        Amount::from_sat(30_000),
        HashMap::from([(1, 2.0), (ECONOMY_FEE_TARGET, 1.0)]),
        setup,
    )
    .await;
    settlement.competition.delta_broadcasted_at = None;
    settlement
}

/// Anyone may broadcast the expiry transaction once the contract expired. When it confirmed
/// while the coordinator held an attestation, the outcome transaction can never confirm: the
/// competition settles on the expiry outcome instead of retrying the outcome forever.
#[tokio::test]
async fn an_expiry_transaction_confirmed_in_place_of_the_outcome_settles_the_expiry() {
    let mut settlement = expired_under_us(990).await;
    settlement.competition.outcome_transaction = None;
    settlement.competition.outcome_broadcasted_at = None;
    let status = CompetitionStatus::from(settlement.competition.clone());
    assert_eq!(status.state_name(), "attested");
    let next = settlement.coordinator.process_status(status).await;
    assert_eq!(next.state_name(), "outcome_broadcasted");
    let settling = next.into_competition();
    assert!(settling.settled_by_expiry());
    assert_eq!(settling.get_current_outcome().unwrap(), Outcome::Expiry);
    assert!(settling.errors.is_empty());
    settlement.database.close().await.unwrap();

    // The outcome transaction was recorded as broadcast, and never confirmed.
    let mut settlement = expired_under_us(990).await;
    let attested = settlement
        .contract
        .signed_outcome_tx(0, Scalar::from_slice(&[10; 32]).unwrap())
        .unwrap();
    settlement.competition.outcome_transaction = Some(attested);
    let next = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(next.state_name(), "outcome_broadcasted");
    assert_eq!(
        next.into_competition().get_current_outcome().unwrap(),
        Outcome::Expiry
    );
    settlement.database.close().await.unwrap();

    // A shallow confirmation could still be undone: the outcome is retried meanwhile.
    let mut settlement = expired_under_us(998).await;
    settlement.competition.outcome_transaction = None;
    settlement.competition.outcome_broadcasted_at = None;
    let next = settlement
        .coordinator
        .process_status(CompetitionStatus::from(settlement.competition.clone()))
        .await;
    assert_eq!(next.state_name(), "attested");
    assert!(!next.into_competition().errors.is_empty());
    settlement.database.close().await.unwrap();
}

/// What a payout must be is what the outcome the contract settles on owes the entry. Once a
/// competition settles on its expiry outcome, a payout prepared for the attested one no longer
/// matches, and is not sent.
#[test]
fn an_entry_is_owed_what_the_settled_outcome_pays() {
    let now = OffsetDateTime::now_utc();
    let attestation = Scalar::from_slice(&[10; 32]).unwrap();
    let players = [1u8, 3, 5].map(|key| Scalar::from_slice(&[key; 32]).unwrap());
    let event = EventLockingConditions {
        locking_points: vec![attestation.base_point_mul().into()],
        expiry: Some(1_790_000_000),
    };
    let mut competition = Competition::new(&CreateEvent {
        id: Uuid::now_v7(),
        signing_date: now - time::Duration::hours(5),
        start_observation_date: now - time::Duration::hours(7),
        end_observation_date: now - time::Duration::hours(6),
        locations: vec!["KDEN".into()],
        number_of_values_per_entry: 3,
        number_of_places_win: 1,
        total_allowed_entries: 3,
        entry_fee: 10_000,
        coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
        total_competition_pool: 30_000,
        relative_locktime_block_delta: Some(72),
        unlisted: false,
        scoring_rules: None,
        scoring_fields: None,
        max_entries_per_player: 1,
        contract_options: None,
    });
    competition.contract_parameters = Some(ContractParameters {
        market_maker: MarketMaker {
            pubkey: Scalar::from_slice(&[7; 32]).unwrap().base_point_mul(),
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
        event: event.clone(),
        outcome_payouts: BTreeMap::from([
            (Outcome::Attestation(0), PayoutWeights::from([(0, 1)])),
            (
                Outcome::Expiry,
                PayoutWeights::from([(0, 1), (1, 1), (2, 1)]),
            ),
        ]),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value: Amount::from_sat(30_000),
        relative_locktime_block_delta: 72,
        anchor: None,
        outcome_bound_splits: false,
    });
    competition.event_announcement = Some(event);
    let key = |index: usize| hex::encode(players[index].base_point_mul().serialize());
    assert_eq!(competition.owed_to_entry(&key(0)), None, "no outcome yet");

    competition.attestation = Some(attestation.into());
    assert_eq!(competition.owed_to_entry(&key(0)), Some(30_000));
    assert_eq!(competition.owed_to_entry(&key(1)), Some(0));
    assert_eq!(competition.owed_to_entry("not a key"), None);

    // Settled on the expiry outcome: the expiry transaction is the outcome transaction.
    competition.attestation = None;
    competition.expiry_broadcasted_at = Some(now);
    assert_eq!(competition.owed_to_entry(&key(0)), Some(10_000));
    assert_eq!(competition.owed_to_entry(&key(1)), Some(10_000));
}
