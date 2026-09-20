//! Production Coordinator service and trusted DLC verifier, connected through the
//! real encrypted HTTP transport. Deterministic enclave keys simulate custody;
//! this test does not exercise Nitro attestation, KMS, or a Lightning node.
use super::*;
use crate::infra::db::{DatabasePoolConfig, DatabaseType};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use coordinator_escrow::{
    generic,
    payout::{self, ContractAuthorization},
    payout_protocol::{InvoiceAuthorizationContext, PayoutMethod, SignedInvoiceAuthorization},
};
use coordinator_escrow_verifier::CoordinatorVerifier;
use dlctix::{
    bitcoin::{hashes::Hash, Amount, FeeRate, Network, OutPoint},
    secp::Scalar,
    ContractParameters, ContractSignatures, EventLockingConditions, MarketMaker, Player,
    TicketedDLC,
};
use keymeld_core::{
    confidential::EnclaveEnvelope,
    protocol::{Command, EnclaveCommand, EnclaveOutcome},
    EnclaveId,
};
use keymeld_enclave::{escrow_verifier::VerifierRegistry, EnclaveOperator};
use keymeld_sdk::dlctix::DlcSubsetBuilder;
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use std::sync::Mutex;

#[derive(Clone)]
struct Relay {
    operator: Arc<Mutex<Arc<EnclaveOperator>>>,
    requests: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<Vec<String>>>,
}
fn operator() -> Arc<EnclaveOperator> {
    let operator = EnclaveOperator::with_verifiers(
        EnclaveId::new(1),
        VerifierRegistry::new(vec![Arc::new(CoordinatorVerifier::default())]).unwrap(),
    )
    .unwrap();
    operator.set_test_keys([15; 32]);
    Arc::new(operator)
}
async fn relay(
    State(state): State<Relay>,
    Json(envelope): Json<EnclaveEnvelope>,
) -> Json<EnclaveEnvelope> {
    state
        .requests
        .lock()
        .unwrap()
        .push(serde_json::to_string(&envelope).unwrap());
    let operator = state.operator.lock().unwrap().clone();
    let outcome = operator
        .handle_command(Command::new(EnclaveCommand::Confidential(Box::new(
            envelope,
        ))))
        .await
        .unwrap();
    let EnclaveOutcome::Confidential(response) = outcome.response else {
        panic!("application response escaped encryption")
    };
    state
        .responses
        .lock()
        .unwrap()
        .push(serde_json::to_string(&response).unwrap());
    Json(*response)
}
async fn public_key(State(state): State<Relay>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "enclave_id":1,"public_key":hex::encode(state.operator.lock().unwrap().get_public_key()),
        "attestation_document":"","pcr_measurements":{},"timestamp":0,"healthy":true,"key_epoch":1
    }))
}
async fn enclaves(State(state): State<Relay>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"enclaves":[{
        "enclave_id":1,"public_key":hex::encode(state.operator.lock().unwrap().get_public_key()),
        "attestation_document":"","active_sessions":0,"uptime_seconds":0,
        "healthy":true,"key_epoch":1,"key_generation_time":0,"last_health_check":0
    }],"total_enclaves":1,"healthy_enclaves":1}))
}
fn parameters() -> ContractParameters {
    ContractParameters {
        market_maker: MarketMaker {
            pubkey: Scalar::from_slice(&[18; 32]).unwrap().base_point_mul(),
        },
        players: vec![Player {
            pubkey: Scalar::from_slice(&[14; 32]).unwrap().base_point_mul(),
            ticket_hash: payout::sha256(&[5; 32]),
            payout_hash: payout::sha256(&[6; 32]),
        }],
        event: EventLockingConditions {
            locking_points: vec![Scalar::from_slice(&[4; 32])
                .unwrap()
                .base_point_mul()
                .into()],
            expiry: Some(500_000),
        },
        outcome_payouts: BTreeMap::from([
            (Outcome::Attestation(0), BTreeMap::from([(0, 100)])),
            (Outcome::Expiry, BTreeMap::from([(0, 100)])),
        ]),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value: Amount::from_sat(100_000),
        relative_locktime_block_delta: 72,
    }
}
fn invoice(preimage: [u8; 32]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    InvoiceBuilder::new(Currency::Regtest)
        .amount_milli_satoshis(100_000_000)
        .description("confidential service settlement".into())
        .payment_hash(dlctix::bitcoin::hashes::sha256::Hash::from_byte_array(
            payout::sha256(&preimage),
        ))
        .payment_secret(PaymentSecret([10; 32]))
        .duration_since_epoch(now)
        .expiry_time(Duration::from_secs(600))
        .min_final_cltv_expiry_delta(18)
        .build_signed(|hash| {
            dlctix::bitcoin::secp256k1::Secp256k1::new().sign_ecdsa_recoverable(
                hash,
                &dlctix::bitcoin::secp256k1::SecretKey::from_slice(&[11; 32]).unwrap(),
            )
        })
        .unwrap()
        .to_string()
}
fn settlement_request(
    session: &DlcKeygenSession,
    user: &UserId,
    terms: &ContractAuthorization,
    binding: &PayoutContractBoundResponse,
    signatures: &ContractSignatures,
    preimage: [u8; 32],
) -> PreparePayoutRequest {
    let claim_id = Uuid::now_v7();
    let invoice = invoice(preimage);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let authorization = SignedInvoiceAuthorization::sign(
        &[14; 32],
        InvoiceAuthorizationContext {
            keygen_session_id: session.session_id.clone(),
            user_id: user.clone(),
            claim_id,
            competition_id: terms.competition_id,
            entry_id: terms.entry_id,
            contract_digest: binding.contract_digest.clone(),
            invoice_digest: payout::invoice_digest(&invoice),
            amount_msat: 100_000_000,
            expires_at: now + 600,
        },
    )
    .unwrap();
    PreparePayoutRequest {
        claim_id,
        binding_receipt: binding.binding_receipt.clone(),
        contract_signatures: serde_json::to_string(signatures).unwrap(),
        attestation: hex::encode([4; 32]),
        method: PayoutMethod::Invoice {
            invoice,
            authorization,
        },
    }
}

