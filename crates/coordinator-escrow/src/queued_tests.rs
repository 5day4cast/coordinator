use super::*;
use crate::{
    authorization::ArkEscrowPolicy,
    oracle_statement::{LineTerms, RankingOutcomes, ScoringRules, Statement},
};
use dlctix::{
    bitcoin::hashes::Hash,
    musig2::secp256k1::{Keypair, Secp256k1, SecretKey},
    secp::{Point, Scalar},
};

const ORACLE_SECRET: [u8; 32] = [7; 32];
const START: i64 = 1_790_000_000;

fn oracle_key() -> XOnlyPublicKey {
    Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_byte_array(ORACLE_SECRET).unwrap(),
    )
    .x_only_public_key()
    .0
}

fn uuid7(n: u128) -> Uuid {
    Uuid::from_u128(0x01926f3a_0000_7000_8000_000000000000 | n)
}

fn observation() -> ObservationTerms {
    ObservationTerms {
        source: "noaa_weather".into(),
        start_observation_date: START,
        end_observation_date: START + 86_400,
        targets: vec!["KORD".into(), "KSAW".into()],
        scoring_fields: vec!["temp_high".into()],
        number_of_values_per_entry: 2,
        scoring_rules: ScoringRules::Lines,
        lines: vec![
            LineTerms {
                target: "KORD".into(),
                metric: "temp_high".into(),
                lower: -2.5,
                upper: -0.5,
                window_hours: 24,
            },
            LineTerms {
                target: "KSAW".into(),
                metric: "temp_high".into(),
                lower: -1.25,
                upper: 1.75,
                window_hours: 24,
            },
        ],
    }
}

fn terms() -> QueuedTerms {
    QueuedTerms {
        competition_id: uuid7(0xc0),
        network: Network::Signet,
        market_maker: MarketMaker {
            pubkey: Scalar::from_slice(&[3; 32]).unwrap().base_point_mul(),
        },
        oracle_pubkey: oracle_key().to_string(),
        signing_date: START + 2 * 86_400,
        expiry: (START + 3 * 86_400) as u32,
        observation: observation(),
        number_of_places_win: 1,
        pool_rules: PoolRules::new(2, 25).unwrap(),
        stake_sats: 5_000,
        relative_locktime_block_delta: 72,
        max_fee_rate: FeeRate::from_sat_per_vb_u32(10),
    }
}

fn entry(n: u128) -> QueuedEntryTerms {
    QueuedEntryTerms {
        terms: terms(),
        entry_id: uuid7(n),
        ticket_hash: [n as u8; 32],
        payout_hash: crate::payout::sha256(&[n as u8 + 100; 32]),
    }
}

fn policy(entry: &QueuedEntryTerms) -> PayoutPolicy {
    PayoutPolicy {
        automatic_lightning_address: Some("player@example.org".into()),
        allow_invoice_fallback: true,
        release_entry_key_after_payment: true,
        contract_terms: String::new(),
        ark_escrow: Some(ArkEscrowPolicy {
            escrow_tap_tree: "00".into(),
            max_fee_sats: 150,
            max_refund_fee_sats: 100,
            checkpoint_exit_script: "00".into(),
        }),
        queued_entry: Some(entry.to_json().unwrap()),
    }
}

fn statement_for(members: &[Uuid]) -> SignedStatement {
    let mut entry_ids = members.to_vec();
    entry_ids.sort_unstable();
    let statement = Statement {
        event_id: uuid7(0xe1),
        signing_date: terms().signing_date,
        expiry: terms().expiry,
        nonce_point: Scalar::from_slice(&[9; 32]).unwrap().base_point_mul(),
        outcomes: Outcomes::Ranking(RankingOutcomes {
            number_of_places_win: 1,
            entry_ids,
        }),
        terms: Terms::Observation(observation()),
    };
    crate::oracle_statement::tests::sign(statement, ORACLE_SECRET)
}

fn resign(mut signed: SignedStatement, change: impl FnOnce(&mut Statement)) -> SignedStatement {
    change(&mut signed.statement);
    crate::oracle_statement::tests::sign(signed.statement, ORACLE_SECRET)
}

