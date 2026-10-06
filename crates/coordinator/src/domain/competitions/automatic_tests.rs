//! Worker tests use real persisted competitions/contracts and a test escrow service.
//! No browser request is made after the initial entry fixtures are stored.
use super::*;
use crate::domain::competitions::CompetitionState;
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
                keygen_session_id: None,
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
    /// The release permission has no preparation left, as after too many renewed claims.
    preparations_exhausted: AtomicBool,
    claims: Mutex<Vec<Uuid>>,
    policies: Mutex<BTreeMap<UserId, coordinator_escrow::escrow::SignedEscrowPolicy>>,
    contract_digest: Mutex<String>,
    prepared: Mutex<BTreeMap<(UserId, Uuid), PayoutPreparedResponse>>,
    released: Mutex<BTreeMap<UserId, Uuid>>,
    /// Whether a player's browser is requesting tickets, and so asks where to register.
    browser_online: AtomicBool,
}

impl Escrow {
    fn record_release(&self, user_id: UserId, claim_id: Uuid) -> Result<(), KeymeldError> {
        let mut released = self.released.lock().unwrap();
        if released
            .get(&user_id)
            .is_some_and(|claim| *claim != claim_id)
        {
            return Err(KeymeldError::Session(
                "release permission already executed".into(),
            ));
        }
        released.insert(user_id, claim_id);
        Ok(())
    }

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
            preparations_exhausted: AtomicBool::new(false),
            claims: Mutex::new(vec![]),
            policies: Mutex::new(BTreeMap::new()),
            contract_digest: Mutex::new(String::new()),
            prepared: Mutex::new(BTreeMap::new()),
            released: Mutex::new(BTreeMap::new()),
            browser_online: AtomicBool::new(false),
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
        if self.preparations_exhausted.load(Ordering::SeqCst) {
            return Err(KeymeldError::Sdk(
                keymeld_sdk::SdkError::EscrowPreparationExhausted {
                    reason: "Release preparation candidate limit reached".into(),
                },
            ));
        }
        assert!(matches!(request.method, PayoutMethod::Automatic));
        assert_eq!(request.binding_receipt, hex::encode("bound-contract"));
        assert!(!request.contract_signatures.is_empty());
        assert!(request
            .attestation
            .as_deref()
            .is_some_and(|attestation| !attestation.is_empty()));
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
        self.record_release(user_id, request.claim_id)?;
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
        session: &DlcKeygenSession,
        user: UserId,
    ) -> Result<RegistrationAssignment, KeymeldError> {
        assert!(
            self.browser_online.load(Ordering::SeqCst),
            "browser is offline"
        );
        Ok(RegistrationAssignment {
            session_id: session.session_id.to_string(),
            user_id: user.uuid(),
            manifest_hash: vec![],
            enclave_id: 1,
            enclave_key_epoch: 1,
            enclave_public_key: "enclave".into(),
            gateway_url: "http://keymeld.invalid".into(),
            trusted_pcrs: BTreeMap::new(),
            dangerous_trust_unattested_enclaves: false,
            payout_policy: None,
        })
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
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
            total_competition_pool: 100_000,
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
                queued_entry: None,
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
            statement: None,
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
        anchor: None,
        outcome_bound_splits: false,
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
            deposit_scope: None,
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
async fn exhausted_enclave_preparations_retire_the_claim_and_the_entry_stops_claiming() {
    let f = Fixture::new().await;
    f.escrow
        .preparations_exhausted
        .store(true, Ordering::SeqCst);
    f.tick().await;
    // The claim is failed for good, with the enclave's answer, instead of being retried.
    let job: (Option<i64>, i64, Option<String>) = sqlx::query_as(
        "SELECT failed_at, attempts, last_error FROM payout_jobs WHERE entry_id = ?",
    )
    .bind(f.winner.to_string())
    .fetch_one(f.database.read())
    .await
    .unwrap();
    assert!(job.0.is_some());
    assert_eq!(job.1, 0);
    assert!(job
        .2
        .unwrap()
        .contains("Escrow preparation capacity exhausted"));
    let store = &f.coordinator.competition_store;
    assert!(store.due_payout_jobs().await.unwrap().is_empty());
    assert!(!store.has_live_payout_job(f.winner).await.unwrap());
    assert!(!store.has_unsettled_payout_jobs(f.event_id).await.unwrap());
    assert_eq!(store.store_counts().await.unwrap().payout_jobs_failed, 1);

    // Later ticks claim again only up to the cap, and then leave the entry to the chain.
    for _ in 0..MAX_AUTOMATIC_CLAIMS_PER_ENTRY + 3 {
        f.ready_retries().await;
        f.tick().await;
    }
    assert_eq!(
        store.payout_claims_used(f.winner).await.unwrap(),
        MAX_AUTOMATIC_CLAIMS_PER_ENTRY
    );
    assert_eq!(
        f.escrow.prepare_calls.load(Ordering::SeqCst) as i64,
        MAX_AUTOMATIC_CLAIMS_PER_ENTRY
    );
    assert!(store.due_payout_jobs().await.unwrap().is_empty());
    assert_eq!(
        store.store_counts().await.unwrap().payout_jobs_failed,
        MAX_AUTOMATIC_CLAIMS_PER_ENTRY
    );
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
        coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
        total_competition_pool: players * 50_000,
        relative_locktime_block_delta: Some(72),
        unlisted: false,
        scoring_rules: None,
        scoring_fields: None,
        max_entries_per_player: 1,
        contract_options: None,
    }
}

