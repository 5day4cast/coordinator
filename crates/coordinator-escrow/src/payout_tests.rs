use super::*;
use dlctix::{
    bitcoin::{
        hashes::sha256 as bitcoin_sha256,
        secp256k1::{Secp256k1, SecretKey},
    },
    Player,
};
use lightning_invoice::{InvoiceBuilder, PaymentSecret};

pub(crate) fn fixture() -> (ContractCommitment, ContractAuthorization) {
    let player = Player {
        pubkey: Scalar::from_slice(&[2; 32]).unwrap().base_point_mul(),
        ticket_hash: sha256(&[5; 32]),
        payout_hash: sha256(&[6; 32]),
    };
    let params = ContractParameters {
        market_maker: MarketMaker {
            pubkey: Scalar::from_slice(&[1; 32]).unwrap().base_point_mul(),
        },
        players: vec![
            player.clone(),
            Player {
                pubkey: Scalar::from_slice(&[3; 32]).unwrap().base_point_mul(),
                ticket_hash: sha256(&[7; 32]),
                payout_hash: sha256(&[8; 32]),
            },
        ],
        event: EventLockingConditions {
            locking_points: vec![MaybePoint::Valid(
                Scalar::from_slice(&[4; 32]).unwrap().base_point_mul(),
            )],
            expiry: Some(500_000),
        },
        outcome_payouts: BTreeMap::from([
            (Outcome::Attestation(0), BTreeMap::from([(0, 100)])),
            (Outcome::Expiry, BTreeMap::from([(0, 50), (1, 50)])),
        ]),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value: Amount::from_sat(100_000),
        relative_locktime_block_delta: 72,
        anchor: None,
        outcome_bound_splits: false,
    };
    let terms = ContractAuthorization {
        competition_id: Uuid::from_u128(1),
        entry_id: Uuid::from_u128(2),
        network: Network::Regtest,
        player_index: 0,
        player_count: 2,
        ticket_hash: player.ticket_hash,
        payout_hash: player.payout_hash,
        market_maker: params.market_maker.clone(),
        event: params.event.clone(),
        outcome_payouts: params.outcome_payouts.clone(),
        funding_value: params.funding_value,
        relative_locktime_block_delta: 72,
        max_fee_rate: params.fee_rate,
    };
    (
        ContractCommitment {
            contract_parameters: params,
            funding_outpoint: OutPoint::null(),
        },
        terms,
    )
}
fn policy(terms: &ContractAuthorization) -> PayoutPolicy {
    PayoutPolicy {
        queued_entry: None,
        automatic_lightning_address: Some("alice@example.com".into()),
        allow_invoice_fallback: true,
        release_entry_key_after_payment: true,
        contract_terms: serde_json::to_string(terms).unwrap(),
        ark_escrow: None,
    }
}
fn invoice(hashed_description: bool, currency: Currency, amount: u64) -> String {
    let builder = InvoiceBuilder::new(currency)
        .amount_milli_satoshis(amount)
        .payment_hash(bitcoin_sha256::Hash::from_byte_array(sha256(&[9; 32])))
        .payment_secret(PaymentSecret([10; 32]))
        .duration_since_epoch(Duration::from_secs(1_000))
        .expiry_time(Duration::from_secs(60))
        .min_final_cltv_expiry_delta(18);
    let builder = if hashed_description {
        builder.description_hash(bitcoin_sha256::Hash::from_byte_array(sha256(
            b"ordinary provider metadata",
        )))
    } else {
        builder.description("Ordinary wallet invoice".into())
    };
    builder
        .build_signed(|hash| {
            Secp256k1::new()
                .sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[11; 32]).unwrap())
        })
        .unwrap()
        .to_string()
}
#[test]
fn expired_prepared_invoice_keeps_structural_checks_for_reconciliation() {
    let text = invoice(false, Currency::Regtest, 100_000_000);
    assert!(validate_invoice(&text, 100_000, Network::Regtest, 1_061).is_err());
    validate_prepared_invoice(&text, 100_000, Network::Regtest).unwrap();
    assert!(validate_prepared_invoice(&text, 100_001, Network::Regtest).is_err());
    assert!(validate_prepared_invoice(&text, 100_000, Network::Bitcoin).is_err());
    assert!(validate_prepared_invoice("not-an-invoice", 100_000, Network::Regtest).is_err());
    assert!(
        validate_prepared_invoice(&"x".repeat(MAX_INVOICE_BYTES + 1), 1, Network::Regtest).is_err()
    );
}