#[test]
fn terms_validate_and_every_field_changes_the_digest() {
    terms().validate().unwrap();
    let digest = terms().digest().unwrap();
    assert_eq!(digest, terms().digest().unwrap());
    let changes: &[fn(&mut QueuedTerms)] = &[
        |t| t.competition_id = uuid7(0xc1),
        |t| t.stake_sats += 1,
        |t| t.expiry += 1,
        |t| t.observation.lines[0].lower = -2.0,
        |t| t.pool_rules = PoolRules::new(3, 25).unwrap(),
        |t| t.max_fee_rate = FeeRate::from_sat_per_vb_u32(11),
    ];
    for change in changes {
        let mut changed = terms();
        change(&mut changed);
        assert_ne!(changed.digest().unwrap(), digest);
    }
    let (session, scope_digest) = deposit_scope(&terms()).unwrap();
    assert_eq!(session, SessionId::from(terms().competition_id));
    assert_eq!(scope_digest, digest);

    let invalid: &[fn(&mut QueuedTerms)] = &[
        |t| t.oracle_pubkey = "zz".into(),
        |t| t.stake_sats = 0,
        |t| t.number_of_places_win = 2,
        |t| t.number_of_places_win = 0,
        |t| t.expiry = t.signing_date as u32,
        |t| t.observation.end_observation_date = t.signing_date + 1,
    ];
    for change in invalid {
        let mut changed = terms();
        change(&mut changed);
        assert!(changed.validate().is_err());
    }
}

#[test]
fn a_queued_policy_names_no_contract_and_needs_an_escrow() {
    let entry = entry(1);
    let policy = policy(&entry);
    assert_eq!(
        QueuedEntryTerms::from_policy(&policy).unwrap(),
        Some(entry.clone())
    );
    assert!(matches!(
        EntryConsent::from_policy(&policy).unwrap(),
        EntryConsent::Queued(_)
    ));
    assert!(crate::payout::ContractAuthorization::from_policy(&policy).is_err());

    let mut with_contract = policy.clone();
    with_contract.contract_terms = "{}".into();
    assert!(EntryConsent::from_policy(&with_contract).is_err());
    let mut without_escrow = policy.clone();
    without_escrow.ark_escrow = None;
    assert!(EntryConsent::from_policy(&without_escrow).is_err());
    let mut without_consent = policy.clone();
    without_consent.release_entry_key_after_payment = false;
    assert!(EntryConsent::from_policy(&without_consent).is_err());
    let mut unknown = serde_json::to_value(&entry).unwrap();
    unknown["seat"] = 3.into();
    let mut unknown_field = policy;
    unknown_field.queued_entry = Some(unknown.to_string());
    assert!(EntryConsent::from_policy(&unknown_field).is_err());
}

#[test]
fn a_policy_without_the_field_serializes_as_before() {
    let policy = PayoutPolicy {
        automatic_lightning_address: None,
        allow_invoice_fallback: true,
        release_entry_key_after_payment: true,
        contract_terms: "{}".into(),
        ark_escrow: None,
        queued_entry: None,
    };
    assert_eq!(
        serde_json::to_string(&policy).unwrap(),
        r#"{"automatic_lightning_address":null,"allow_invoice_fallback":true,"release_entry_key_after_payment":true,"contract_terms":"{}"}"#
    );
}

