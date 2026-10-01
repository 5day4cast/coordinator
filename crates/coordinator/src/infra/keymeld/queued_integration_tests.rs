//! A queued competition end to end, through the production Coordinator service, the real
//! encrypted transport, Keymeld's enclave logic on two enclaves, and the Coordinator verifier.
//! Players deposit their entry keys before any pool exists; pools form from a seed; one pool's
//! session registers its members' deposits, binds the contract the oracle's statement gives, and
//! signs it at an Arkade commitment's outpoint. Deterministic enclave keys simulate custody; this
//! does not exercise Nitro attestation, KMS, an oracle server or an Arkade server.
use super::*;
use crate::infra::db::{DatabasePoolConfig, DatabaseType};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use coordinator_ark::testing::{keypair, mock_info, xonly};
use coordinator_ark::{escrow_terms, server_rules, DlcKickoff};
use coordinator_escrow::{
    authorization::ArkEscrowPolicy,
    oracle_statement::{
        LineTerms, ObservationTerms, Outcomes, RankingOutcomes, ScoringRules, SignedStatement,
        Statement, Terms,
    },
    payout,
    pools::{self, Formation, PoolRules},
    queued::{self, DepositEvidence, QueuedEntryTerms, QueuedTerms},
};
use coordinator_escrow_verifier::CoordinatorVerifier;
use dlctix::{
    bitcoin::{hashes::Hash, BlockHash, FeeRate, Network, OutPoint},
    musig2::secp256k1::{Keypair, Secp256k1, SecretKey},
    secp::Scalar,
    ContractParameters, Player,
};
use keymeld_core::{
    confidential::EnclaveEnvelope,
    protocol::{Command, EnclaveCommand, EnclaveOutcome},
    EnclaveId,
};
use keymeld_enclave::{escrow_verifier::VerifierRegistry, EnclaveOperator};
use std::collections::BTreeSet;

const ORACLE_SECRET: [u8; 32] = [7; 32];
/// The coordinator's key, which is the contract's market maker and every escrow's coordinator.
const MAKER: u8 = 18;
const START: i64 = 1_790_000_000;
const REFUND_AT: u32 = (START + 7 * 86_400) as u32;
const ENCLAVES: [u32; 2] = [1, 2];

/// Two enclaves, each running the Coordinator verifier, behind a relay that sees only
/// ciphertext, as a gateway does.
#[derive(Clone)]
struct Enclaves(Arc<BTreeMap<EnclaveId, Arc<EnclaveOperator>>>);

fn operator(id: u32) -> Arc<EnclaveOperator> {
    let operator = EnclaveOperator::with_verifiers(
        EnclaveId::new(id),
        VerifierRegistry::new(vec![Arc::new(
            CoordinatorVerifier::default().with_test_ledger(),
        )])
        .unwrap(),
    )
    .unwrap();
    operator.set_test_keys([200 + id as u8; 32]);
    Arc::new(operator)
}

async fn relay(
    State(enclaves): State<Enclaves>,
    Json(envelope): Json<EnclaveEnvelope>,
) -> Json<EnclaveEnvelope> {
    let operator = enclaves.0[&envelope.destination_enclave].clone();
    let outcome = operator
        .handle_command(Command::new(EnclaveCommand::Confidential(Box::new(
            envelope,
        ))))
        .await
        .unwrap();
    let EnclaveOutcome::Confidential(response) = outcome.response else {
        panic!("application response escaped encryption")
    };
    Json(*response)
}

async fn public_key(
    State(enclaves): State<Enclaves>,
    Path(id): Path<u32>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "enclave_id":id,"public_key":hex::encode(enclaves.0[&EnclaveId::new(id)].get_public_key()),
        "attestation_document":"","pcr_measurements":{},"timestamp":0,"healthy":true,"key_epoch":1
    }))
}

async fn list_enclaves(State(enclaves): State<Enclaves>) -> Json<serde_json::Value> {
    let listed: Vec<_> = enclaves
        .0
        .iter()
        .map(|(id, operator)| {
            serde_json::json!({
                "enclave_id":id.as_u32(),"public_key":hex::encode(operator.get_public_key()),
                "attestation_document":"","active_sessions":0,"uptime_seconds":0,
                "healthy":true,"key_epoch":1,"key_generation_time":0,"last_health_check":0
            })
        })
        .collect();
    Json(serde_json::json!({
        "total_enclaves":listed.len(),"healthy_enclaves":listed.len(),"enclaves":listed
    }))
}