#[test]
fn ordinary_invoices_accept_both_description_forms_and_exact_payment_proofs() {
    for hashed in [false, true] {
        let text = invoice(hashed, Currency::Regtest, 100_000_000);
        validate_invoice(&text, 100_000, Network::Regtest, 1_001).unwrap();
        assert!(validate_invoice(&text, 100_001, Network::Regtest, 1_001).is_err());
        assert!(validate_invoice(&text, 100_000, Network::Bitcoin, 1_001).is_err());
        assert!(validate_invoice(&text, 100_000, Network::Regtest, 1_061).is_err());
        verify_payment_preimage(&text, &[9; 32]).unwrap(); // Recovery after expiry has no clock dependency.
        assert!(verify_payment_preimage(&text, &[8; 32]).is_err());
        let mut broken = text.into_bytes();
        broken[20] = if broken[20] == b'p' { b'q' } else { b'p' };
        assert!(validate_invoice(
            std::str::from_utf8(&broken).unwrap(),
            100_000,
            Network::Regtest,
            1_001
        )
        .is_err());
    }
    assert!(validate_invoice(&"x".repeat(MAX_INVOICE_BYTES + 1), 1, Network::Regtest, 0).is_err());
}
#[test]
fn economics_escrow_and_slot_are_fixed_while_future_funding_outpoint_can_bind() {
    let (contract, terms) = fixture();
    let key = contract.contract_parameters.players[0].pubkey.serialize();
    ContractAuthorization::from_policy(&policy(&terms))
        .unwrap()
        .verify_contract(&contract, &key)
        .unwrap();
    terms.verify_preimage(&[6; 32]).unwrap();
    assert!(terms.verify_preimage(&[7; 32]).is_err());
    let mut changed = contract.clone();
    changed.funding_outpoint.vout = 1;
    terms.verify_contract(&changed, &key).unwrap();
    assert_ne!(
        contract_digest(&contract).unwrap(),
        contract_digest(&changed).unwrap()
    );
    for field in 0..8 {
        let mut changed = contract.clone();
        let p = &mut changed.contract_parameters;
        match field {
            0 => p.funding_value = Amount::from_sat(90_000),
            1 => p.fee_rate = FeeRate::from_sat_per_vb_u32(2),
            2 => p.relative_locktime_block_delta += 1,
            3 => p.players[0].payout_hash = [0; 32],
            4 => p.players[0].ticket_hash = [0; 32],
            5 => p.players.swap(0, 1),
            6 => p.event.expiry = Some(600_000),
            _ => {
                p.outcome_payouts
                    .insert(Outcome::Attestation(0), BTreeMap::from([(1, 100)]));
            }
        }
        assert!(
            terms.verify_contract(&changed, &key).is_err(),
            "mutation {field}"
        );
    }
    let mut relative_expiry = terms.clone();
    relative_expiry
        .outcome_payouts
        .insert(Outcome::Expiry, BTreeMap::from([(0, 1), (1, 1)]));
    ContractAuthorization::from_policy(&policy(&relative_expiry)).unwrap();
    let mut denied = policy(&terms);
    denied.release_entry_key_after_payment = false;
    assert!(ContractAuthorization::from_policy(&denied).is_err());
}

fn ratio_fixture(
    weights: &[u64],
    funding_sats: u64,
) -> (ContractCommitment, ContractAuthorization) {
    let (mut contract, mut terms) = fixture();
    let params = &mut contract.contract_parameters;
    params.players = weights
        .iter()
        .enumerate()
        .map(|(index, _)| Player {
            pubkey: Scalar::from_slice(&[index as u8 + 2; 32])
                .unwrap()
                .base_point_mul(),
            ticket_hash: sha256(&[index as u8 + 20; 32]),
            payout_hash: sha256(&[index as u8 + 40; 32]),
        })
        .collect();
    let payouts: PayoutWeights = weights.iter().copied().enumerate().collect();
    params.outcome_payouts = BTreeMap::from([
        (Outcome::Attestation(0), payouts.clone()),
        (Outcome::Expiry, payouts),
    ]);
    params.funding_value = Amount::from_sat(funding_sats);
    terms.player_count = params.players.len();
    terms.ticket_hash = params.players[0].ticket_hash;
    terms.payout_hash = params.players[0].payout_hash;
    terms.outcome_payouts = params.outcome_payouts.clone();
    terms.funding_value = params.funding_value;
    (contract, terms)
}