#[tokio::test]
async fn contract_generation_preserves_previously_authorized_payout_tables() {
    let f = Fixture::new().await;
    let store = &f.coordinator.competition_store;
    let mut entries = Vec::new();
    let legacy = BTreeMap::from([
        (Outcome::Attestation(0), PayoutWeights::from([(0, 100)])),
        (Outcome::Attestation(1), PayoutWeights::from([(1, 100)])),
        (
            Outcome::Attestation(2),
            PayoutWeights::from([(0, 50), (1, 50)]),
        ),
        (Outcome::Expiry, PayoutWeights::from([(0, 50), (1, 50)])),
    ]);
    for entry_id in [f.winner, f.loser] {
        entries.push(store.get_entry_by_id(entry_id).await.unwrap().unwrap());
        let json = store.entry_payout_policy(entry_id).await.unwrap().unwrap();
        let mut policy: PayoutPolicy = serde_json::from_str(&json).unwrap();
        let mut terms: ContractAuthorization =
            serde_json::from_str(&policy.contract_terms).unwrap();
        terms.outcome_payouts = legacy.clone();
        policy.contract_terms = serde_json::to_string(&terms).unwrap();
        let json = serde_json::to_string(&policy).unwrap();
        // Seed the economics accepted before upgrading; the normal store
        // intentionally refuses to replace an entry's accepted authorization.
        f.database
            .execute_write(move |pool| async move {
                sqlx::query("UPDATE entry_payout_policies SET policy_json = ? WHERE entry_id = ?")
                    .bind(json)
                    .bind(entry_id.to_string())
                    .execute(&pool)
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
    }
    assert_ne!(legacy, slot_payouts(2, 1).unwrap());
    assert_eq!(
        f.coordinator
            .accepted_outcome_payouts(f.event_id, &entries)
            .await
            .unwrap(),
        Some(legacy)
    );
}

#[tokio::test]
async fn contract_generation_rejects_mixed_or_missing_entry_payout_authorizations() {
    let f = Fixture::new().await;
    let store = &f.coordinator.competition_store;
    let entries = vec![
        store.get_entry_by_id(f.winner).await.unwrap().unwrap(),
        store.get_entry_by_id(f.loser).await.unwrap().unwrap(),
    ];
    let json = store.entry_payout_policy(f.loser).await.unwrap().unwrap();
    let mut policy: PayoutPolicy = serde_json::from_str(&json).unwrap();
    let mut terms: ContractAuthorization = serde_json::from_str(&policy.contract_terms).unwrap();
    terms.outcome_payouts = slot_payouts(2, 1).unwrap();
    policy.contract_terms = serde_json::to_string(&terms).unwrap();
    let json = serde_json::to_string(&policy).unwrap();
    let entry_id = f.loser;
    f.database
        .execute_write(move |pool| async move {
            sqlx::query("UPDATE entry_payout_policies SET policy_json = ? WHERE entry_id = ?")
                .bind(json)
                .bind(entry_id.to_string())
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(f
        .coordinator
        .accepted_outcome_payouts(f.event_id, &entries)
        .await
        .unwrap_err()
        .to_string()
        .contains("different payout tables"));

    f.database
        .execute_write(move |pool| async move {
            sqlx::query("DELETE FROM entry_payout_policies WHERE entry_id = ?")
                .bind(entry_id.to_string())
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(f
        .coordinator
        .accepted_outcome_payouts(f.event_id, &entries)
        .await
        .unwrap_err()
        .to_string()
        .contains("no accepted payout policy"));
}

#[test]
fn canonical_ticket_slots_match_contract_payouts_despite_reordered_local_entry_ids() {
    for player_count in [3, 4, 7] {
        for places in 1..=3.min(player_count - 1) {
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
            assert!(refund.values().all(|weight| *weight == 1));
            assert_eq!(authorized[&Outcome::Expiry], *refund);
            let total = refund.values().sum::<u64>();
            for weight in refund.values() {
                assert_eq!(player_count as u64 * 1_000 * weight / total, 1_000);
            }
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
        legacy_preimage_hex: "encrypted".into(),
        preimage_ciphertext: None,
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
    let mut oversized = event(3, 10);
    // A full-day window, so the capacity check is what refuses it.
    oversized.end_observation_date = oversized.start_observation_date + time::Duration::days(1);
    oversized.signing_date = oversized.end_observation_date + time::Duration::hours(1);
    let event_id = oversized.id;
    let error = coordinator.create_competition(oversized).await.unwrap_err();
    let Error::BadRequest(message) = &error else {
        panic!("expected a capacity rejection, got {error:?}");
    };
    assert!(message.contains("winning place"), "{message}");
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

/// A competition taking tickets, whose players' browsers request them.
struct TicketFixture {
    _directory: TempDir,
    coordinator: Coordinator,
    ln: Arc<MockLnClient>,
    event_id: Uuid,
}

impl TicketFixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = Fixture::open(&directory).await;
        let escrow = Arc::new(Escrow::new());
        escrow.browser_online.store(true, Ordering::SeqCst);
        let ln = Arc::new(MockLnClient::new());
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database.clone()),
            Arc::new(MockBitcoinClient::new(Network::Regtest)),
            ln.clone(),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            escrow,
            None,
            72,
            1,
            "ticket-retry-test".into(),
            false,
            1,
        )
        .await
        .unwrap()
        .with_automatic_payouts(true, 100)
        .unwrap();
        let now = OffsetDateTime::now_utc();
        let mut competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: now + time::Duration::hours(3),
            start_observation_date: now + time::Duration::hours(1),
            end_observation_date: now + time::Duration::hours(2),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 3,
            entry_fee: 50_000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
            total_competition_pool: 150_000,
            relative_locktime_block_delta: Some(72),
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
            contract_options: None,
        });
        let event_id = competition.id;
        let store = &coordinator.competition_store;
        store
            .add_competition_with_tickets(competition.clone(), vec![])
            .await
            .unwrap();
        competition.event_announcement = Some(parameters(coordinator.private_key).event);
        store.update_competitions(vec![competition]).await.unwrap();
        let tickets = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        for (index, ticket_id) in tickets.into_iter().enumerate() {
            let preimage = [40 + index as u8; 32];
            let hash = hex::encode(dlctix::hashlock::sha256(&preimage));
            database.execute_write(move |pool| async move {
                sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, ?, ?)")
                    .bind(ticket_id.to_string()).bind(event_id.to_string()).bind(hex::encode(preimage)).bind(hash)
                    .execute(&pool).await?;
                Ok(())
            }).await.unwrap();
        }
        let session = session([UserId::from(tickets[0]), UserId::from(tickets[1])]);
        let stored = StoredDlcKeygenSession::from_session(
            &session,
            &coordinator.keymeld_storage_keys().unwrap(),
        )
        .unwrap();
        store
            .store_keymeld_session(event_id, &stored)
            .await
            .unwrap();
        store.enable_automatic_payouts(event_id).await.unwrap();
        Self {
            _directory: directory,
            coordinator,
            ln,
            event_id,
        }
    }

    /// A browser's entry: its key and its payout choice.
    fn entry(key: u8) -> (BitcoinPublicKey, PayoutRegistrationRequest) {
        let secret = bitcoin::secp256k1::SecretKey::from_slice(&[key; 32]).unwrap();
        let pubkey =
            BitcoinPublicKey::new(secret.public_key(&bitcoin::secp256k1::Secp256k1::new()));
        let choice = PayoutRegistrationRequest {
            entry_id: Uuid::now_v7(),
            payout_hash: hex::encode(dlctix::hashlock::sha256(&[key + 1; 32])),
            lightning_address: None,
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
        };
        (pubkey, choice)
    }

    async fn request(
        &self,
        player: &str,
        (pubkey, choice): &(BitcoinPublicKey, PayoutRegistrationRequest),
    ) -> Result<TicketResponse, Error> {
        tokio::time::timeout(
            Duration::from_secs(15),
            self.coordinator.request_ticket_with_payout(
                player.into(),
                self.event_id,
                *pubkey,
                Some(choice.clone()),
            ),
        )
        .await
        .unwrap()
    }

    fn invoice_cancelled(&self, ticket: &TicketResponse) -> bool {
        matches!(
            self.ln.get_invoice_state(&ticket.payment_hash),
            Some(crate::infra::lightning::InvoiceState::Canceled)
        )
    }
}

#[tokio::test]
async fn a_retried_ticket_request_gets_the_same_ticket_and_invoice() {
    let f = TicketFixture::new().await;
    let entry = TicketFixture::entry(1);
    let first = f.request("alice", &entry).await.unwrap();
    // The response was lost and the browser asks again with the same entry.
    let retry = f.request("alice", &entry).await.unwrap();
    assert_eq!(retry.ticket_id, first.ticket_id);
    assert_eq!(retry.payment_request, first.payment_request);
    assert_eq!(retry.payment_hash, first.payment_hash);
    let policy = |ticket: &TicketResponse| {
        ticket
            .keymeld_registration
            .as_ref()
            .and_then(|registration| registration.payout_policy.clone())
    };
    assert!(policy(&first).is_some());
    assert_eq!(policy(&retry), policy(&first));
    assert!(!f.invoice_cancelled(&first));
    assert_eq!(
        f.ln.invoices_added(),
        1,
        "the retry issued no second invoice"
    );
}

#[tokio::test]
async fn a_retry_sent_while_the_first_request_is_answered_gets_the_same_ticket() {
    let f = TicketFixture::new().await;
    let entry = TicketFixture::entry(1);
    // The browser gave up waiting and asked again; the first request is still being answered.
    let (first, retry) = tokio::join!(f.request("alice", &entry), f.request("alice", &entry));
    let (first, retry) = (first.unwrap(), retry.unwrap());
    assert_eq!(retry.ticket_id, first.ticket_id);
    assert_eq!(retry.payment_request, first.payment_request);
    assert_eq!(retry.payment_hash, first.payment_hash);
    assert_eq!(f.ln.invoices_added(), 1, "one invoice for the one ticket");
    assert!(!f.invoice_cancelled(&first));
    let ticket = f
        .coordinator
        .competition_store
        .get_ticket(first.ticket_id)
        .await
        .unwrap();
    assert_eq!(
        ticket.payment_request.as_ref(),
        Some(&first.payment_request)
    );
    assert_eq!(ticket.reserved_by.as_deref(), Some("alice"));
}

#[tokio::test]
async fn a_retry_that_keeps_the_players_choices_keeps_the_fixed_authorization() {
    let f = TicketFixture::new().await;
    let entry = TicketFixture::entry(1);
    let first = f.request("alice", &entry).await.unwrap();
    let store = &f.coordinator.competition_store;
    let ticket = store.get_ticket(first.ticket_id).await.unwrap();
    let fixed = store
        .ticket_payout_policy(ticket.id, &ticket.hash)
        .await
        .unwrap()
        .unwrap();
    // The coordinator derives the contract terms; a retry deriving them differently, as after
    // a fee cap change, keeps those the player's registration is sealed against.
    let mut policy: PayoutPolicy = serde_json::from_str(&fixed).unwrap();
    let mut terms: ContractAuthorization = serde_json::from_str(&policy.contract_terms).unwrap();
    terms.max_fee_rate = FeeRate::from_sat_per_vb_u32(7);
    policy.contract_terms = serde_json::to_string(&terms).unwrap();
    f.coordinator
        .fix_ticket_payout_policy(&ticket, &entry.0, &policy)
        .await
        .unwrap();
    assert_eq!(
        store
            .ticket_payout_policy(ticket.id, &ticket.hash)
            .await
            .unwrap(),
        Some(fixed.clone())
    );
    // Another payout address is the player's choice, and is refused.
    let mut policy: PayoutPolicy = serde_json::from_str(&fixed).unwrap();
    policy.automatic_lightning_address = Some("mallory@example.org".into());
    assert!(matches!(
        f.coordinator
            .fix_ticket_payout_policy(&ticket, &entry.0, &policy)
            .await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn a_retry_with_another_entry_is_a_conflict_that_releases_the_ticket() {
    let f = TicketFixture::new().await;
    let first = f.request("alice", &TicketFixture::entry(1)).await.unwrap();
    // A browser that started its entry over has a new entry key and payout hash.
    let fresh = TicketFixture::entry(5);
    let refused = f.request("alice", &fresh).await.unwrap_err();
    assert!(matches!(refused, Error::Conflict(_)), "{refused:?}");
    assert!(refused.is_refusal());
    // The first invoice can no longer buy the ticket, so it is cancelled.
    assert!(f.invoice_cancelled(&first));
    let again = f.request("alice", &fresh).await.unwrap();
    assert_ne!(again.payment_hash, first.payment_hash);
    assert!(!f.invoice_cancelled(&again));
}

#[tokio::test]
async fn another_player_is_never_handed_a_reserved_ticket() {
    let f = TicketFixture::new().await;
    let alice = f.request("alice", &TicketFixture::entry(1)).await.unwrap();
    let bob = f.request("bob", &TicketFixture::entry(1)).await.unwrap();
    assert_ne!(bob.ticket_id, alice.ticket_id);
    assert_ne!(bob.payment_hash, alice.payment_hash);
    assert!(!f.invoice_cancelled(&alice));
    let retry = f.request("alice", &TicketFixture::entry(1)).await;
    assert!(matches!(retry, Err(Error::Conflict(_))));
}
