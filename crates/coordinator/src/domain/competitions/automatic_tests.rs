//! Worker tests use real persisted competitions/contracts and a test escrow service.
//! No browser request is made after the initial entry fixtures are stored.
use super::*;
use crate::infra::{
    bitcoin_mock::MockBitcoinClient,
    db::{DBConnection, DatabasePoolConfig, DatabaseType},
    keymeld::{KeygenSessionStatus, KeymeldError, PayoutCapabilities, PayoutSecrets},
    lightning_mock::MockLnClient,
    lnurl_mock::MockLnurlPay,
    oracle_mock::MockOracle,
};
use async_trait::async_trait;
use bitcoin::{hashes::Hash, Network};
use keymeld_core::authorization::EnclaveRecipientAuthorization;
use keymeld_sdk::{
    dlctix::{dlctix::SigningData, DlcSignatureResults},
    types::{EnclaveId, SessionAuthorizationManifest, SignedSessionManifest},
    AuthorizationCredentials, SessionCredentials, SessionId,
};
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex,
};
use std::time::Duration;
use tempfile::TempDir;

fn generic_response(
    policy: &coordinator_escrow::escrow::SignedEscrowPolicy,
    operation: coordinator_escrow::escrow::protocol::Operation,
    permission: Option<&str>,
    attempt: Option<Uuid>,
    output: coordinator_escrow::escrow::protocol::Payload,
    sealed: &[u8],
) -> coordinator_escrow::escrow::protocol::EscrowResponse {
    use coordinator_escrow::escrow::{
        self,
        protocol::{EscrowResponse, Payload, ReceiptContext, RequestContext},
        ActionAttempt,
    };
    EscrowResponse::sign(
        ReceiptContext {
            schema_version: escrow::SCHEMA_VERSION,
            enclave_id: EnclaveId::new(1),
            enclave_key_epoch: 1,
            request: RequestContext {
                schema_version: escrow::SCHEMA_VERSION,
                operation,
                escrow: policy.policy.context.clone(),
                policy_digest: policy.policy.digest().unwrap(),
                request_id: Uuid::now_v7(),
                action_id: permission.map(str::to_owned),
                attempt: attempt.map(|attempt_id| ActionAttempt {
                    attempt_id,
                    signing_session_id: None,
                }),
            },
            request_digest: [1; 32],
        },
        output,
        Payload::new(sealed.to_vec()).unwrap(),
        &[6; 32],
    )
    .unwrap()
}

struct Escrow {
    invoice: Mutex<String>,
    prepare_calls: AtomicUsize,
    release_calls: AtomicUsize,
    fail_prepare: AtomicBool,
    fail_release: AtomicBool,
    claims: Mutex<Vec<Uuid>>,
    policies: Mutex<BTreeMap<UserId, coordinator_escrow::escrow::SignedEscrowPolicy>>,
    contract_digest: Mutex<String>,
    prepared: Mutex<BTreeMap<(UserId, Uuid), PayoutPreparedResponse>>,
    released: Mutex<BTreeMap<UserId, Uuid>>,
}

impl Escrow {
    fn new() -> Self {
        Self {
            invoice: Mutex::new(Self::invoice(
                [7; 32],
                Duration::from_secs(OffsetDateTime::now_utc().unix_timestamp() as u64),
            )),
            prepare_calls: AtomicUsize::new(0),
            release_calls: AtomicUsize::new(0),
            fail_prepare: AtomicBool::new(false),
            fail_release: AtomicBool::new(false),
            claims: Mutex::new(vec![]),
            policies: Mutex::new(BTreeMap::new()),
            contract_digest: Mutex::new(String::new()),
            prepared: Mutex::new(BTreeMap::new()),
            released: Mutex::new(BTreeMap::new()),
        }
    }

    fn invoice(proof: [u8; 32], timestamp: Duration) -> String {
        InvoiceBuilder::new(Currency::Regtest)
            .description("offline automatic winner".into())
            .payment_hash(bitcoin::hashes::sha256::Hash::hash(&proof))
            .payment_secret(PaymentSecret([8; 32]))
            .amount_milli_satoshis(100_000_000)
            .duration_since_epoch(timestamp)
            .min_final_cltv_expiry_delta(18)
            .build_signed(|message| {
                bitcoin::secp256k1::Secp256k1::new().sign_ecdsa_recoverable(
                    message,
                    &bitcoin::secp256k1::SecretKey::from_slice(&[9; 32]).unwrap(),
                )
            })
            .unwrap()
            .to_string()
    }
}