#[test]
fn equal_refund_ratios_pay_equal_amounts_for_three_and_seven_players() {
    for player_count in [3, 7] {
        let (contract, terms) = ratio_fixture(&vec![1; player_count], player_count as u64 * 1_000);
        let authorized = ContractAuthorization::from_policy(&policy(&terms)).unwrap();
        assert_eq!(authorized, terms);
        let params = &contract.contract_parameters;
        authorized
            .verify_contract(&contract, &params.players[0].pubkey.serialize())
            .unwrap();
        for outcome in [Outcome::Attestation(0), Outcome::Expiry] {
            for player in &params.players {
                assert_eq!(
                    owed_sats(params, &outcome, &player.pubkey.serialize()).unwrap(),
                    1_000
                );
            }
        }
    }
}

#[test]
fn legacy_percentage_refunds_keep_their_authorized_amounts() {
    let (contract, terms) = ratio_fixture(&[34, 33, 33], 3_000);
    assert_eq!(
        ContractAuthorization::from_policy(&policy(&terms)).unwrap(),
        terms
    );
    let params = &contract.contract_parameters;
    for outcome in [Outcome::Attestation(0), Outcome::Expiry] {
        for (player, expected) in params.players.iter().zip([1_020, 990, 990]) {
            assert_eq!(
                owed_sats(params, &outcome, &player.pubkey.serialize()).unwrap(),
                expected
            );
        }
    }
    let (changed, _) = ratio_fixture(&[1, 1, 1], 3_000);
    assert!(terms
        .verify_contract(&changed, &params.players[0].pubkey.serialize())
        .is_err());
}

#[test]
fn ratios_use_the_actual_total_and_round_down_without_overflow() {
    for (weights, funding_sats, amounts) in [
        (vec![2, 1], 100_000, vec![66_666, 33_333]),
        (vec![101, 101], 100_000, vec![50_000, 50_000]),
        (vec![u64::MAX - 1, 1], u64::MAX, vec![u64::MAX - 1, 1]),
    ] {
        let (contract, terms) = ratio_fixture(&weights, funding_sats);
        ContractAuthorization::from_policy(&policy(&terms)).unwrap();
        let params = &contract.contract_parameters;
        for outcome in [Outcome::Attestation(0), Outcome::Expiry] {
            for (player, expected) in params.players.iter().zip(&amounts) {
                assert_eq!(
                    owed_sats(params, &outcome, &player.pubkey.serialize()).unwrap(),
                    *expected
                );
            }
        }
    }
}

#[test]
fn invalid_ratio_maps_are_rejected_by_policy_and_payout_verification() {
    for outcome in [Outcome::Attestation(0), Outcome::Expiry] {
        for weights in [
            BTreeMap::new(),
            BTreeMap::from([(0, 0)]),
            BTreeMap::from([(0, 1), (1, 0)]),
            BTreeMap::from([(0, 1), (2, 1)]),
            BTreeMap::from([(0, u64::MAX), (1, 1)]),
        ] {
            let (mut contract, mut terms) = fixture();
            terms.outcome_payouts.insert(outcome, weights.clone());
            contract
                .contract_parameters
                .outcome_payouts
                .insert(outcome, weights);
            assert!(matches!(
                ContractAuthorization::from_policy(&policy(&terms)),
                Err(PayoutError::InvalidPolicy(_))
            ));
            let params = &contract.contract_parameters;
            assert!(matches!(
                owed_sats(params, &outcome, &params.players[0].pubkey.serialize()),
                Err(PayoutError::InvalidPolicy(_))
            ));
        }
    }
}

