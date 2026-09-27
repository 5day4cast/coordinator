use super::*;
use dlctix::{EventLockingConditions, MarketMaker, SignedContract};

fn signed_contract() -> (SignedContract, Scalar, [Scalar; 3]) {
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
        funding_value: Amount::from_sat(100_000),
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
        );
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
        );
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
    );
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