fn oracle_keypair() -> Keypair {
    Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_byte_array(ORACLE_SECRET).unwrap(),
    )
}

fn queued_terms() -> QueuedTerms {
    QueuedTerms {
        competition_id: Uuid::now_v7(),
        network: Network::Regtest,
        market_maker: dlctix::MarketMaker {
            pubkey: Scalar::from_slice(&[MAKER; 32]).unwrap().base_point_mul(),
        },
        oracle_pubkey: oracle_keypair().x_only_public_key().0.to_string(),
        signing_date: START + 2 * 86_400,
        expiry: (START + 3 * 86_400) as u32,
        observation: ObservationTerms {
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
        },
        number_of_places_win: 1,
        // Four tickets form two pools of two. The verifier recomputes the pools under these
        // rules, so they are the rules the kickoff forms with.
        pool_rules: PoolRules::new(2, 3).unwrap(),
        stake_sats: 20_000,
        relative_locktime_block_delta: 72,
        max_fee_rate: FeeRate::from_sat_per_vb_u32(2),
    }
}

/// The oracle's signed statement of pool `event`'s ranking of `entries`.
fn oracle_statement(terms: &QueuedTerms, event: Uuid, entries: &[Uuid]) -> SignedStatement {
    let statement = Statement {
        event_id: event,
        signing_date: terms.signing_date,
        expiry: terms.expiry,
        nonce_point: Scalar::from_slice(&[9; 32]).unwrap().base_point_mul(),
        outcomes: Outcomes::Ranking(RankingOutcomes {
            number_of_places_win: terms.number_of_places_win,
            entry_ids: entries.to_vec(),
        }),
        terms: Terms::Observation(terms.observation.clone()),
    };
    let signature =
        Secp256k1::new().sign_schnorr_no_aux_rand(&statement.digest().unwrap(), &oracle_keypair());
    SignedStatement {
        statement,
        signature: signature.to_string(),
    }
}

/// Player `slot`'s entry key, which is both their DLC key and their escrow's player key.
fn entry_secret(slot: usize) -> [u8; 32] {
    [slot as u8 + 20; 32]
}

fn preimage(slot: usize) -> [u8; 32] {
    [slot as u8 + 60; 32]
}

/// One player's queued entry, as their wallet consented and deposited it.
struct Ticket {
    slot: usize,
    entry: QueuedEntryTerms,
    policy: PayoutPolicy,
    deposit: ParticipantRegistrationData,
}

/// Show why the enclave refused, so a refusal for the wrong reason is visible in the output.
fn refused<T>(what: &str, result: Result<T, KeymeldError>) {
    match result {
        Ok(_) => panic!("{what} was accepted"),
        Err(error) => println!("refused, {what}: {error}"),
    }
}