#[test]
fn signing_context_covers_expiry_and_all_adaptor_and_subset_requirements() {
    let (contract, _) = fixture();
    let requirements = signing_requirements(&contract).unwrap();
    let dlc = TicketedDLC::new(
        contract.contract_parameters.clone(),
        contract.funding_outpoint,
    )
    .unwrap();
    let data = dlc.signing_data().unwrap();
    assert_eq!(requirements.len(), data.total_signature_count());
    let requirement = |outcome| {
        &requirements
            .iter()
            .find(|(sighash, _)| *sighash == data.outcome_sighashes[&outcome])
            .unwrap()
            .1
    };
    assert_eq!(requirement(Outcome::Expiry).adaptor_point, None);
    assert_eq!(
        requirement(Outcome::Attestation(0)).adaptor_point,
        Some(data.adaptor_points[&0].serialize())
    );
    let hashes: Vec<_> = requirements.iter().map(|(sighash, _)| *sighash).collect();
    verify_contract_binding(&contract, &hashes).unwrap();
    assert!(verify_contract_binding(&contract, &hashes[1..]).is_err());
    let mut repeated = hashes;
    repeated.push(repeated[0]);
    assert!(verify_contract_binding(&contract, &repeated).is_err());
    let empty = ContractSignatures {
        expiry_tx_signature: None,
        outcome_tx_signatures: BTreeMap::new(),
        split_tx_signatures: BTreeMap::new(),
    };
    assert!(verify_completed_contract(&contract, &empty).is_err());
    assert_eq!(
        owed_sats(
            &contract.contract_parameters,
            &Outcome::Attestation(0),
            &contract.contract_parameters.players[0].pubkey.serialize()
        )
        .unwrap(),
        100_000
    );
    assert!(owed_sats(
        &contract.contract_parameters,
        &Outcome::Attestation(0),
        &contract.contract_parameters.players[1].pubkey.serialize()
    )
    .is_err());
}

/// Two outcomes ranking the same two winners in either order build one outcome transaction, so
/// the contract signs its sighash once under each outcome's adaptor point: no more, no fewer.
/// Split transactions differ by payout, so their sighashes never repeat.
#[test]
fn a_shared_outcome_transaction_is_signed_once_per_outcome() {
    let (mut contract, _) = fixture();
    let params = &mut contract.contract_parameters;
    params.event.locking_points.push(MaybePoint::Valid(
        Scalar::from_slice(&[9; 32]).unwrap().base_point_mul(),
    ));
    params.outcome_payouts = BTreeMap::from([
        (Outcome::Attestation(0), BTreeMap::from([(0, 70), (1, 30)])),
        (Outcome::Attestation(1), BTreeMap::from([(0, 30), (1, 70)])),
        (Outcome::Expiry, BTreeMap::from([(0, 50), (1, 50)])),
    ]);
    let data = TicketedDLC::new(params.clone(), contract.funding_outpoint)
        .unwrap()
        .signing_data()
        .unwrap();
    let shared = data.outcome_sighashes[&Outcome::Attestation(0)];
    assert_eq!(data.outcome_sighashes[&Outcome::Attestation(1)], shared);
    assert_ne!(data.outcome_sighashes[&Outcome::Expiry], shared);

    let requirements = signing_requirements(&contract).unwrap();
    assert_eq!(requirements.len(), data.total_signature_count());
    let points: Vec<_> = requirements
        .iter()
        .filter(|(sighash, _)| *sighash == shared)
        .map(|(_, requirement)| requirement.adaptor_point)
        .collect();
    assert_eq!(
        points,
        [0, 1].map(|index| Some(data.adaptor_points[&index].serialize()))
    );
    let splits: BTreeSet<_> = data.split_sighashes.values().collect();
    assert_eq!(splits.len(), data.split_sighashes.len());

    let hashes: Vec<_> = requirements.iter().map(|(sighash, _)| *sighash).collect();
    assert_eq!(contract_sighashes(&contract).unwrap().len(), hashes.len());
    verify_contract_binding(&contract, &hashes).unwrap();
    let mut reversed = hashes.clone();
    reversed.reverse();
    verify_contract_binding(&contract, &reversed).unwrap();
    // One signature of the shared transaction is not both outcomes'.
    let first = hashes
        .iter()
        .position(|sighash| *sighash == shared)
        .unwrap();
    let once: Vec<_> = hashes
        .iter()
        .enumerate()
        .filter(|(index, sighash)| **sighash != shared || *index == first)
        .map(|(_, sighash)| *sighash)
        .collect();
    assert_eq!(once.len(), hashes.len() - 1);
    assert!(verify_contract_binding(&contract, &once).is_err());
    // Nor is a third copy, or a copy of a split, signed in place of a missing message.
    let mut third = hashes.clone();
    third.push(shared);
    assert!(verify_contract_binding(&contract, &third).is_err());
    let mut swapped = hashes.clone();
    *swapped.last_mut().unwrap() = shared;
    assert!(verify_contract_binding(&contract, &swapped).is_err());
}

