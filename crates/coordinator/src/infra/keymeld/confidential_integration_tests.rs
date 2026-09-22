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
        ark_funding: None,
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
        ark_escrow: None,
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
    // The lifecycle starts keygen only once this reports every participant registered.
    let waiting = service.get_keygen_status(&session).await.unwrap();
    assert!(!waiting.is_completed);
    assert_eq!(waiting.status, "collecting_participants");
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
    let registered = service.get_keygen_status(&session).await.unwrap();
    assert!(
        registered.is_completed,
        "a fully registered session must let the lifecycle start keygen"
    );
    assert_eq!(registered.status, "participants_registered");
    service.wait_for_keygen_completion(&session).await.unwrap();
    assert_eq!(
        service.get_keygen_status(&session).await.unwrap().status,
        "completed"
    );
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

/// Binds and signs through the confidential service, as the Arkade kickoff hook does.
struct ServiceContractSigner {
    service: Arc<dyn Keymeld>,
    session: DlcKeygenSession,
    players: Vec<UserId>,
    policies: BTreeMap<UserId, PayoutPolicy>,
}

#[async_trait]
impl coordinator_ark::ContractSigner for ServiceContractSigner {
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        _commitment_tx: &dlctix::bitcoin::Psbt,
    ) -> Result<ContractSignatures, coordinator_ark::BoxError> {
        let contract = ContractCommitment {
            contract_parameters: dlc.params().clone(),
            funding_outpoint: dlc.funding_outpoint(),
        };
        self.service
            .bind_payout_contract(&self.session, &contract, &self.policies)
            .await?;
        let result = self
            .service
            .sign_dlc_batch(
                &self.session,
                &dlc.signing_data()?,
                dlc.params(),
                self.players.clone(),
            )
            .await?;
        Ok(ContractSignatures {
            outcome_tx_signatures: result.outcome_signatures,
            split_tx_signatures: result.split_signatures,
            expiry_tx_signature: result.expiry_signature,
        })
    }
}

/// A pool of `count` players, each winning one outcome, with an expiry that splits the pot.
fn pool_parameters(count: usize) -> ContractParameters {
    let players = (0..count)
        .map(|index| Player {
            pubkey: Scalar::from_slice(&[index as u8 + 20; 32])
                .unwrap()
                .base_point_mul(),
            ticket_hash: payout::sha256(&[index as u8 + 100; 32]),
            payout_hash: payout::sha256(&[index as u8 + 150; 32]),
        })
        .collect();
    // Weights are percentages. Expiry splits the pot evenly, with the remainder to the first players.
    let mut outcome_payouts: BTreeMap<Outcome, BTreeMap<usize, u64>> = (0..count)
        .map(|index| (Outcome::Attestation(index), BTreeMap::from([(index, 100)])))
        .collect();
    let (share, remainder) = (100 / count as u64, 100 % count as u64);
    outcome_payouts.insert(
        Outcome::Expiry,
        (0..count)
            .map(|index| (index, share + u64::from((index as u64) < remainder)))
            .collect(),
    );
    ContractParameters {
        market_maker: MarketMaker {
            pubkey: Scalar::from_slice(&[18; 32]).unwrap().base_point_mul(),
        },
        players,
        event: EventLockingConditions {
            locking_points: (0..count)
                .map(|index| {
                    Scalar::from_slice(&[index as u8 + 60; 32])
                        .unwrap()
                        .base_point_mul()
                        .into()
                })
                .collect(),
            expiry: Some(3_500_000),
        },
        outcome_payouts,
        fee_rate: FeeRate::from_sat_per_vb_u32(2),
        funding_value: Amount::from_sat(20_000 * count as u64),
        relative_locktime_block_delta: 144,
    }
}

/// A running in-process Keymeld with a pool of registered players.
struct PoolHarness {
    service: Arc<dyn Keymeld>,
    session: DlcKeygenSession,
    params: ContractParameters,
    players: Vec<UserId>,
    policies: BTreeMap<UserId, PayoutPolicy>,
    db: DBConnection,
    server: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

/// Entry key `index`, which is both the player's DLC key and their escrow key.
fn entry_secret(index: usize) -> [u8; 32] {
    [index as u8 + 20; 32]
}

impl PoolHarness {
    /// Register `count` players. With `escrows`, each consents to its Arkade escrow.
    async fn start(count: usize, escrows: Option<&[coordinator_ark_escrow::EntryEscrow]>) -> Self {
        Self::start_with(pool_parameters(count), escrows).await
    }

