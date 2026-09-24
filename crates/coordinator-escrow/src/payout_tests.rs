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
    let mut invalid = terms.clone();
    invalid
        .outcome_payouts
        .insert(Outcome::Attestation(0), BTreeMap::from([(0, 101)]));
    assert!(ContractAuthorization::from_policy(&policy(&invalid)).is_err());
    let mut relative_expiry = terms.clone();
    relative_expiry
        .outcome_payouts
        .insert(Outcome::Expiry, BTreeMap::from([(0, 1), (1, 1)]));
    ContractAuthorization::from_policy(&policy(&relative_expiry)).unwrap();
    let mut denied = policy(&terms);
    denied.release_entry_key_after_payment = false;
    assert!(ContractAuthorization::from_policy(&denied).is_err());
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
    assert_eq!(
        requirements[&data.outcome_sighashes[&Outcome::Expiry]].adaptor_point,
        None
    );
    assert_eq!(
        requirements[&data.outcome_sighashes[&Outcome::Attestation(0)]].adaptor_point,
        Some(data.adaptor_points[&0].serialize())
    );
    let hashes: Vec<_> = requirements.keys().copied().collect();
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