#[tokio::test]
async fn coordinator_service_executes_confidential_dlc_and_recovers_late_paid_candidate() {
    let relay_state = Relay {
        operator: Arc::new(Mutex::new(operator())),
        requests: Default::default(),
        responses: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/api/v1/confidential", post(relay))
        .route("/api/v1/enclaves/1/public-key", get(public_key))
        .route("/api/v1/enclaves", get(enclaves))
        .with_state(relay_state.clone())
        .layer(tower_http::decompression::RequestDecompressionLayer::new());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let db = DBConnection::new(
        directory.path().to_str().unwrap(),
        "confidential-dlc",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap();
    let maker = Uuid::now_v7();
    let player = UserId::new_v7();
    let competition = Uuid::now_v7();
    let settings = KeymeldSettings {
        enabled: true,
        dangerous_trust_unattested_enclaves: true,
        gateway_url: url,
        initial_polling_delay_ms: 1,
        max_polling_delay_ms: 10,
        ..Default::default()
    };
    let service = create_keymeld_service(settings.clone(), maker, &[18; 32], db.clone()).unwrap();
    let capabilities = service.payout_capabilities().await.unwrap();
    assert!(capabilities.payout);
    assert!(!capabilities.lnurl);
    let params = parameters();
    let subsets = DlcSubsetBuilder::new(&params, UserId::from(maker), vec![player.clone()])
        .build()
        .unwrap();
    let session = service
        .init_keygen_session(
            competition,
            vec![player.clone()],
            DlcSubsetInfo {
                definitions: subsets.definitions,
                outcome_subset_ids: subsets.outcome_subset_ids,
            },
        )
        .await
        .unwrap();
    let terms = ContractAuthorization {
        competition_id: competition,
        entry_id: Uuid::now_v7(),
        network: Network::Regtest,
        player_index: 0,
        player_count: 1,
        ticket_hash: params.players[0].ticket_hash,
        payout_hash: params.players[0].payout_hash,
        market_maker: params.market_maker.clone(),
        event: params.event.clone(),
        outcome_payouts: params.outcome_payouts.clone(),
        funding_value: params.funding_value,
        relative_locktime_block_delta: 72,
        max_fee_rate: params.fee_rate,
    };
    let policy = PayoutPolicy {
        automatic_lightning_address: None,
        allow_invoice_fallback: true,
        release_entry_key_after_payment: true,
        contract_terms: serde_json::to_string(&terms).unwrap(),
    };
    let mut assignment = service
        .get_registration_assignment(&session, player.clone())
        .await
        .unwrap();
    assignment.payout_policy = Some(serde_json::to_string(&policy).unwrap());
    let prepared =
        coordinator_core::keymeld::prepare_payout_registration(&[14; 32], &[6; 32], &assignment)
            .await
            .unwrap();
    service
        .register_participant(
            &session,
            player.clone(),
            &ParticipantRegistrationData {
                encrypted_private_key: prepared.encrypted_private_key,
                public_key: hex::encode(&prepared.context.public_key),
                auth_pubkey: prepared.auth_pubkey,
                context: prepared.context,
                payout_policy: Some(policy.clone()),
                escrow_policy: prepared.escrow_policy,
            },
        )
        .await
        .unwrap();
    service.wait_for_keygen_completion(&session).await.unwrap();
    let contract = ContractCommitment {
        contract_parameters: params.clone(),
        funding_outpoint: OutPoint::null(),
    };
    let expected_policies = BTreeMap::from([(player.clone(), policy)]);
    let mut changed_contract = contract.clone();
    changed_contract.contract_parameters.fee_rate = FeeRate::from_sat_per_vb_u32(2);
    let before_rejection = relay_state.requests.lock().unwrap().len();
    assert!(service
        .bind_payout_contract(&session, &changed_contract, &expected_policies)
        .await
        .is_err());
    assert!(relay_state.requests.lock().unwrap().len() > before_rejection);
    let bindings = service
        .bind_payout_contract(&session, &contract, &expected_policies)
        .await
        .unwrap();
    assert_eq!(bindings.len(), 1);
    let binding = &bindings[0];
    assert_eq!(binding.user_id, player);
    let dlc = TicketedDLC::new(params.clone(), contract.funding_outpoint).unwrap();
    let signing = dlc.signing_data().unwrap();
    let result = service
        .sign_dlc_batch(&session, &signing, &params, vec![player.clone()])
        .await
        .unwrap();
    let signatures = ContractSignatures {
        outcome_tx_signatures: result.outcome_signatures,
        split_tx_signatures: result.split_signatures,
        expiry_tx_signature: result.expiry_signature,
    };
    payout::verify_completed_contract(&contract, &signatures).unwrap();
    let first_request =
        settlement_request(&session, &player, &terms, binding, &signatures, [9; 32]);
    let first = service
        .prepare_payout(&session, player.clone(), first_request.clone())
        .await
        .unwrap();
    let renewed_request =
        settlement_request(&session, &player, &terms, binding, &signatures, [22; 32]);
    let renewed = service
        .prepare_payout(&session, player.clone(), renewed_request)
        .await
        .unwrap();
    assert_ne!(first.invoice, renewed.invoice);
    assert_ne!(first.claim_id, renewed.claim_id);
    // Replace both enclave state and the service. The database and authenticated
    // generic receipts are the only continuity for policy, candidate and custody.
    drop(service);
    *relay_state.operator.lock().unwrap() = operator();
    let restored = create_keymeld_service(settings, maker, &[18; 32], db.clone()).unwrap();
    let wrong = ReleasePayoutRequest {
        claim_id: first.claim_id,
        state_receipt: first.state_receipt.clone(),
        payment_preimage: hex::encode([8; 32]),
    };
    assert!(restored
        .release_payout(&session, player.clone(), wrong)
        .await
        .is_err());
    let release = ReleasePayoutRequest {
        claim_id: first.claim_id,
        state_receipt: first.state_receipt.clone(),
        payment_preimage: hex::encode([9; 32]),
    };
    let released = restored
        .release_payout(&session, player.clone(), release.clone())
        .await
        .unwrap();
    assert_eq!(released.entry_private_key, hex::encode([14; 32]));
    assert_eq!(released.payout_preimage, hex::encode([6; 32]));
    let retry = restored
        .release_payout(&session, player.clone(), release)
        .await
        .unwrap();
    assert_eq!(retry.entry_private_key, released.entry_private_key);
    assert_eq!(retry.payout_preimage, released.payout_preimage);
    assert!(restored
        .release_payout(
            &session,
            player.clone(),
            ReleasePayoutRequest {
                claim_id: renewed.claim_id,
                state_receipt: renewed.state_receipt,
                payment_preimage: hex::encode([22; 32])
            }
        )
        .await
        .is_err());
    let wire = format!(
        "{:?}{:?}",
        relay_state.requests.lock().unwrap(),
        relay_state.responses.lock().unwrap()
    );
    for private in [
        generic::VERIFIER_ID.to_owned(),
        competition.to_string(),
        player.to_string(),
        first.invoice,
        hex::encode([14; 32]),
        hex::encode([6; 32]),
        hex::encode([9; 32]),
        "contract_signatures".into(),
        "policy_data".into(),
        "payment_preimage".into(),
    ] {
        assert!(
            !wire.contains(&private),
            "private application data exposed in relay envelopes"
        );
    }
    db.close().await.unwrap();
    server.abort();
}