#[async_trait]
impl Keymeld for Escrow {
    async fn payout_capabilities(&self) -> Result<PayoutCapabilities, KeymeldError> {
        Ok(PayoutCapabilities {
            payout: true,
            lnurl: true,
        })
    }
    async fn prepare_payout(
        &self,
        _session: &DlcKeygenSession,
        user_id: UserId,
        request: PreparePayoutRequest,
    ) -> Result<PayoutPreparedResponse, KeymeldError> {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        self.claims.lock().unwrap().push(request.claim_id);
        assert!(matches!(request.method, PayoutMethod::Automatic));
        assert_eq!(request.binding_receipt, hex::encode("bound-contract"));
        assert!(!request.contract_signatures.is_empty());
        assert!(!request.attestation.is_empty());
        if let Some(response) = self
            .prepared
            .lock()
            .unwrap()
            .get(&(user_id.clone(), request.claim_id))
        {
            return Ok(response.clone());
        }
        if self
            .released
            .lock()
            .unwrap()
            .get(&user_id)
            .is_some_and(|claim| *claim != request.claim_id)
        {
            return Err(KeymeldError::Session(
                "release permission already executed".into(),
            ));
        }
        let invoice = self.invoice.lock().unwrap().clone();
        let payment_hash =
            crate::infra::lightning::extract_payment_hash_from_invoice(&invoice).unwrap();
        let policy = self.policies.lock().unwrap().get(&user_id).unwrap().clone();
        let output = coordinator_escrow::generic::PreparedSettlement {
            claim_id: request.claim_id,
            contract_digest: self.contract_digest.lock().unwrap().clone(),
            invoice_digest: coordinator_escrow::payout::invoice_digest(&invoice),
            invoice,
            payment_hash,
            owed_sats: 100_000,
        };
        let first = generic_response(
            &policy,
            coordinator_escrow::escrow::protocol::Operation::Prepare,
            Some(coordinator_escrow::generic::RELEASE_PREIMAGE),
            Some(request.claim_id),
            coordinator_escrow::escrow::protocol::Payload::encode(&output).unwrap(),
            b"preimage-preparation",
        );
        let second = generic_response(
            &policy,
            coordinator_escrow::escrow::protocol::Operation::Prepare,
            Some(coordinator_escrow::generic::RELEASE_ENTRY_KEY),
            Some(request.claim_id),
            coordinator_escrow::escrow::protocol::Payload::encode(&output).unwrap(),
            b"key-preparation",
        );
        let response = PayoutPreparedResponse::from_responses(first, second).unwrap();
        self.prepared
            .lock()
            .unwrap()
            .insert((user_id, request.claim_id), response.clone());
        if self.fail_prepare.swap(false, Ordering::SeqCst) {
            return Err(KeymeldError::Session(
                "simulated lost prepare response".into(),
            ));
        }
        Ok(response)
    }
    async fn release_payout(
        &self,
        _: &DlcKeygenSession,
        user_id: UserId,
        request: ReleasePayoutRequest,
    ) -> Result<PayoutSecrets, KeymeldError> {
        self.release_calls.fetch_add(1, Ordering::SeqCst);
        let receipts = coordinator_escrow::payout_protocol::PreparedPayoutReceipts::decode(
            &request.state_receipt,
        )
        .unwrap();
        receipts
            .verify(
                &keymeld_sdk::UserCredentials::from_private_key(&[6; 32])
                    .unwrap()
                    .public_key_bytes(),
            )
            .unwrap();
        assert_eq!(receipts.settlement().unwrap().claim_id, request.claim_id);
        let prepared = self
            .prepared
            .lock()
            .unwrap()
            .get(&(user_id.clone(), request.claim_id))
            .cloned()
            .unwrap();
        assert_eq!(prepared.state_receipt, request.state_receipt);
        coordinator_escrow::payout::verify_payment_preimage(
            &prepared.invoice,
            &hex::decode(&request.payment_preimage)
                .unwrap()
                .try_into()
                .unwrap(),
        )
        .unwrap();
        let mut released = self.released.lock().unwrap();
        if released
            .get(&user_id)
            .is_some_and(|claim| *claim != request.claim_id)
        {
            return Err(KeymeldError::Session(
                "release permission already executed".into(),
            ));
        }
        released.insert(user_id, request.claim_id);
        drop(released);
        assert!(self.claims.lock().unwrap().contains(&request.claim_id));
        if self.fail_release.swap(false, Ordering::SeqCst) {
            return Err(KeymeldError::Session("simulated enclave restart".into()));
        }
        Ok(PayoutSecrets {
            entry_private_key: hex::encode([1; 32]),
            payout_preimage: hex::encode([2; 32]),
        })
    }
    async fn bind_payout_contract(
        &self,
        _: &DlcKeygenSession,
        _: &ContractCommitment,
        _: &BTreeMap<UserId, coordinator_escrow::authorization::PayoutPolicy>,
    ) -> Result<Vec<PayoutContractBoundResponse>, KeymeldError> {
        panic!("contract was already bound")
    }
    async fn init_keygen_session(
        &self,
        _: Uuid,
        _: Vec<UserId>,
        _: DlcSubsetInfo,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        panic!("session was already created")
    }
    async fn register_participant(
        &self,
        _: &DlcKeygenSession,
        _: UserId,
        _: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        panic!("browser is offline")
    }
    async fn wait_for_keygen_completion(
        &self,
        _: &DlcKeygenSession,
    ) -> Result<SignedRoster, KeymeldError> {
        panic!("keygen already completed")
    }
    async fn get_keygen_status(
        &self,
        _: &DlcKeygenSession,
    ) -> Result<KeygenSessionStatus, KeymeldError> {
        panic!("keygen already completed")
    }
    async fn sign_dlc_batch(
        &self,
        _: &DlcKeygenSession,
        _: &SigningData,
        _: &ContractParameters,
        _: Vec<UserId>,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        panic!("contract already signed")
    }
    fn is_enabled(&self) -> bool {
        true
    }
    fn coordinator_user_id(&self) -> UserId {
        UserId::from(Uuid::from_u128(999))
    }
    async fn get_registration_assignment(
        &self,
        _: &DlcKeygenSession,
        _: UserId,
    ) -> Result<RegistrationAssignment, KeymeldError> {
        panic!("browser is offline")
    }
}