#[test]
fn pool_evidence_recomputes_the_members_from_the_seed() {
    let tickets: Vec<Uuid> = (1..=30).map(uuid7).collect();
    let block_hash = BlockHash::from_byte_array([5; 32]);
    let rules = PoolRules::new(2, 25).unwrap();
    let pools::Formation::Pools { pools, .. } =
        pools::form(&rules, terms().competition_id, &tickets, &block_hash).unwrap()
    else {
        panic!("30 tickets form pools");
    };
    assert_eq!(pools.len(), 2);
    for (index, pool) in pools.iter().enumerate() {
        let evidence = DepositEvidence::Pool {
            competition_id: terms().competition_id,
            tickets: tickets.iter().rev().copied().collect(),
            block_hash,
            pool_index: index,
        };
        let decoded = DepositEvidence::decode(&evidence.encode().unwrap()).unwrap();
        let mut expected = pool.clone();
        expected.sort_unstable();
        assert_eq!(decoded.pool_members(&rules).unwrap(), Some(expected));
    }
    let missing = DepositEvidence::Pool {
        competition_id: terms().competition_id,
        tickets: tickets.clone(),
        block_hash,
        pool_index: 2,
    };
    assert!(missing.pool_members(&rules).is_err());
    let too_few = DepositEvidence::Pool {
        competition_id: terms().competition_id,
        tickets: tickets[..1].to_vec(),
        block_hash,
        pool_index: 0,
    };
    assert!(too_few.pool_members(&rules).is_err());
    let refund = DepositEvidence::Refund {
        competition_id: terms().competition_id,
    };
    assert_eq!(refund.pool_members(&rules).unwrap(), None);
    assert!(DepositEvidence::decode(br#"{"kind":"pool"}"#).is_err());
}

#[test]
fn payouts_follow_the_oracle_outcome_order() {
    let payouts = pool_payouts(3, 1).unwrap();
    assert_eq!(payouts.len(), 5);
    assert_eq!(
        payouts[&Outcome::Attestation(1)],
        BTreeMap::from([(1, 100)])
    );
    let equal: PayoutWeights = (0..3).map(|index| (index, 1)).collect();
    assert_eq!(payouts[&Outcome::Attestation(3)], equal);
    assert_eq!(payouts[&Outcome::Expiry], equal);
    assert!(pool_payouts(1, 1).is_err());
    assert!(pool_payouts(26, 1).is_err());
}

#[test]
fn a_pool_member_gets_the_contract_the_statement_fixes() {
    let members = [uuid7(3), uuid7(1), uuid7(2)];
    let signed = statement_for(&members);
    let entry = entry(2);
    let terms = pool_authorization(&entry, &members, &signed).unwrap();
    assert_eq!(terms.competition_id, signed.statement.event_id);
    assert_eq!(terms.entry_id, entry.entry_id);
    assert_eq!(terms.player_index, 1);
    assert_eq!(terms.player_count, 3);
    assert_eq!(terms.funding_value, Amount::from_sat(15_000));
    assert_eq!(terms.ticket_hash, entry.ticket_hash);
    assert_eq!(terms.payout_hash, entry.payout_hash);
    assert_eq!(
        terms.event.locking_points,
        signed.statement.locking_points(oracle_key())
    );
    assert_eq!(terms.event.expiry, Some(entry.terms.expiry));
    assert_eq!(terms.outcome_payouts, pool_payouts(3, 1).unwrap());
    let mut policy_terms = policy(&entry);
    policy_terms.queued_entry = None;
    policy_terms.contract_terms = serde_json::to_string(&terms).unwrap();
    // The derived terms are valid concrete consent.
    crate::payout::ContractAuthorization::from_policy(&policy_terms).unwrap();
}

#[test]
fn a_pool_statement_that_differs_from_the_template_is_refused() {
    let members = [uuid7(1), uuid7(2), uuid7(3)];
    let entry = entry(2);
    let signed = statement_for(&members);
    pool_authorization(&entry, &members, &signed).unwrap();

    // Another key signed it.
    let forged = crate::oracle_statement::tests::sign(signed.statement.clone(), [8; 32]);
    assert!(pool_authorization(&entry, &members, &forged).is_err());
    // Changed after signing.
    let mut tampered = signed.clone();
    tampered.statement.nonce_point = Point::generator();
    assert!(pool_authorization(&entry, &members, &tampered).is_err());

    let changes: &[fn(&mut Statement)] = &[
        |s| s.event_id = uuid7(0xc0),
        |s| s.signing_date += 1,
        |s| s.expiry += 1,
        |s| match &mut s.terms {
            Terms::Observation(terms) => terms.lines[1].upper = 2.0,
        },
        |s| match &mut s.terms {
            Terms::Observation(terms) => terms.targets.reverse(),
        },
        |s| match &mut s.outcomes {
            Outcomes::Ranking(ranking) => ranking.number_of_places_win = 2,
        },
        |s| match &mut s.outcomes {
            Outcomes::Ranking(ranking) => ranking.entry_ids.swap(0, 1),
        },
        |s| match &mut s.outcomes {
            Outcomes::Ranking(ranking) => ranking.entry_ids[2] = uuid7(9),
        },
    ];
    for change in changes {
        let changed = resign(signed.clone(), change);
        assert!(pool_authorization(&entry, &members, &changed).is_err());
    }

    // Not a member, a duplicated member, or a pool below the minimum.
    assert!(pool_authorization(&self::entry(4), &members, &signed).is_err());
    let duplicated = [uuid7(1), uuid7(2), uuid7(2)];
    assert!(pool_authorization(&entry, &duplicated, &signed).is_err());
    let lonely = [uuid7(2)];
    assert!(pool_authorization(&entry, &lonely, &statement_for(&lonely)).is_err());
}