    /// Register the players of `params`, whose keys are the entry keys from [`entry_secret`].
    async fn start_with(
        params: ContractParameters,
        escrows: Option<&[coordinator_ark_escrow::EntryEscrow]>,
    ) -> Self {
        let count = params.players.len();
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
            .with_state(relay_state)
            .layer(tower_http::decompression::RequestDecompressionLayer::new());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let db = DBConnection::new(
            directory.path().to_str().unwrap(),
            "pool",
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
        let maker = Uuid::now_v7();
        let service = create_keymeld_service(settings, maker, &[18; 32], db.clone()).unwrap();
        let competition = Uuid::now_v7();
        let players: Vec<UserId> = (0..count).map(|_| UserId::new_v7()).collect();
        let subsets = DlcSubsetBuilder::new(&params, UserId::from(maker), players.clone())
            .build()
            .unwrap();
        let session = service
            .init_keygen_session(
                competition,
                players.clone(),
                DlcSubsetInfo {
                    definitions: subsets.definitions,
                    outcome_subset_ids: subsets.outcome_subset_ids,
                },
            )
            .await
            .unwrap();
        let mut policies = BTreeMap::new();
        for (index, player) in players.iter().enumerate() {
            let terms = ContractAuthorization {
                competition_id: competition,
                entry_id: Uuid::now_v7(),
                network: Network::Regtest,
                player_index: index,
                player_count: count,
                ticket_hash: params.players[index].ticket_hash,
                payout_hash: params.players[index].payout_hash,
                market_maker: params.market_maker.clone(),
                event: params.event.clone(),
                outcome_payouts: params.outcome_payouts.clone(),
                funding_value: params.funding_value,
                relative_locktime_block_delta: params.relative_locktime_block_delta,
                max_fee_rate: params.fee_rate,
            };
            let policy = PayoutPolicy {
                automatic_lightning_address: None,
                allow_invoice_fallback: true,
                release_entry_key_after_payment: true,
                contract_terms: serde_json::to_string(&terms).unwrap(),
                ark_escrow: escrows.map(|escrows| {
                    coordinator_escrow::authorization::ArkEscrowPolicy {
                        escrow_tap_tree: hex::encode(
                            escrows[index].vtxo_script().encode_tap_tree(),
                        ),
                        max_fee_sats: 0,
                        max_refund_fee_sats: 0,
                    }
                }),
            };
            let mut assignment = service
                .get_registration_assignment(&session, player.clone())
                .await
                .unwrap();
            assignment.payout_policy = Some(serde_json::to_string(&policy).unwrap());
            let prepared = coordinator_core::keymeld::prepare_payout_registration(
                &entry_secret(index),
                &[index as u8 + 150; 32],
                &assignment,
            )
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
            policies.insert(player.clone(), policy);
        }
        service.wait_for_keygen_completion(&session).await.unwrap();
        Self {
            service,
            session,
            params,
            players,
            policies,
            db,
            server,
            _directory: directory,
        }
    }

    async fn stop(self) {
        self.db.close().await.unwrap();
        self.server.abort();
    }
}

/// How long Keymeld takes to bind and sign a whole pool inside the Arkade kickoff hook.
///
/// The kickoff must finish this within the rest of the server's session after `BatchFinalization`.
/// This runs the real confidential transport and enclave logic in process, so it excludes network latency.
///
///     KEYMELD_POOL_SIZES=3,10,25 cargo test -p coordinator --release pool_signing_fits -- --ignored --nocapture
#[tokio::test]
#[ignore = "timing measurement; run in release"]
async fn pool_signing_fits_in_the_kickoff_hook() {
    let sizes: Vec<usize> = std::env::var("KEYMELD_POOL_SIZES")
        .unwrap_or_else(|_| "3".into())
        .split(',')
        .map(|size| size.trim().parse().unwrap())
        .collect();
    for count in sizes {
        let registered = std::time::Instant::now();
        let pool = PoolHarness::start(count, None).await;
        let before_batch = registered.elapsed();
        let PoolHarness {
            service,
            session,
            params,
            players,
            policies,
            ..
        } = &pool;
        let (service, session, params, players, policies) = (
            service.clone(),
            session.clone(),
            params.clone(),
            players.clone(),
            policies.clone(),
        );

        let hooks = coordinator_ark::DlcKickoff::new(
            params.clone(),
            ServiceContractSigner {
                service: service.clone(),
                session,
                players,
                policies,
            },
        )
        .unwrap();
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

        let started = std::time::Instant::now();
        coordinator_ark::KickoffHooks::before_forfeits(&hooks, funding, &commitment)
            .await
            .unwrap();
        let in_hook = started.elapsed();
        let contract = hooks.signed_contract().unwrap();
        let signatures = contract.all_signatures();
        println!(
            "{count} players: registration and keygen {:.2}s; bind, sign, and verify in the hook {:.2}s \
             ({} outcome, {} split, expiry {})",
            before_batch.as_secs_f64(),
            in_hook.as_secs_f64(),
            signatures.outcome_tx_signatures.len(),
            signatures.split_tx_signatures.len(),
            signatures.expiry_tx_signature.is_some(),
        );
        pool.stop().await;
    }
}

/// Prints how long each Keymeld signing call takes.
struct Timed(Arc<crate::domain::KeymeldArkPool>);

#[async_trait]
impl coordinator_ark::ContractSigner for Timed {
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        commitment_tx: &dlctix::bitcoin::Psbt,
    ) -> Result<ContractSignatures, coordinator_ark::BoxError> {
        let started = std::time::Instant::now();
        let signatures = self.0.sign_contract(dlc, commitment_tx).await;
        println!("  contract: {:.2}s", started.elapsed().as_secs_f64());
        signatures
    }