struct Fixture {
    directory: TempDir,
    database: DBConnection,
    coordinator: Coordinator,
    escrow: Arc<Escrow>,
    event_id: Uuid,
    winner: Uuid,
    loser: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = Self::open(&directory).await;
        let escrow = Arc::new(Escrow::new());
        let coordinator = Self::coordinator(database.clone(), escrow.clone()).await;
        let now = OffsetDateTime::now_utc();
        let event_id = Uuid::now_v7();
        let mut competition = Competition::new(&CreateEvent {
            id: event_id,
            signing_date: now,
            start_observation_date: now - time::Duration::hours(2),
            end_observation_date: now - time::Duration::hours(1),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 2,
            entry_fee: 50_000,
            coordinator_fee_percentage: 0,
            total_competition_pool: 100_000,
            relative_locktime_block_delta: Some(72),
            unlisted: false,
        });
        coordinator
            .competition_store
            .add_competition_with_tickets(competition.clone(), vec![])
            .await
            .unwrap();
        let winner = Uuid::now_v7();
        let loser = Uuid::now_v7();
        let tickets = [Uuid::now_v7(), Uuid::now_v7()];
        let session = session(tickets.map(UserId::from));
        let params = parameters(coordinator.private_key);
        let bound_contract = ContractCommitment {
            contract_parameters: params.clone(),
            funding_outpoint: OutPoint::null(),
        };
        *escrow.contract_digest.lock().unwrap() =
            coordinator_escrow::payout::contract_digest(&bound_contract).unwrap();
        competition.event_announcement = Some(params.event.clone());
        competition.attestation = Some(Scalar::from_slice(&[10; 32]).unwrap().into());
        competition.contract_parameters = Some(params.clone());
        competition.funding_outpoint = Some(OutPoint::null());
        competition.signed_contract = Some(sign(params, coordinator.private_key));
        competition.funding_confirmed_at = Some(now);
        coordinator
            .competition_store
            .update_competitions(vec![competition])
            .await
            .unwrap();
        for (index, (entry_id, ticket_id)) in [winner, loser].into_iter().zip(tickets).enumerate() {
            let key_byte = if index == 0 { 1 } else { 3 };
            let key = Scalar::from_slice(&[key_byte; 32])
                .unwrap()
                .base_point_mul();
            let context = RegistrationContext {
                keygen_session_id: session.session_id.clone(),
                manifest_hash: session.authorization_manifest.digest().unwrap(),
                user_id: UserId::from(ticket_id),
                enclave_id: EnclaveId::new(1),
                enclave_key_epoch: 1,
                public_key: key.serialize().to_vec(),
                auth_pubkey: vec![],
                require_signing_approval: false,
            };
            let params = &bound_contract.contract_parameters;
            let terms = ContractAuthorization {
                competition_id: event_id,
                entry_id,
                network: Network::Regtest,
                player_index: index,
                player_count: 2,
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
                automatic_lightning_address: Some("winner@example.org".into()),
                allow_invoice_fallback: true,
                release_entry_key_after_payment: true,
                contract_terms: serde_json::to_string(&terms).unwrap(),
                ark_escrow: None,
            };
            use coordinator_escrow::escrow::{
                ApplicationContext, EscrowContext, PublicKeyBytes, Recipient,
            };
            let signed = coordinator_escrow::generic::registration(
                EscrowContext {
                    keygen_session_id: context.keygen_session_id.clone(),
                    user_id: context.user_id.clone(),
                    escrow_id: entry_id,
                    manifest_digest: context.manifest_hash.as_slice().try_into().unwrap(),
                    application: ApplicationContext::commit("placeholder".into(), 1, &[]).unwrap(),
                },
                &[key_byte; 32],
                policy.clone(),
                &[key_byte + 1; 32],
                Recipient {
                    encryption_public_key: PublicKeyBytes::new(
                        &params.market_maker.pubkey.serialize(),
                    )
                    .unwrap(),
                },
            )
            .unwrap()
            .policy
            .clone();
            escrow
                .policies
                .lock()
                .unwrap()
                .insert(context.user_id.clone(), signed.clone());
            let signed_json = serde_json::to_string(&signed).unwrap();
            let submission = serde_json::to_string(&AddEventEntry {
                id: entry_id,
                event_id,
                expected_observations: vec![],
            })
            .unwrap();
            let context = serde_json::to_string(&context).unwrap();
            database.execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash, paid_at) VALUES (?, ?, 'unused', ?, datetime('now'))")
                    .bind(ticket_id.to_string()).bind(event_id.to_string()).bind(format!("ticket-{ticket_id}"))
                    .execute(&mut *tx).await?;
                sqlx::query("INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey, payout_hash, entry_submission, keymeld_registration_context, keymeld_escrow_policy) VALUES (?, ?, ?, 'offline-owner', ?, ?, ?, ?, ?)")
                    .bind(entry_id.to_string()).bind(event_id.to_string()).bind(ticket_id.to_string()).bind(key.to_string())
                    .bind(hex::encode(dlctix::hashlock::sha256(&[key_byte + 1; 32]))).bind(submission).bind(context).bind(signed_json)
                    .execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(())
            }).await.unwrap();
            coordinator
                .competition_store
                .store_entry_payout_policy(entry_id, serde_json::to_string(&policy).unwrap())
                .await
                .unwrap();
        }
        let stored = StoredDlcKeygenSession::from_session(
            &session,
            &coordinator.keymeld_storage_keys().unwrap(),
        )
        .unwrap();
        coordinator
            .competition_store
            .store_keymeld_session(event_id, &stored)
            .await
            .unwrap();
        coordinator
            .competition_store
            .enable_automatic_payouts(event_id)
            .await
            .unwrap();
        let binding_data = coordinator_escrow::generic::ContractBinding {
            contract: bound_contract,
        };
        let policies = escrow.policies.lock().unwrap().clone();
        let digests = policies
            .iter()
            .map(|(user, policy)| (user.clone(), policy.policy.digest().unwrap()))
            .collect();
        let output = coordinator_escrow::escrow::protocol::Payload::encode(
            &coordinator_escrow::escrow::protocol::BindingOutput {
                binding_data_digest: coordinator_escrow::escrow::sha256(
                    coordinator_escrow::escrow::protocol::Payload::encode(&binding_data)
                        .unwrap()
                        .as_bytes(),
                ),
                participant_policy_digests: digests,
            },
        )
        .unwrap();
        let bindings = policies
            .values()
            .map(|policy| {
                PayoutContractBoundResponse::from_response(
                    binding_data.clone(),
                    generic_response(
                        policy,
                        coordinator_escrow::escrow::protocol::Operation::Bind,
                        None,
                        None,
                        output.clone(),
                        b"bound-contract",
                    ),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        coordinator
            .competition_store
            .store_payout_contract_binding(event_id, serde_json::to_string(&bindings).unwrap())
            .await
            .unwrap();
        Self {
            directory,
            database,
            coordinator,
            escrow,
            event_id,
            winner,
            loser,
        }
    }

    async fn open(directory: &TempDir) -> DBConnection {
        DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap()
    }

    async fn coordinator(database: DBConnection, escrow: Arc<Escrow>) -> Coordinator {
        Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database),
            Arc::new(MockBitcoinClient::new(Network::Regtest)),
            Arc::new(MockLnClient::new()),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            escrow,
            None,
            72,
            1,
            "offline-worker-test".into(),
            false,
            1,
        )
        .await
        .unwrap()
        .with_automatic_payouts(true, 100)
        .unwrap()
    }

    async fn tick(&self) {
        tokio::time::timeout(
            Duration::from_secs(15),
            self.coordinator.automatic_payout_tick(),
        )
        .await
        .unwrap()
        .unwrap();
    }

    async fn ready_retries(&self) {
        self.database
            .execute_write(|pool| async move {
                sqlx::query("UPDATE payout_jobs SET retry_at = 0")
                    .execute(&pool)
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn assert_no_secrets(&self) {
        let entry = self
            .coordinator
            .competition_store
            .get_entry_by_id(self.winner)
            .await
            .unwrap()
            .unwrap();
        assert!(entry.ephemeral_privatekey.is_none());
        assert!(entry.payout_preimage.is_none());
    }
}

fn parameters(market_maker: Scalar) -> ContractParameters {
    ContractParameters {
        market_maker: dlctix::MarketMaker {
            pubkey: market_maker.base_point_mul(),
        },
        players: [1, 3]
            .into_iter()
            .map(|key| Player {
                pubkey: Scalar::from_slice(&[key; 32]).unwrap().base_point_mul(),
                ticket_hash: dlctix::hashlock::sha256(&[key + 10; 32]),
                payout_hash: dlctix::hashlock::sha256(&[key + 1; 32]),
            })
            .collect(),
        event: dlctix::EventLockingConditions {
            locking_points: vec![Scalar::from_slice(&[10; 32])
                .unwrap()
                .base_point_mul()
                .into()],
            expiry: None,
        },
        outcome_payouts: BTreeMap::from([(
            Outcome::Attestation(0),
            PayoutWeights::from([(0, 100)]),
        )]),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value: Amount::from_sat(100_000),
        relative_locktime_block_delta: 72,
    }
}

fn sign(params: ContractParameters, market_maker: Scalar) -> dlctix::SignedContract {
    let dlc = TicketedDLC::new(params, OutPoint::null()).unwrap();
    let mut rng = rand::rng();
    let mut sessions: BTreeMap<_, _> = [
        market_maker,
        Scalar::from_slice(&[1; 32]).unwrap(),
        Scalar::from_slice(&[3; 32]).unwrap(),
    ]
    .into_iter()
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
    coordinator.aggregate_all_signatures(signatures).unwrap()
}

fn session(players: [UserId; 2]) -> DlcKeygenSession {
    let coordinator = UserId::from(Uuid::from_u128(999));
    let creator = AuthorizationCredentials::from_secret(&[11; 32]).unwrap();
    let signing = AuthorizationCredentials::from_secret(&[12; 32]).unwrap();
    let credentials = SessionCredentials::from_session_secret(&[13; 32]).unwrap();
    let registrations: BTreeMap<_, _> = std::iter::once(coordinator.clone())
        .chain(players)
        .enumerate()
        .map(|(i, user)| {
            (
                user,
                AuthorizationCredentials::from_secret(&[20 + i as u8; 32]).unwrap(),
            )
        })
        .collect();
    let session_id = SessionId::new_v7();
    let manifest = SignedSessionManifest::sign(
        SessionAuthorizationManifest {
            keygen_session_id: session_id.clone(),
            coordinator_user_id: coordinator,
            creator_pubkey: creator.public_key_bytes(),
            signing_pubkey: signing.public_key_bytes(),
            session_public_key: credentials.public_key_bytes(),
            participant_verifiers: registrations
                .iter()
                .map(|(user, authority)| (user.clone(), authority.public_key_bytes()))
                .collect(),
            timeout_secs: 300,
            max_signing_sessions: None,
            encrypted_taproot_tweak: "test".into(),
            subset_definitions: vec![],
        },
        &creator.export_secret(),
    )
    .unwrap();
    let enclave = EnclaveId::new(1);
    let recipients = EnclaveRecipientAuthorization::sign(
        &manifest,
        registrations
            .keys()
            .map(|user| (user.clone(), enclave))
            .collect(),
        BTreeMap::from([(
            enclave,
            AuthorizationCredentials::from_secret(&[6; 32])
                .unwrap()
                .public_key_bytes(),
        )]),
        &creator.export_secret(),
    )
    .unwrap();
    DlcKeygenSession {
        session_id,
        session_secret: [13; 32],
        authorization_manifest: manifest,
        recipient_authorization: recipients,
        signing_authority: signing,
        registration_authorities: registrations,
        aggregate_key: vec![],
        outcome_subset_ids: BTreeMap::new(),
    }
}

#[tokio::test]
async fn finalized_winner_is_prepared_without_browser_and_loser_has_no_job() {
    let f = Fixture::new().await;
    f.tick().await;
    f.tick().await;
    let poll: (i64, i64, Option<String>) =
        sqlx::query_as("SELECT retry_at, attempts, last_error FROM payout_jobs WHERE entry_id = ?")
            .bind(f.winner.to_string())
            .fetch_one(f.database.read())
            .await
            .unwrap();
    assert!(poll.0 > OffsetDateTime::now_utc().unix_timestamp());
    assert_eq!((poll.1, poll.2), (0, None));
    f.ready_retries().await;
    let jobs = f
        .coordinator
        .competition_store
        .due_payout_jobs()
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].entry_id, f.winner);
    assert!(jobs[0].payout_id.is_some());
    assert!(!f
        .coordinator
        .competition_store
        .has_live_payout_job(f.loser)
        .await
        .unwrap());
    assert_eq!(f.escrow.prepare_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.escrow.release_calls.load(Ordering::SeqCst), 0);
    f.assert_no_secrets().await;
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn lost_preparation_response_reuses_claim_id_and_never_releases_unpaid_secrets() {
    let f = Fixture::new().await;
    f.escrow.fail_prepare.store(true, Ordering::SeqCst);
    f.tick().await;
    f.ready_retries().await;
    f.tick().await;
    let claims = f.escrow.claims.lock().unwrap().clone();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0], claims[1]);
    assert_eq!(f.escrow.release_calls.load(Ordering::SeqCst), 0);
    f.assert_no_secrets().await;
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn expired_lost_preparation_response_reconciles_before_obtaining_a_new_invoice() {
    use crate::domain::PayoutWatcher;
    use crate::infra::lightning::{Ln, PaymentNotFound};
    use tokio_util::sync::CancellationToken;

    let f = Fixture::new().await;
    *f.escrow.invoice.lock().unwrap() = Escrow::invoice([7; 32], Duration::from_secs(1));
    f.escrow.fail_prepare.store(true, Ordering::SeqCst);
    f.tick().await; // Enclave prepared the old claim, but its response was lost.
    f.ready_retries().await;
    f.tick().await; // Its cached response arrives after invoice expiry.
    let jobs = f
        .coordinator
        .competition_store
        .due_payout_jobs()
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    let old_claim = jobs[0].id;
    let old_payout = jobs[0]
        .payout_id
        .expect("expired prepared claim must enter reconciliation");
    let claims = f.escrow.claims.lock().unwrap().clone();
    assert_eq!(claims, vec![old_claim, old_claim]);
    f.assert_no_secrets().await;

    // Exercise the real sender: no LND payment exists for this expired invoice.
    // It must retire the outbox row without initiating a payment.
    let ln = Arc::new(MockLnClient::new());
    let cancel = CancellationToken::new();
    let watcher = PayoutWatcher::new(
        Arc::new(Fixture::coordinator(f.database.clone(), f.escrow.clone()).await),
        ln.clone(),
        cancel.clone(),
        Duration::from_millis(5),
    );
    let watching = tokio::spawn(async move { watcher.watch().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let payout = f
                .coordinator
                .competition_store
                .get_payout(old_payout)
                .await
                .unwrap()
                .unwrap();
            if payout.failed_at.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    watching.await.unwrap().unwrap();
    assert!(ln
        .lookup_payment(&hex::encode(dlctix::hashlock::sha256(&[7; 32])))
        .await
        .unwrap_err()
        .is::<PaymentNotFound>());

    f.tick().await; // Conclusive failure retires the old claim.
    *f.escrow.invoice.lock().unwrap() = Escrow::invoice(
        [8; 32],
        Duration::from_secs(OffsetDateTime::now_utc().unix_timestamp() as u64),
    );
    f.tick().await; // Automatic discovery can now prepare a new claim/invoice.
    let jobs = f
        .coordinator
        .competition_store
        .due_payout_jobs()
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_ne!(jobs[0].id, old_claim);
    let new_payout = jobs[0].payout_id.unwrap();
    assert_ne!(new_payout, old_payout);
    assert_eq!(f.escrow.prepare_calls.load(Ordering::SeqCst), 3);
    assert_eq!(f.escrow.release_calls.load(Ordering::SeqCst), 0);
    f.assert_no_secrets().await;
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn paid_claim_survives_worker_restart_and_enclave_retry_without_second_invoice() {
    let mut f = Fixture::new().await;
    f.tick().await;
    let jobs = f
        .coordinator
        .competition_store
        .due_payout_jobs()
        .await
        .unwrap();
    let payout_id = jobs[0].payout_id.unwrap();
    f.coordinator
        .competition_store
        .mark_payout_succeeded(
            payout_id,
            OffsetDateTime::now_utc(),
            Some(hex::encode([7; 32])),
        )
        .await
        .unwrap();
    f.assert_no_secrets().await;
    f.escrow.fail_release.store(true, Ordering::SeqCst);
    f.tick().await;
    f.assert_no_secrets().await;
    // Recovery remains valid after the on-chain payout window closes.
    f.coordinator
        .competition_store
        .close_payout_window(f.event_id)
        .await
        .unwrap();
    f.database.close().await.unwrap();
    f.database = Fixture::open(&f.directory).await;
    f.coordinator = Fixture::coordinator(f.database.clone(), f.escrow.clone()).await;
    f.ready_retries().await;
    f.tick().await;
    f.tick().await;
    assert_eq!(f.escrow.prepare_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.escrow.release_calls.load(Ordering::SeqCst), 2);
    let entry = f
        .coordinator
        .competition_store
        .get_entry_by_id(f.winner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entry.ephemeral_privatekey.as_deref(),
        Some(hex::encode([1; 32]).as_str())
    );
    assert_eq!(
        entry.payout_preimage.as_deref(),
        Some(hex::encode([2; 32]).as_str())
    );
    assert!(f
        .coordinator
        .competition_store
        .due_payout_jobs()
        .await
        .unwrap()
        .is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payouts")
        .fetch_one(f.database.read())
        .await
        .unwrap();
    assert_eq!(count, 1);
    f.database.close().await.unwrap();
}

fn event(places: usize, players: usize) -> CreateEvent {
    let now = OffsetDateTime::now_utc();
    CreateEvent {
        id: Uuid::now_v7(),
        signing_date: now + time::Duration::hours(3),
        start_observation_date: now + time::Duration::hours(1),
        end_observation_date: now + time::Duration::hours(2),
        locations: vec!["KDEN".into()],
        number_of_values_per_entry: 3,
        number_of_places_win: places,
        total_allowed_entries: players,
        entry_fee: 50_000,
        coordinator_fee_percentage: 0,
        total_competition_pool: players * 50_000,
        relative_locktime_block_delta: Some(72),
        unlisted: false,
    }
}

#[test]
fn canonical_ticket_slots_match_contract_payouts_despite_reordered_local_entry_ids() {
    for player_count in [4, 7] {
        for places in 1..=3 {
            let competition = Competition::new(&event(places, player_count));
            let mut entries: Vec<_> = (0..player_count)
                .map(|index| {
                    let key = Scalar::from_slice(&[index as u8 + 1; 32])
                        .unwrap()
                        .base_point_mul();
                    let ticket_id = Uuid::from_u128(100 + index as u128);
                    let mut entry = AddEntry {
                        id: Uuid::from_u128(1000 - index as u128),
                        ticket_id,
                        event_id: competition.id,
                        ephemeral_pubkey: key.to_string(),
                        payout_hash: hex::encode([index as u8; 32]),
                        expected_observations: vec![],
                        encrypted_keymeld_private_key: None,
                        keymeld_auth_pubkey: None,
                        keymeld_registration_context: None,
                        keymeld_escrow_policy: None,
                    }
                    .into_user_entry("owner".into());
                    entry.entry_submission.id = ticket_id;
                    entry
                })
                .collect();
            let players: Vec<_> = entries
                .iter()
                .map(|entry| Player {
                    pubkey: entry.ephemeral_pubkey.parse().unwrap(),
                    ticket_hash: [1; 32],
                    payout_hash: [2; 32],
                })
                .collect();
            // Database row order/local UUID order differs from canonical ticket order.
            entries.reverse();
            let actual = generate_payouts(&competition, &entries, &players).unwrap();
            let authorized = slot_payouts(player_count, places).unwrap();
            assert_eq!(
                authorized, actual,
                "players={player_count}, places={places}"
            );
            let refund = authorized.last_key_value().unwrap().1;
            assert_eq!(refund.len(), player_count);
            assert_eq!(refund.values().sum::<u64>(), 100);
            assert!(refund.values().max().unwrap() - refund.values().min().unwrap() <= 1);
        }
    }
}

#[tokio::test]
async fn preannounced_automatic_competition_is_persisted_atomically_without_advancing_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let database = Fixture::open(&directory).await;
    let store = CompetitionStore::new(database.clone());
    let mut competition = Competition::new(&event(1, 2));
    let announcement = parameters(Scalar::from_slice(&[9; 32]).unwrap()).event;
    competition.event_announcement = Some(announcement.clone());
    let event_id = competition.id;
    let ticket_id = Uuid::now_v7();
    let ticket = Ticket {
        id: ticket_id,
        competition_id: event_id,
        entry_id: None,
        encrypted_preimage: "encrypted".into(),
        hash: hex::encode([19; 32]),
        payment_request: None,
        invoice_expires_at: None,
        expiry: OffsetDateTime::now_utc() + time::Duration::hours(1),
        ephemeral_pubkey: None,
        reserved_by: None,
        reserved_at: None,
        paid_at: None,
        settled_at: None,
        escrow_transaction: None,
    };
    // A ticket failure after inserting the competition/marker must roll back both.
    assert!(store
        .add_competition_with_tickets_mode(
            competition.clone(),
            vec![ticket.clone(), ticket.clone()],
            true
        )
        .await
        .is_err());
    assert!(!store.has_automatic_payouts(event_id).await.unwrap());
    assert!(store.get_competition(event_id).await.is_err());
    store
        .add_competition_with_tickets_mode(competition, vec![ticket], true)
        .await
        .unwrap();
    let persisted = store.get_competition(event_id).await.unwrap();
    assert_eq!(persisted.event_announcement, Some(announcement));
    assert!(persisted.event_created_at.is_none());
    assert!(matches!(persisted.get_state(), CompetitionState::Created));
    assert!(store.has_automatic_payouts(event_id).await.unwrap());
    assert_eq!(store.ticket_ids(event_id).await.unwrap(), vec![ticket_id]);
    database.close().await.unwrap();
}

#[tokio::test]
async fn oversized_automatic_competition_is_refused_before_any_oracle_or_ticket_effect() {
    let directory = tempfile::tempdir().unwrap();
    let database = Fixture::open(&directory).await;
    let coordinator = Fixture::coordinator(database.clone(), Arc::new(Escrow::new())).await;
    // Three winning places is past the confidential admission cap, though the
    // Oracle would happily announce the event.
    let oversized = event(3, 10);
    let event_id = oversized.id;
    let error = coordinator.create_competition(oversized).await.unwrap_err();
    let Error::BadRequest(message) = &error else {
        panic!("expected a capacity rejection, got {error:?}");
    };
    assert!(message.contains("winning places"), "{message}");
    // Admission must precede every persisted or announced effect.
    let store = CompetitionStore::new(database.clone());
    assert!(store.get_competition(event_id).await.is_err());
    assert!(store.ticket_ids(event_id).await.unwrap().is_empty());
    database.close().await.unwrap();
}

#[tokio::test]
async fn supported_automatic_competition_shape_passes_admission() {
    // The gate must not reject a competition the confidential path can serve.
    let capacity = coordinator_escrow::capacity::validate_competition_capacity(2, 1).unwrap();
    assert!(capacity.signing_items <= keymeld_core::escrow::MAX_BATCH_ITEMS);
}