#[tokio::test]
async fn a_queued_pool_registers_deposits_binds_its_statement_and_signs() {
    let started = std::time::Instant::now();
    let enclaves = Enclaves(Arc::new(
        ENCLAVES
            .iter()
            .map(|id| (EnclaveId::new(*id), operator(*id)))
            .collect(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/api/v1/confidential", post(relay))
        .route("/api/v1/enclaves/{id}/public-key", get(public_key))
        .route("/api/v1/enclaves", get(list_enclaves))
        .with_state(enclaves)
        .layer(tower_http::decompression::RequestDecompressionLayer::new());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let db = DBConnection::new(
        directory.path().to_str().unwrap(),
        "queued-pool",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap();
    let settings = KeymeldSettings {
        enabled: true,
        dangerous_trust_unattested_enclaves: true,
        gateway_url: url,
        initial_polling_delay_ms: 1,
        max_polling_delay_ms: 10,
        ..Default::default()
    };
    let service =
        create_keymeld_service(settings, Uuid::now_v7(), &[MAKER; 32], db.clone()).unwrap();

    // 1. The queued competition's terms, and the deposit scope every entry is sealed under.
    let terms = queued_terms();
    terms.validate().unwrap();
    let (scope_id, digest) = queued::deposit_scope(&terms).unwrap();
    let scope = |evidence: &DepositEvidence| DepositScopeRequest {
        deposit_session_id: scope_id.clone(),
        deposit_digest: digest,
        evidence: evidence.encode().unwrap(),
    };
    let refund_evidence = DepositEvidence::Refund {
        competition_id: terms.competition_id,
    };

    // Tickets, and the pools a fixed closing block forms from them. Deposits spread over the
    // enclaves by ticket, so pick tickets whose pool under test spans both.
    let block_hash = BlockHash::from_byte_array([5; 32]);
    let (tickets, pools, pool_index) = loop {
        let tickets: Vec<Uuid> = (0..4).map(|_| Uuid::now_v7()).collect();
        let mut enclave_of = BTreeMap::new();
        for ticket in &tickets {
            let assignment = service
                .deposit_assignment(scope_id.clone(), digest, UserId::from(*ticket))
                .await
                .unwrap();
            enclave_of.insert(*ticket, assignment.enclave_id);
        }
        let Formation::Pools { pools, .. } = pools::form(
            &terms.pool_rules,
            terms.competition_id,
            &tickets,
            &block_hash,
        )
        .unwrap() else {
            panic!("four tickets form pools");
        };
        assert_eq!(pools.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2]);
        if let Some(index) = pools.iter().position(|pool| {
            pool.iter()
                .map(|ticket| enclave_of[ticket])
                .collect::<BTreeSet<_>>()
                .len()
                > 1
        }) {
            break (tickets, pools, index);
        }
    };

    // 2. Each player consents to the queued terms with an Arkade escrow, and deposits their
    // entry key through the wallet's registration path. The enclave checks each deposit alone.
    let info = mock_info(&keypair(7));
    let rules = server_rules(&info).unwrap();
    let mut by_ticket = BTreeMap::new();
    for (slot, ticket) in tickets.iter().enumerate() {
        let entry = QueuedEntryTerms {
            terms: terms.clone(),
            entry_id: *ticket,
            ticket_hash: payout::sha256(&[slot as u8 + 40; 32]),
            payout_hash: payout::sha256(&preimage(slot)),
        };
        let escrow = coordinator_ark_escrow::EntryEscrow::new(
            escrow_terms(
                &rules,
                xonly(&keypair(entry_secret(slot)[0])),
                xonly(&keypair(MAKER)),
                REFUND_AT,
                REFUND_AT - 86_400,
            )
            .unwrap(),
        )
        .unwrap();
        let policy = PayoutPolicy {
            queued_entry: Some(entry.to_json().unwrap()),
            automatic_lightning_address: None,
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            contract_terms: String::new(),
            ark_escrow: Some(ArkEscrowPolicy {
                escrow_tap_tree: hex::encode(escrow.vtxo_script().encode_tap_tree()),
                max_fee_sats: 0,
                max_refund_fee_sats: 0,
                checkpoint_exit_script: hex::encode(info.checkpoint_tapscript.as_bytes()),
            }),
        };
        let user = UserId::from(*ticket);
        let mut assignment = service
            .deposit_assignment(scope_id.clone(), digest, user.clone())
            .await
            .unwrap();
        assert_eq!(assignment.session_id, scope_id.to_string());
        assert_eq!(assignment.manifest_hash, digest.to_vec());
        assignment.payout_policy = Some(serde_json::to_string(&policy).unwrap());
        let prepared = coordinator_core::keymeld::prepare_payout_registration(
            &entry_secret(slot),
            &preimage(slot),
            &assignment,
        )
        .await
        .unwrap();
        let deposit = ParticipantRegistrationData {
            encrypted_private_key: prepared.encrypted_private_key,
            public_key: hex::encode(&prepared.context.public_key),
            auth_pubkey: prepared.auth_pubkey,
            context: prepared.context,
            payout_policy: Some(policy.clone()),
            escrow_policy: prepared.escrow_policy,
        };
        service
            .validate_deposit(scope(&refund_evidence), user, &deposit)
            .await
            .unwrap();
        by_ticket.insert(
            *ticket,
            Ticket {
                slot,
                entry,
                policy,
                deposit,
            },
        );
    }
    let used_enclaves: BTreeSet<_> = by_ticket
        .values()
        .map(|ticket| ticket.deposit.context.enclave_id)
        .collect();
    assert_eq!(used_enclaves.len(), ENCLAVES.len());

    // 3. The pool's session, as the kickoff makes it: members in ticket order, each on the
    // enclave its deposit was sealed to, with evidence of how the pool was formed.
    let mut members = pools[pool_index].clone();
    members.sort_unstable();
    let mut others = pools[1 - pool_index].clone();
    others.sort_unstable();
    let evidence = DepositEvidence::Pool {
        competition_id: terms.competition_id,
        tickets: tickets.clone(),
        block_hash,
        pool_index,
    };
    assert_eq!(
        evidence.pool_members(&terms.pool_rules).unwrap(),
        Some(members.clone())
    );
    let players: Vec<UserId> = members.iter().copied().map(UserId::from).collect();
    let session_members = |tickets: &[Uuid]| -> Vec<(UserId, EnclaveId)> {
        tickets
            .iter()
            .map(|ticket| {
                (
                    UserId::from(*ticket),
                    by_ticket[ticket].deposit.context.enclave_id,
                )
            })
            .collect()
    };
    let subsets = |players: &[UserId]| {
        crate::domain::compute_dlc_subset_definitions(
            service.coordinator_user_id(),
            players,
            terms.number_of_places_win as usize,
        )
    };
    let pool_id = Uuid::now_v7();
    let session = service
        .init_deposit_session(
            pool_id,
            scope(&evidence),
            session_members(&members),
            subsets(&players),
        )
        .await
        .unwrap();
    assert_eq!(
        session.registration_scope().unwrap(),
        (scope_id.clone(), digest.to_vec())
    );

    // 4. The pool's contract, as each member's consent and the oracle's statement give it.
    let statement = oracle_statement(&terms, pool_id, &members);
    let authorization = |ticket: &Uuid| {
        queued::pool_authorization(&by_ticket[ticket].entry, &members, &statement).unwrap()
    };
    let first = authorization(&members[0]);
    let params = ContractParameters {
        market_maker: first.market_maker.clone(),
        players: members
            .iter()
            .map(|ticket| {
                let terms = authorization(ticket);
                Player {
                    pubkey: Scalar::from_slice(&entry_secret(by_ticket[ticket].slot))
                        .unwrap()
                        .base_point_mul(),
                    ticket_hash: terms.ticket_hash,
                    payout_hash: terms.payout_hash,
                }
            })
            .collect(),
        event: first.event.clone(),
        outcome_payouts: first.outcome_payouts.clone(),
        fee_rate: first.max_fee_rate,
        funding_value: first.funding_value,
        relative_locktime_block_delta: first.relative_locktime_block_delta,
    };
    assert_eq!(
        params.funding_value.to_sat(),
        terms.stake_sats * members.len() as u64
    );
    assert_eq!(
        params.outcome_payouts,
        queued::pool_payouts(members.len(), 1).unwrap()
    );

    // A member's single-competition registration is not a deposit, so the deposit-scoped
    // session refuses it: sealed for this session, the coordinator refuses its context; sealed
    // under the deposit scope, the enclave refuses it.
    let member = &by_ticket[&members[0]];
    let single_policy = PayoutPolicy {
        queued_entry: None,
        contract_terms: serde_json::to_string(&first).unwrap(),
        ..member.policy.clone()
    };
    let for_session = service
        .get_registration_assignment(&session, players[0].clone())
        .await
        .unwrap();
    let for_scope = service
        .deposit_assignment(scope_id.clone(), digest, players[0].clone())
        .await
        .unwrap();
    for (what, mut assignment) in [
        (
            "single-competition registration for the session",
            for_session,
        ),
        ("single-competition registration under the scope", for_scope),
    ] {
        assignment.payout_policy = Some(serde_json::to_string(&single_policy).unwrap());
        let prepared = coordinator_core::keymeld::prepare_payout_registration(
            &entry_secret(member.slot),
            &preimage(member.slot),
            &assignment,
        )
        .await
        .unwrap();
        let single = ParticipantRegistrationData {
            encrypted_private_key: prepared.encrypted_private_key,
            public_key: hex::encode(&prepared.context.public_key),
            auth_pubkey: prepared.auth_pubkey,
            context: prepared.context,
            payout_policy: Some(single_policy.clone()),
            escrow_policy: prepared.escrow_policy,
        };
        refused(
            what,
            service
                .register_participant(&session, players[0].clone(), &single)
                .await,
        );
    }

    // Every member's deposit registers, and keygen completes.
    for ticket in &members {
        service
            .register_participant(&session, UserId::from(*ticket), &by_ticket[ticket].deposit)
            .await
            .unwrap();
    }
    assert!(
        service
            .get_keygen_status(&session)
            .await
            .unwrap()
            .is_completed
    );
    let roster = service.wait_for_keygen_completion(&session).await.unwrap();
    roster
        .verify_registrations(&session.authorization_manifest)
        .unwrap();
    assert_eq!(roster.roster.participants.len(), members.len() + 1);

    let policies: BTreeMap<UserId, PayoutPolicy> = members
        .iter()
        .map(|ticket| (UserId::from(*ticket), by_ticket[ticket].policy.clone()))
        .collect();
    // Arkade-funded: the pool is bound before the batch, without a funding outpoint.
    let contract = ContractCommitment {
        contract_parameters: params.clone(),
        funding_outpoint: OutPoint::null(),
    };
    // 5. A statement of the other pool's entries, even under this pool's event, is refused.
    refused(
        "statement of the other pool",
        service
            .bind_payout_contract_with_statement(
                &session,
                &contract,
                &policies,
                oracle_statement(&terms, pool_id, &others),
            )
            .await,
    );
    // So is a binding without the oracle's statement.
    refused(
        "binding without a statement",
        service
            .bind_payout_contract(&session, &contract, &policies)
            .await,
    );
    let bindings = service
        .bind_payout_contract_with_statement(&session, &contract, &policies, statement.clone())
        .await
        .unwrap();
    assert_eq!(
        bindings
            .iter()
            .map(|bound| bound.user_id.clone())
            .collect::<Vec<_>>(),
        players
    );

    // The contract is signed at the outpoint the batch's commitment transaction pays, as the
    // Arkade kickoff hook signs it, and every signature verifies.
    let pool = crate::domain::KeymeldArkPool::new(
        service.clone(),
        session.clone(),
        members
            .iter()
            .map(|ticket| {
                (
                    xonly(&keypair(entry_secret(by_ticket[ticket].slot)[0])),
                    UserId::from(*ticket),
                )
            })
            .collect(),
    );
    let hooks = DlcKickoff::new(params.clone(), pool).unwrap();
    let commitment = dlctix::bitcoin::Transaction {
        version: dlctix::bitcoin::transaction::Version::TWO,
        lock_time: dlctix::bitcoin::absolute::LockTime::ZERO,
        input: vec![dlctix::bitcoin::TxIn {
            previous_output: OutPoint::new(dlctix::bitcoin::Txid::from_byte_array([7; 32]), 0),
            ..Default::default()
        }],
        output: vec![hooks.funding_output().clone()],
    };
    let funding = OutPoint::new(commitment.compute_txid(), 0);
    let commitment = dlctix::bitcoin::Psbt::from_unsigned_tx(commitment).unwrap();
    coordinator_ark::KickoffHooks::before_forfeits(&hooks, funding, &commitment)
        .await
        .unwrap();
    let signed = hooks.signed_contract().expect("signed before the forfeits");
    assert_eq!(signed.dlc().funding_outpoint(), funding);
    payout::verify_completed_contract(
        &ContractCommitment {
            contract_parameters: params.clone(),
            funding_outpoint: funding,
        },
        &signed.all_signatures().clone(),
    )
    .unwrap();

    // A session whose players differ from the pool its evidence recomputes refuses enrollment.
    let mixed = [members[0], others[0]];
    let mixed_players: Vec<UserId> = mixed.iter().copied().map(UserId::from).collect();
    let wrong = service
        .init_deposit_session(
            Uuid::now_v7(),
            scope(&evidence),
            session_members(&mixed),
            subsets(&mixed_players),
        )
        .await
        .unwrap();
    for ticket in &mixed {
        refused(
            "session unlike its pool",
            service
                .register_participant(&wrong, UserId::from(*ticket), &by_ticket[ticket].deposit)
                .await,
        );
    }

    // 6. A ticket no pool took registers into a session made only for refunds, under which
    // nothing can be bound.
    let leftover = others[0];
    let refund_id = Uuid::now_v7();
    let refunds = service
        .init_deposit_session(
            refund_id,
            scope(&refund_evidence),
            session_members(&[leftover]),
            DlcSubsetInfo {
                definitions: vec![],
                outcome_subset_ids: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    service
        .register_participant(
            &refunds,
            UserId::from(leftover),
            &by_ticket[&leftover].deposit,
        )
        .await
        .unwrap();
    service.wait_for_keygen_completion(&refunds).await.unwrap();
    let leftover_policies =
        BTreeMap::from([(UserId::from(leftover), by_ticket[&leftover].policy.clone())]);
    let other_statement = oracle_statement(&terms, refund_id, &others);
    refused(
        "refund-only binding with a statement",
        service
            .bind_payout_contract_with_statement(
                &refunds,
                &contract,
                &leftover_policies,
                other_statement,
            )
            .await,
    );
    refused(
        "refund-only binding without a statement",
        service
            .bind_payout_contract(&refunds, &contract, &leftover_policies)
            .await,
    );

    println!(
        "queued pool of {} on {} enclaves: {:.2}s",
        members.len(),
        used_enclaves.len(),
        started.elapsed().as_secs_f64()
    );
    db.close().await.unwrap();
    server.abort();
}