/// With no attestation a payout settles on the expiry outcome, but only once the contract
/// has expired, and never for a block-height expiry a clock cannot check.
#[test]
fn settled_outcome_is_expiry_only_after_the_contract_expires() {
    let (mut contract, _) = ratio_fixture(&[1, 1, 1], 3_000);
    let params = &mut contract.contract_parameters;
    let expiry = 1_900_000_000u32;
    params.event.expiry = Some(expiry);
    assert!(matches!(
        settled_outcome(params, None, u64::from(expiry) - 1),
        Err(PayoutError::NotExpired)
    ));
    assert_eq!(
        settled_outcome(params, None, u64::from(expiry)).unwrap(),
        Outcome::Expiry
    );
    params.event.expiry = Some(800_000);
    assert!(settled_outcome(params, None, u64::MAX).is_err());
    params.event.expiry = None;
    assert!(settled_outcome(params, None, u64::MAX).is_err());
    // An attestation still decides the outcome after the expiry.
    assert!(matches!(
        settled_outcome(params, Some(&[9; 32]), u64::MAX),
        Err(PayoutError::UnknownOutcome)
    ));
}

/// Only successes are remembered, and the least recently used is dropped past the bound.
#[test]
fn verified_contracts_remember_only_successes_within_their_bound() {
    let (contract, _) = fixture();
    let empty = ContractSignatures {
        expiry_tx_signature: None,
        outcome_tx_signatures: BTreeMap::new(),
        split_tx_signatures: BTreeMap::new(),
    };
    let verified = VerifiedContracts::default();
    for _ in 0..2 {
        assert!(verified.verify(&contract, &empty).is_err());
    }
    assert!(verified.0.lock().unwrap().is_empty());

    let key = |n: usize| ([n as u8; 32], [(n >> 8) as u8; 32]);
    for n in 0..MAX_VERIFIED_CONTRACTS {
        verified.remember(key(n));
    }
    verified.remember(key(0));
    assert!(verified.recall(&key(0)));
    verified.remember(key(MAX_VERIFIED_CONTRACTS));
    assert_eq!(verified.0.lock().unwrap().len(), MAX_VERIFIED_CONTRACTS);
    assert!(verified.recall(&key(0)));
    assert!(!verified.recall(&key(1)));
    assert!(verified.recall(&key(MAX_VERIFIED_CONTRACTS)));
}

/// The anchor and split binding are not part of the authorized economics, so a contract with or
/// without them binds, but an anchor out of bounds does not.
#[test]
fn contract_options_are_bounded_rather_than_fixed() {
    use crate::contract_options::{ContractOptions, MAX_ANCHOR_VALUE};
    let (contract, terms) = fixture();
    let key = contract.contract_parameters.players[0].pubkey.serialize();
    let mut anchored = contract.clone();
    ContractOptions::NEW.apply(&mut anchored.contract_parameters);
    terms.verify_contract(&anchored, &key).unwrap();
    anchored.contract_parameters.anchor = Some(dlctix::AnchorParams {
        value: MAX_ANCHOR_VALUE,
    });
    terms.verify_contract(&anchored, &key).unwrap();
    anchored.contract_parameters.anchor = Some(dlctix::AnchorParams {
        value: MAX_ANCHOR_VALUE + Amount::ONE_SAT,
    });
    assert!(terms.verify_contract(&anchored, &key).is_err());
}