    async fn keep(
        &self,
        contract: &dlctix::SignedContract,
        commitment_tx: &dlctix::bitcoin::Psbt,
    ) -> Result<(), coordinator_ark::BoxError> {
        self.0.keep(contract, commitment_tx).await
    }
}

#[async_trait]
impl coordinator_ark::EscrowSigner for Timed {
    async fn sign(
        &self,
        requests: &[coordinator_ark::SigningRequest],
    ) -> Result<Vec<dlctix::bitcoin::secp256k1::schnorr::Signature>, coordinator_ark::BoxError>
    {
        let started = std::time::Instant::now();
        let signatures = self.0.sign(requests).await;
        let kind = match requests.first().map(|request| &request.purpose) {
            Some(coordinator_ark::SigningPurpose::IntentProof) => "intent proof",
            _ => "forfeits",
        };
        println!(
            "  {kind}: {} signatures in {:.2}s",
            requests.len(),
            started.elapsed().as_secs_f64()
        );
        signatures
    }
}

/// A pool funded from Arkade escrows, with Keymeld signing every player's escrow spends and the
/// whole contract. The scripted arkd checks every intent proof and forfeit signature itself.
///
///     KEYMELD_ARK_PLAYERS=10 cargo test -p coordinator --lib keymeld_signs -- --nocapture
#[tokio::test]
async fn keymeld_signs_an_arkade_pool_kickoff() {
    use coordinator_ark::testing::{keypair, mock_info, xonly, Commitment, MockArkd};
    use coordinator_ark::{
        escrow_terms, fund_pool, server_rules, DlcKickoff, EscrowInput, KeypairSigner,
        KickoffConfig, PoolFunding,
    };

    let count: usize = std::env::var("KEYMELD_ARK_PLAYERS")
        .ok()
        .map(|count| count.parse().unwrap())
        .unwrap_or(3);
    let info = mock_info(&keypair(7));
    let rules = server_rules(&info).unwrap();
    let market_maker = keypair(18);
    let escrows: Vec<_> = (0..count)
        .map(|index| {
            let entry_key = keypair(entry_secret(index)[0]);
            let terms = escrow_terms(
                &rules,
                xonly(&entry_key),
                xonly(&market_maker),
                1_790_000_000,
                1_789_900_000,
            )
            .unwrap();
            coordinator_ark_escrow::EntryEscrow::new(terms).unwrap()
        })
        .collect();
    let harness = PoolHarness::start(count, Some(&escrows)).await;

    // Arkade-funded: the pool is bound before the batch, without a funding outpoint.
    harness
        .service
        .bind_payout_contract(
            &harness.session,
            &ContractCommitment {
                contract_parameters: harness.params.clone(),
                funding_outpoint: OutPoint::null(),
            },
            &harness.policies,
        )
        .await
        .unwrap();

    let inputs = escrows
        .into_iter()
        .enumerate()
        .map(|(index, escrow)| EscrowInput {
            escrow,
            outpoint: OutPoint::new(
                dlctix::bitcoin::Txid::from_byte_array([index as u8 + 1; 32]),
                0,
            ),
            amount: Amount::from_sat(20_000),
        })
        .collect();
    let keymeld_pool = Arc::new(crate::domain::KeymeldArkPool::new(
        harness.service.clone(),
        harness.session.clone(),
        (0..count)
            .map(|index| {
                (
                    xonly(&keypair(entry_secret(index)[0])),
                    harness.players[index].clone(),
                )
            })
            .collect(),
    ));
    let timed = Timed(keymeld_pool.clone());
    let hooks = DlcKickoff::new(harness.params.clone(), timed).unwrap();
    let pool = PoolFunding::new(inputs, hooks.funding_output().clone(), &rules, info.dust).unwrap();
    let arkd = MockArkd::new(&pool, &info, Commitment::PaysThePool);

    let started = std::time::Instant::now();
    let kickoff = fund_pool(
        &arkd,
        &info,
        &pool,
        &Timed(keymeld_pool.clone()),
        &KeypairSigner::new([market_maker]),
        &hooks,
        &KickoffConfig {
            intent_lifetime: Duration::from_secs(120),
            timeout: Duration::from_secs(120),
        },
    )
    .await
    .unwrap();
    println!(
        "{count} players: Keymeld signed the intent proof, contract and forfeits in {:.2}s",
        started.elapsed().as_secs_f64()
    );

    let contract = hooks.signed_contract().expect("signed before the forfeits");
    assert_eq!(contract.dlc().funding_outpoint(), kickoff.funding);
    let expiry = contract.expiry_tx().expect("a signed expiry transaction");
    assert_eq!(expiry.input[0].previous_output, kickoff.funding);
    assert_eq!(arkd.forfeits().len(), count);
    assert_eq!(
        keymeld_pool
            .signed_contract()
            .map(|kept| kept.dlc().funding_outpoint()),
        Some(kickoff.funding)
    );

    // A retried batch has a new commitment transaction, so the contract is signed again for the
    // new funding outpoint, in the same keygen session.
    let retry = MockArkd::new(&pool, &info, Commitment::PaysThePool).with_batch_nonce(1);
    let hooks = DlcKickoff::new(harness.params.clone(), Timed(keymeld_pool.clone())).unwrap();
    let second = fund_pool(
        &retry,
        &info,
        &pool,
        &Timed(keymeld_pool.clone()),
        &KeypairSigner::new([market_maker]),
        &hooks,
        &KickoffConfig {
            intent_lifetime: Duration::from_secs(120),
            timeout: Duration::from_secs(120),
        },
    )
    .await
    .unwrap();
    assert_ne!(second.funding, kickoff.funding);
    assert_eq!(
        hooks
            .signed_contract()
            .map(|signed| signed.dlc().funding_outpoint()),
        Some(second.funding)
    );
    harness.stop().await;
}

/// A run of [`keymeld_kicks_off_a_pool_on_mutinynet`], kept between its steps.
#[derive(serde::Serialize, serde::Deserialize)]
struct MutinynetRun {
    players: usize,
    escrow_sats: u64,
    created_at: u32,
    refund_at: u32,
    kickoff: Option<MutinynetKickoff>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MutinynetKickoff {
    batch_id: String,
    commitment_txid: String,
    funding: String,
    expiry_height: u32,
    /// Consensus hex of the contract's expiry transaction, signed by Keymeld.
    expiry_tx: String,
    seconds_in_batch: f64,
}

/// Keymeld signs a pool's kickoff on Arkade's Mutinynet server: every intent proof, the contract,
/// and every forfeit. The run is kept in `ARK_KEYMELD_STATE`, and each step is a new run:
///
/// 1. With no state, print each escrow's address. Fund each with `escrow_sats`, through `ark-swapd`.
/// 2. Once every escrow holds a VTXO, kick off the pool in the next batch.
/// 3. Once the tip reaches the contract's expiry, broadcast Keymeld's expiry transaction.
///
///     ARK_KEYMELD_STATE=run.json cargo test -p coordinator --lib keymeld_kicks_off -- --ignored --nocapture
#[tokio::test]
#[ignore = "needs Arkade's Mutinynet server and funded escrows"]
async fn keymeld_kicks_off_a_pool_on_mutinynet() {
    use coordinator_ark::testing::{keypair, xonly};
    use coordinator_ark::{
        fund_pool, ArkServer, DlcKickoff, EscrowInput, KeypairSigner, KickoffConfig, PoolFunding,
    };

    const SERVER: &str = "https://mutinynet.arkade.sh";
    const ESPLORA: &str = "https://mutinynet.com/api";
    const EXPIRY_BLOCKS: u32 = 6;

    let path = std::path::PathBuf::from(
        std::env::var("ARK_KEYMELD_STATE").expect("ARK_KEYMELD_STATE names the run's state file"),
    );
    let save = |run: &MutinynetRun| {
        std::fs::write(&path, serde_json::to_string_pretty(run).unwrap()).unwrap()
    };
    let http = reqwest::Client::new();
    let tip = || async {
        http.get(format!("{ESPLORA}/blocks/tip/height"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap()
    };
    let server = ArkServer::connect(SERVER).await.unwrap();
    let mut run: MutinynetRun = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    } else {
        let created_at = time::OffsetDateTime::now_utc().unix_timestamp() as u32;
        let run = MutinynetRun {
            players: 3,
            escrow_sats: 2_000,
            created_at,
            refund_at: created_at + 2 * 24 * 60 * 60,
            kickoff: None,
        };
        save(&run);
        run
    };
    let market_maker = keypair(18);
    let escrows: Vec<_> = (0..run.players)
        .map(|index| {
            let terms = server
                .escrow_terms(
                    xonly(&keypair(entry_secret(index)[0])),
                    xonly(&market_maker),
                    run.refund_at,
                    run.created_at,
                )
                .unwrap();
            server.entry_escrow(terms).unwrap()
        })
        .collect();

    if let Some(kickoff) = &run.kickoff {
        let tip = tip().await;
        if tip < kickoff.expiry_height {
            println!(
                "the expiry transaction is valid from height {}; the tip is {tip}",
                kickoff.expiry_height
            );
            return;
        }
        let response = http
            .post(format!("{ESPLORA}/tx"))
            .body(kickoff.expiry_tx.clone())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "broadcast failed: {status} {body}");
        println!("broadcast Keymeld's expiry transaction: https://mutinynet.com/tx/{body}");
        return;
    }

    let vtxos = server.escrow_vtxos(&escrows).await.unwrap();
    let mut inputs = Vec::new();
    for escrow in &escrows {
        let address = escrow.address(server.hrp()).unwrap().encode();
        let script = escrow.script_pubkey();
        match vtxos
            .iter()
            .find(|vtxo| vtxo.script == script && !vtxo.is_spent && !vtxo.is_swept)
        {
            Some(vtxo) => inputs.push(EscrowInput {
                escrow: escrow.clone(),
                outpoint: vtxo.outpoint,
                amount: vtxo.amount,
            }),
            None => println!("unfunded: {address} {}", run.escrow_sats),
        }
    }
    if inputs.len() < escrows.len() {
        return;
    }

    let total: Amount = inputs.iter().map(|input| input.amount).sum();
    let expiry_height = tip().await + EXPIRY_BLOCKS;
    let mut params = pool_parameters(run.players);
    params.funding_value = total;
    params.event.expiry = Some(expiry_height);
    params.relative_locktime_block_delta = 2;
    let harness = PoolHarness::start_with(params.clone(), Some(&escrows)).await;
    harness
        .service
        .bind_payout_contract(
            &harness.session,
            &ContractCommitment {
                contract_parameters: params.clone(),
                funding_outpoint: OutPoint::null(),
            },
            &harness.policies,
        )
        .await
        .unwrap();
    let keymeld_pool = Arc::new(crate::domain::KeymeldArkPool::new(
        harness.service.clone(),
        harness.session.clone(),
        (0..run.players)
            .map(|index| {
                (
                    xonly(&keypair(entry_secret(index)[0])),
                    harness.players[index].clone(),
                )
            })
            .collect(),
    ));
    let hooks = DlcKickoff::new(params, Timed(keymeld_pool.clone())).unwrap();
    let pool = PoolFunding::new(
        inputs,
        hooks.funding_output().clone(),
        server.rules(),
        server.info().dust,
    )
    .unwrap();
    println!(
        "kicking off {} escrows worth {total}, expiry at height {expiry_height}",
        run.players
    );
    let started = std::time::Instant::now();
    let kickoff = fund_pool(
        server.client(),
        server.info(),
        &pool,
        &Timed(keymeld_pool.clone()),
        &KeypairSigner::new([market_maker]),
        &hooks,
        &KickoffConfig::for_server(server.info()),
    )
    .await
    .unwrap();
    let seconds_in_batch = started.elapsed().as_secs_f64();
    let contract = keymeld_pool
        .signed_contract()
        .expect("signed before the forfeits");
    assert_eq!(contract.dlc().funding_outpoint(), kickoff.funding);
    let expiry = contract.expiry_tx().expect("a signed expiry transaction");
    println!(
        "batch {} committed https://mutinynet.com/tx/{} after {seconds_in_batch:.1}s",
        kickoff.batch_id, kickoff.commitment_txid
    );
    run.kickoff = Some(MutinynetKickoff {
        batch_id: kickoff.batch_id,
        commitment_txid: kickoff.commitment_txid.to_string(),
        funding: kickoff.funding.to_string(),
        expiry_height,
        expiry_tx: dlctix::bitcoin::consensus::encode::serialize_hex(&expiry),
        seconds_in_batch,
    });
    save(&run);
    harness.stop().await;
}
