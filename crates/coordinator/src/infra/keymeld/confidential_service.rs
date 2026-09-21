//! Coordinator-owned orchestration over Keymeld's opaque relay. No application
//! policy, roster, signing message, receipt or error goes to a gateway API.
use super::confidential_store::{
    self, DurableCheckpoint, ProtocolState, SettlementPlan, SigningPlan,
};
use super::*;
use coordinator_escrow::{generic, payout_protocol::PreparedPayoutReceipts};
use keymeld_core::{
    authorization::{authorization_digest, SessionAuthorizationManifest},
    escrow::{
        self,
        protocol::{
            BindEscrowRequest, EscrowCommand, EscrowResponse, ExecuteEscrowRequest,
            ExecutionOutput, Operation, Payload, PrepareEscrowRequest, RequestContext,
        },
        ActionAttempt, ConditionProof,
    },
    protocol::{
        Command, EnclaveCommand, EnclaveOutcome, KeygenCommand, KeygenOutcome, MusigCommand,
        MusigOutcome, ParticipantRegistrationData as NativeRegistration, SystemCommand,
        SystemOutcome,
    },
};
use keymeld_sdk::{
    confidential::ConfidentialTransport,
    confidential_scope::scope_for_participant,
    confidential_session::{ConfidentialJournal, ConfidentialSession},
};
use std::collections::BTreeSet;
use tokio::sync::{Mutex, OwnedMutexGuard};
use zeroize::Zeroizing;

fn invalid(message: impl Into<String>) -> KeymeldError {
    KeymeldError::Session(message.into())
}
fn encoded<T: Serialize>(value: &T) -> Result<Vec<u8>, KeymeldError> {
    serde_json::to_vec(value).map_err(|error| invalid(error.to_string()))
}
fn decode_hex(value: &str) -> Result<Vec<u8>, KeymeldError> {
    hex::decode(value).map_err(|_| invalid("Invalid protocol hex encoding"))
}

impl KeymeldService {
    fn database(&self) -> Result<&DBConnection, KeymeldError> {
        self.db
            .as_ref()
            .ok_or_else(|| invalid("Confidential signing requires durable application storage"))
    }
    async fn lock_session(&self, id: &SessionId) -> OwnedMutexGuard<()> {
        let mut locks = self.session_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = locks
            .get(id)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(id.clone(), Arc::downgrade(&lock));
                lock
            });
        drop(locks);
        lock.lock_owned().await
    }
    fn storage_keys(&self) -> Result<Keys, KeymeldError> {
        let secret = Zeroizing::new(self.user_credentials.private_key_bytes());
        Ok(Keys::new(
            nostr::SecretKey::from_slice(secret.as_ref())
                .map_err(|_| invalid("Invalid storage credentials"))?,
        ))
    }
    async fn checkpoint(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<(ProtocolState, DurableCheckpoint), KeymeldError> {
        session.validate_credentials()?;
        let (version, state) =
            confidential_store::load(self.database()?, &self.store_key, &session.session_id)
                .await?
                .ok_or_else(|| {
                    invalid(
                        "No confidential protocol checkpoint exists; fresh enrollment is required",
                    )
                })?;
        if state.session.authorization_manifest.digest()?
            != session.authorization_manifest.digest()?
            || state.session.recipient_authorization != session.recipient_authorization
        {
            return Err(invalid(
                "Confidential checkpoint differs from the pinned session",
            ));
        }
        let checkpoint = DurableCheckpoint::new(
            self.database()?.clone(),
            self.store_key.clone(),
            session.session_id.clone(),
            version,
            state.clone(),
        );
        Ok((state, checkpoint))
    }
    async fn connect<'a>(
        &'a self,
        session: &'a DlcKeygenSession,
        state: &ProtocolState,
        credentials: &'a SessionCredentials,
        journal: &'a mut ConfidentialJournal,
        checkpoint: &'a DurableCheckpoint,
    ) -> Result<ConfidentialSession<'a>, KeymeldError> {
        Ok(ConfidentialSession::connect(
            self.get_client()?,
            &session.authorization_manifest,
            &session.recipient_authorization,
            &state.epochs,
            credentials,
            &session.signing_authority,
            &session.signing_authority,
            journal,
            checkpoint,
        )
        .await?)
    }
    fn native_registration(
        session: &DlcKeygenSession,
        user: &UserId,
        data: &ParticipantRegistrationData,
    ) -> Result<NativeRegistration, KeymeldError> {
        session.validate_registration(user, data)?;
        coordinator_core::keymeld::verify_registration_policy(
            &data.context,
            data.payout_policy.as_ref(),
            data.escrow_policy.as_ref(),
        )?;
        let authority = session
            .registration_authorities
            .get(user)
            .ok_or_else(|| invalid("Missing registration authority"))?;
        let secret = Zeroizing::new(authority.export_secret());
        Ok(NativeRegistration {
            user_id: user.clone(),
            registration_authorization: RegistrationAuthorization::sign(
                &secret,
                data.context.clone(),
                &data.encrypted_private_key,
            )?,
            enclave_encrypted_data: data.encrypted_private_key.clone(),
            auth_pubkey: data.context.auth_pubkey.clone(),
            require_signing_approval: false,
        })
    }
}

/// The outer confidential response authenticates the exact native request. The
/// inner signed receipt independently binds its issuer, policy, operation and
/// attempt, and remains verifiable when stored by the payout worker.
#[allow(clippy::too_many_arguments)]
async fn escrow_request<T: Serialize>(
    driver: &mut ConfidentialSession<'_>,
    session: &DlcKeygenSession,
    state: &ProtocolState,
    credentials: &SessionCredentials,
    user: &UserId,
    stage: &str,
    operation: Operation,
    action: Option<&str>,
    attempt: Option<ActionAttempt>,
    request: &T,
) -> Result<EscrowResponse, KeymeldError> {
    let policy = state
        .policies
        .get(user)
        .ok_or_else(|| invalid("Participant has no accepted escrow consent"))?;
    let enclave = *session
        .recipient_authorization
        .user_enclave_assignments
        .get(user)
        .ok_or_else(|| invalid("Participant enclave is missing"))?;
    // Only an authenticated terminal rejection permits a fresh request identity.
    // Unknown delivery outcomes must retain the original command and ciphertext.
    if driver.command_was_rejected(stage, enclave) {
        driver.clear_rejected_command(stage, enclave).await?;
    }
    let context = RequestContext {
        schema_version: escrow::SCHEMA_VERSION,
        operation,
        escrow: policy.policy.context.clone(),
        policy_digest: policy.policy.digest()?,
        request_id: Uuid::nil(), // Template identity is excluded from semantic retries.
        action_id: action.map(str::to_owned),
        attempt,
    };
    let plaintext = Zeroizing::new(encoded(request)?);
    let input = (&context, &*plaintext);
    let authority = Zeroizing::new(session.signing_authority.export_secret());
    let outcome = driver
        .command_once(stage, enclave, &input, || {
            let encrypted = credentials
                .session_secret()
                .encrypt(&plaintext, "escrow-request-v1")?;
            let mut fresh_context = context.clone();
            fresh_context.request_id = Uuid::now_v7();
            let command = EscrowCommand::sign(
                fresh_context,
                Payload::new(encrypted.to_bytes()?)?,
                &authority,
            )?;
            Ok(EnclaveCommand::Musig(MusigCommand::Keygen(
                KeygenCommand::Escrow(command),
            )))
        })
        .await?;
    let EnclaveOutcome::Musig(MusigOutcome::Keygen(KeygenOutcome::Escrow(response))) = outcome
    else {
        return Err(invalid("Unexpected generic escrow response"));
    };
    let Some(Command {
        command: EnclaveCommand::Musig(MusigCommand::Keygen(KeygenCommand::Escrow(accepted))),
        ..
    }) = driver.recorded_command(stage, enclave)
    else {
        return Err(invalid("Missing authenticated escrow request checkpoint"));
    };
    if response.context.request != accepted.context
        || response.context.request_digest != accepted.digest()?
        || response.context.enclave_id != enclave
        || state.epochs.get(&enclave) != Some(&response.context.enclave_key_epoch)
    {
        return Err(invalid(
            "Generic escrow response differs from its accepted request",
        ));
    }
    response.verify(
        &response.context,
        &session.recipient_authorization.recipient_public_keys[&enclave],
    )?;
    Ok(*response)
}

#[async_trait]
impl Keymeld for KeymeldService {
    fn is_enabled(&self) -> bool {
        self.settings.enabled && self.client.is_some()
    }
    fn coordinator_user_id(&self) -> UserId {
        self.coordinator_user_id.clone()
    }

    async fn payout_capabilities(&self) -> Result<PayoutCapabilities, KeymeldError> {
        if !self.is_enabled() {
            return Ok(PayoutCapabilities::default());
        }
        let client = self.get_client()?;
        let enclaves = client.health().list_enclaves().await?;
        if enclaves.enclaves.is_empty() {
            return Ok(PayoutCapabilities::default());
        }
        let transport = ConfidentialTransport::new(client);
        let authority = AuthorizationCredentials::generate()?;
        let reply = AuthorizationCredentials::generate()?;
        let mut capabilities = PayoutCapabilities {
            payout: true,
            lnurl: true,
        };
        for advertised in enclaves.enclaves {
            let enclave = transport.attest(advertised.enclave_id).await?;
            let request = transport.prepare(
                &enclave,
                Uuid::now_v7(),
                Command::new(EnclaveCommand::System(
                    SystemCommand::DescribeEscrowVerifiers,
                )),
                &authority,
                &reply,
            )?;
            let outcome = transport.execute(&enclave, &request, &reply).await?;
            let EnclaveOutcome::System(SystemOutcome::EscrowVerifiers(verifiers)) =
                outcome.response
            else {
                return Err(invalid("Enclave did not describe its trusted verifiers"));
            };
            let found = verifiers.iter().find(|info| {
                info.descriptor.id == generic::VERIFIER_ID
                    && info.descriptor.version == generic::VERIFIER_VERSION
            });
            let supported: PayoutCapabilities = match found {
                Some(info) => info.capabilities.decode()?,
                None => PayoutCapabilities::default(),
            };
            capabilities.payout &= supported.payout;
            capabilities.lnurl &= supported.payout && supported.lnurl;
        }
        Ok(capabilities)
    }

    async fn init_keygen_session(
        &self,
        competition_id: Uuid,
        players: Vec<UserId>,
        subsets: DlcSubsetInfo,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        let id = SessionId::from(competition_id);
        let _guard = self.lock_session(&id).await;
        let db = self.database()?;
        let keys = self.storage_keys()?;
        if let Some((_, state)) = confidential_store::load(db, &self.store_key, &id).await? {
            let session = state.session.to_session(&keys)?;
            let manifest = &session.authorization_manifest.manifest;
            let expected: BTreeSet<_> = players
                .iter()
                .chain(std::iter::once(&manifest.coordinator_user_id))
                .collect();
            if manifest
                .participant_verifiers
                .keys()
                .collect::<BTreeSet<_>>()
                != expected
                || encoded(&manifest.subset_definitions)? != encoded(&subsets.definitions)?
            {
                return Err(invalid(
                    "Keygen retry changed its authorized participants or subsets",
                ));
            }
            return Ok(session);
        }
        let client = self.get_client()?;
        let mut available: Vec<_> = client
            .health()
            .list_enclaves()
            .await?
            .enclaves
            .into_iter()
            .map(|info| info.enclave_id)
            .collect();
        available.sort();
        available.dedup();
        if available.is_empty() {
            return Err(invalid(
                "No enclaves available for confidential registration",
            ));
        }
        let credentials = SessionCredentials::generate()?;
        let authority = AuthorizationCredentials::generate()?;
        let users: Vec<_> = std::iter::once(self.coordinator_user_id.clone())
            .chain(players)
            .collect();
        let mut registration_authorities = BTreeMap::new();
        for user in &users {
            if registration_authorities
                .insert(user.clone(), AuthorizationCredentials::generate()?)
                .is_some()
            {
                return Err(invalid("Duplicate keygen participant"));
            }
        }
        let manifest = SignedSessionManifest::sign(
            SessionAuthorizationManifest {
                keygen_session_id: id.clone(),
                coordinator_user_id: self.coordinator_user_id.clone(),
                creator_pubkey: authority.public_key_bytes(),
                signing_pubkey: authority.public_key_bytes(),
                session_public_key: credentials.public_key_bytes(),
                participant_verifiers: registration_authorities
                    .iter()
                    .map(|(user, key)| (user.clone(), key.public_key_bytes()))
                    .collect(),
                timeout_secs: self.settings.keygen_session_expiry_secs,
                max_signing_sessions: None,
                encrypted_taproot_tweak: credentials
                    .encrypt(&encoded(&TaprootTweak::None)?, "taproot_tweak")?,
                subset_definitions: subsets
                    .definitions
                    .iter()
                    .map(|subset| keymeld_core::protocol::SubsetDefinition {
                        subset_id: subset.subset_id,
                        participants: subset.participants.clone(),
                    })
                    .collect(),
            },
            &authority.export_secret(),
        )?;
        let assignments: BTreeMap<_, _> = users
            .iter()
            .enumerate()
            .map(|(index, user)| (user.clone(), available[index % available.len()]))
            .collect();
        let transport = ConfidentialTransport::new(client);
        let mut epochs = BTreeMap::new();
        let mut public_keys = BTreeMap::new();
        for enclave in assignments.values().copied().collect::<BTreeSet<_>>() {
            let pinned = transport.attest(enclave).await?;
            epochs.insert(enclave, pinned.key_epoch());
            public_keys.insert(enclave, pinned.public_key().to_vec());
        }
        let recipients = EnclaveRecipientAuthorization::sign(
            &manifest,
            assignments,
            public_keys,
            &authority.export_secret(),
        )?;
        let session = DlcKeygenSession {
            session_id: id.clone(),
            session_secret: credentials.export_session_secret(),
            authorization_manifest: manifest,
            recipient_authorization: recipients,
            signing_authority: authority,
            registration_authorities,
            aggregate_key: Vec::new(),
            outcome_subset_ids: subsets.outcome_subset_ids,
        };
        let enclave =
            session.recipient_authorization.user_enclave_assignments[&self.coordinator_user_id];
        let context = RegistrationContext {
            keygen_session_id: id.clone(),
            manifest_hash: session.authorization_manifest.digest()?,
            user_id: self.coordinator_user_id.clone(),
            enclave_id: enclave,
            enclave_key_epoch: epochs[&enclave],
            public_key: self.user_credentials.public_key_bytes(),
            auth_pubkey: self
                .user_credentials
                .derive_session_auth_pubkey(&id.to_string())?,
            require_signing_approval: false,
        };
        let data = ParticipantRegistrationData {
            encrypted_private_key: self.user_credentials.prepare_registration(
                context.clone(),
                &hex::encode(&session.recipient_authorization.recipient_public_keys[&enclave]),
            )?,
            public_key: hex::encode(&context.public_key),
            auth_pubkey: hex::encode(&context.auth_pubkey),
            context,
            payout_policy: None,
            escrow_policy: None,
        };
        let native = Self::native_registration(&session, &self.coordinator_user_id, &data)?;
        let state = ProtocolState {
            schema_version: 1,
            session: StoredDlcKeygenSession::from_session(&session, &keys)?,
            epochs,
            registrations: BTreeMap::from([(self.coordinator_user_id.clone(), native)]),
            policies: BTreeMap::new(),
            journal: ConfidentialJournal::default(),
            roster: None,
            bindings: BTreeMap::new(),
            signing: None,
            settlements: BTreeMap::new(),
        };
        if !confidential_store::create(db, &self.store_key, &id, &state).await? {
            return Err(invalid(
                "Another worker created this session; reload before retrying",
            ));
        }
        Ok(session)
    }

    async fn register_participant(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        data: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (mut state, checkpoint) = self.checkpoint(session).await?;
        let native = Self::native_registration(session, &user, data)?;
        if state.epochs.get(&data.context.enclave_id) != Some(&data.context.enclave_key_epoch) {
            return Err(invalid("Registration enclave epoch changed"));
        }
        if let Some(accepted) = state.registrations.get(&user) {
            if encoded(accepted)? != encoded(&native)?
                || encoded(&state.policies.get(&user))? != encoded(&data.escrow_policy.as_ref())?
            {
                return Err(invalid(
                    "Participant retry changed its accepted registration or consent",
                ));
            }
            return Ok(());
        }
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let mut driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        driver.validate_registration(&native).await?;
        drop(driver);
        state.registrations.insert(user.clone(), native);
        if let Some(policy) = &data.escrow_policy {
            state.policies.insert(user, policy.clone());
        }
        state.journal = journal;
        checkpoint.finish(state).await?;
        Ok(())
    }

    async fn wait_for_keygen_completion(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<SignedRoster, KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (mut state, checkpoint) = self.checkpoint(session).await?;
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let mut driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        let roster = driver.restore_keygen(&state.registrations).await?;
        drop(driver);
        state.roster = Some(roster.clone());
        state.journal = journal;
        checkpoint.finish(state).await?;
        Ok(roster)
    }

    async fn get_keygen_status(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<KeygenSessionStatus, KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (state, _) = self.checkpoint(session).await?;
        // The lifecycle completes keygen (wait_for_keygen_completion, which
        // records the roster) only after this reports ready. Requiring the
        // roster here as well meant keygen never started. Every participant
        // the manifest authorized having registered is the readiness signal.
        let registered = session
            .authorization_manifest
            .manifest
            .participant_verifiers
            .keys()
            .all(|user| state.registrations.contains_key(user));
        Ok(KeygenSessionStatus {
            session_id: session.session_id.to_string(),
            status: if state.roster.is_some() {
                "completed"
            } else if registered {
                "participants_registered"
            } else {
                "collecting_participants"
            }
            .into(),
            is_completed: state.roster.is_some() || registered,
        })
    }

    async fn get_registration_assignment(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
    ) -> Result<RegistrationAssignment, KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (state, _) = self.checkpoint(session).await?;
        let enclave = *session
            .recipient_authorization
            .user_enclave_assignments
            .get(&user)
            .ok_or_else(|| invalid("No authorized participant slot"))?;
        let pinned = ConfidentialTransport::new(self.get_client()?)
            .attest(enclave)
            .await?;
        if pinned.public_key() != session.recipient_authorization.recipient_public_keys[&enclave]
            || state.epochs.get(&enclave) != Some(&pinned.key_epoch())
        {
            return Err(invalid(
                "Enclave recipient changed; fresh participant consent is required",
            ));
        }
        Ok(RegistrationAssignment {
            session_id: session.session_id.to_string(),
            user_id: user.uuid(),
            manifest_hash: session.authorization_manifest.digest()?,
            enclave_id: enclave.as_u32(),
            enclave_key_epoch: pinned.key_epoch(),
            enclave_public_key: hex::encode(pinned.public_key()),
            gateway_url: self
                .settings
                .public_gateway_url
                .clone()
                .unwrap_or_else(|| self.settings.gateway_url.clone()),
            trusted_pcrs: self.settings.trusted_pcrs.clone(),
            dangerous_trust_unattested_enclaves: self.settings.dangerous_trust_unattested_enclaves,
            payout_policy: None,
        })
    }

    async fn bind_payout_contract(
        &self,
        session: &DlcKeygenSession,
        contract: &ContractCommitment,
        expected: &BTreeMap<UserId, PayoutPolicy>,
    ) -> Result<Vec<PayoutContractBoundResponse>, KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (mut state, checkpoint) = self.checkpoint(session).await?;
        if state.policies.keys().collect::<BTreeSet<_>>() != expected.keys().collect() {
            return Err(invalid(
                "Accepted payout policies differ from deposited consent",
            ));
        }
        for (user, policy) in expected {
            let registered = &state
                .registrations
                .get(user)
                .ok_or_else(|| invalid("Payout participant not registered"))?
                .registration_authorization
                .context;
            coordinator_core::keymeld::verify_registration_policy(
                registered,
                Some(policy),
                state.policies.get(user),
            )?;
        }
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let mut driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        let roster = driver.restore_keygen(&state.registrations).await?;
        let binding = generic::ContractBinding {
            contract: contract.clone(),
        };
        let mut responses = Vec::new();
        for (user, policy) in &state.policies {
            let request = BindEscrowRequest {
                schema_version: escrow::SCHEMA_VERSION,
                policy: policy.clone(),
                application_context: policy
                    .policy
                    .verifier
                    .as_ref()
                    .ok_or_else(|| invalid("Missing trusted verifier"))?
                    .policy_data
                    .clone(),
                participant_policies: state.policies.clone(),
                binding_data: Payload::encode(&binding)?,
            };
            let response = escrow_request(
                &mut driver,
                session,
                &state,
                &credentials,
                user,
                &format!("escrow/bind/{user}"),
                Operation::Bind,
                None,
                None,
                &request,
            )
            .await?;
            responses.push(PayoutContractBoundResponse::from_response(
                binding.clone(),
                response,
            )?);
        }
        drop(driver);
        state.bindings = responses
            .iter()
            .map(|bound| (bound.user_id.clone(), bound.response.clone()))
            .collect();
        state.roster = Some(roster);
        state.journal = journal;
        checkpoint.finish(state).await?;
        Ok(responses)
    }

    async fn prepare_payout(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        request: PreparePayoutRequest,
    ) -> Result<PayoutPreparedResponse, KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (mut state, checkpoint) = self.checkpoint(session).await?;
        let binding = state
            .bindings
            .get(&user)
            .cloned()
            .ok_or_else(|| invalid("Participant contract has not been bound"))?;
        if decode_hex(&request.binding_receipt)? != binding.sealed_state.as_bytes() {
            return Err(invalid("Payout binding differs from its accepted contract"));
        }
        let previous = state.settlements.get(&user).cloned();
        if previous
            .as_ref()
            .is_none_or(|plan| plan.claim_id != request.claim_id)
        {
            let mut predecessors = previous
                .as_ref()
                .map(|plan| plan.prior_preparations.clone())
                .unwrap_or_default();
            if let Some(previous) = previous {
                let enclave = session.recipient_authorization.user_enclave_assignments[&user];
                for permission in [generic::RELEASE_PREIMAGE, generic::RELEASE_ENTRY_KEY] {
                    let stage =
                        format!("escrow/prepare/{user}/{}/{permission}/", previous.claim_id);
                    if let Some(outcome) = state.journal.command_outcome(&stage, enclave) {
                        if let EnclaveOutcome::Musig(MusigOutcome::Keygen(KeygenOutcome::Escrow(
                            prepared,
                        ))) = &outcome.response
                        {
                            predecessors.insert(permission.into(), *prepared.clone());
                        }
                    }
                }
            }
            state.settlements.insert(
                user.clone(),
                SettlementPlan {
                    claim_id: request.claim_id,
                    prior_preparations: predecessors,
                },
            );
            // Record the replacement identity and its authenticated predecessors
            // before a resolver request can create an externally payable invoice.
            checkpoint.finish(state.clone()).await?;
        }
        let settlement_plan = state.settlements[&user].clone();
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let mut driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        driver.restore_keygen(&state.registrations).await?;
        let attempt = ActionAttempt {
            attempt_id: request.claim_id,
            signing_session_id: None,
        };
        let parameters = Payload::encode(&generic::ActionParameters::PrepareSettlement {
            claim_id: request.claim_id,
            contract_signatures: request.contract_signatures,
            attestation: request.attestation,
            method: request.method,
        })?;
        let first_request = PrepareEscrowRequest {
            schema_version: escrow::SCHEMA_VERSION,
            binding_receipt: binding.sealed_state.clone(),
            action_id: generic::RELEASE_PREIMAGE.into(),
            attempt: attempt.clone(),
            action: None,
            action_parameters: parameters.clone(),
            prior_preparation_receipts: settlement_plan
                .prior_preparations
                .get(generic::RELEASE_PREIMAGE)
                .map(|previous| vec![previous.sealed_state.clone()])
                .unwrap_or_default(),
        };
        let first = escrow_request(
            &mut driver,
            session,
            &state,
            &credentials,
            &user,
            &format!(
                "escrow/prepare/{user}/{}/{}/",
                request.claim_id,
                generic::RELEASE_PREIMAGE
            ),
            Operation::Prepare,
            Some(generic::RELEASE_PREIMAGE),
            Some(attempt.clone()),
            &first_request,
        )
        .await?;
        let mut key_predecessors = vec![first.sealed_state.clone()];
        if let Some(previous) = settlement_plan
            .prior_preparations
            .get(generic::RELEASE_ENTRY_KEY)
        {
            key_predecessors.push(previous.sealed_state.clone());
        }
        let second_request = PrepareEscrowRequest {
            action_id: generic::RELEASE_ENTRY_KEY.into(),
            prior_preparation_receipts: key_predecessors,
            ..first_request
        };
        let second = escrow_request(
            &mut driver,
            session,
            &state,
            &credentials,
            &user,
            &format!(
                "escrow/prepare/{user}/{}/{}/",
                request.claim_id,
                generic::RELEASE_ENTRY_KEY
            ),
            Operation::Prepare,
            Some(generic::RELEASE_ENTRY_KEY),
            Some(attempt),
            &second_request,
        )
        .await?;
        let response = PayoutPreparedResponse::from_responses(first, second)?;
        verify_prepared_payout_origin(session, &user, &response)?;
        drop(driver);
        state.journal = journal;
        checkpoint.finish(state).await?;
        Ok(response)
    }

    async fn release_payout(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        request: ReleasePayoutRequest,
    ) -> Result<PayoutSecrets, KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (mut state, checkpoint) = self.checkpoint(session).await?;
        let enclave = *session
            .recipient_authorization
            .user_enclave_assignments
            .get(&user)
            .ok_or_else(|| invalid("Payout participant is unassigned"))?;
        let receipts = PreparedPayoutReceipts::decode(&request.state_receipt)?;
        receipts.verify(&session.recipient_authorization.recipient_public_keys[&enclave])?;
        let settlement = receipts.settlement()?;
        if settlement.claim_id != request.claim_id
            || receipts.preimage_preparation.context.request.escrow.user_id != user
            || receipts
                .preimage_preparation
                .context
                .request
                .escrow
                .keygen_session_id
                != session.session_id
        {
            return Err(invalid(
                "Prepared payout belongs to another session, participant or claim",
            ));
        }
        let preimage = Zeroizing::new(decode_hex(&request.payment_preimage)?);
        let evidence = generic::PaymentEvidence {
            payment_preimage: preimage
                .as_slice()
                .try_into()
                .map_err(|_| invalid("Invalid payment preimage length"))?,
        };
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let mut driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        driver.restore_keygen(&state.registrations).await?;
        let mut released = BTreeMap::new();
        for (permission, prepared) in [
            (generic::RELEASE_PREIMAGE, &receipts.preimage_preparation),
            (generic::RELEASE_ENTRY_KEY, &receipts.key_preparation),
        ] {
            let execute = ExecuteEscrowRequest {
                schema_version: escrow::SCHEMA_VERSION,
                prepared_receipt: prepared.sealed_state.clone(),
                proof: ConditionProof::VerifierEvidence {
                    evidence: Payload::encode(&evidence)?,
                },
            };
            let response = escrow_request(
                &mut driver,
                session,
                &state,
                &credentials,
                &user,
                &format!("escrow/execute/{user}/{}/{permission}", request.claim_id),
                Operation::Execute,
                Some(permission),
                prepared.context.request.attempt.clone(),
                &execute,
            )
            .await?;
            let output: ExecutionOutput = response.output.decode()?;
            let (recipient, ciphertext) = match output {
                ExecutionOutput::ReleasedSecret {
                    ref name,
                    ref recipient,
                    ref encrypted_secret,
                } if permission == generic::RELEASE_PREIMAGE
                    && name == generic::PREIMAGE_SECRET =>
                {
                    (recipient, encrypted_secret)
                }
                ExecutionOutput::ReleasedSigningKey {
                    ref public_key,
                    ref recipient,
                    ref encrypted_key,
                } if permission == generic::RELEASE_ENTRY_KEY
                    && public_key == &state.policies[&user].policy.participant_public_key =>
                {
                    (recipient, encrypted_key)
                }
                _ => {
                    return Err(invalid(
                        "Enclave release has an unexpected permission or key",
                    ))
                }
            };
            if recipient.encryption_public_key.as_bytes()
                != self.user_credentials.public_key_bytes()
            {
                return Err(invalid("Released escrow belongs to a different recipient"));
            }
            let value = self.user_credentials.decrypt_ecies(ciphertext.as_bytes())?;
            if value.len() != 32 {
                return Err(invalid("Invalid released secret length"));
            }
            released.insert(permission, Zeroizing::new(hex::encode(&*value)));
        }
        drop(driver);
        state.journal = journal;
        checkpoint.finish(state).await?;
        Ok(PayoutSecrets {
            entry_private_key: released[generic::RELEASE_ENTRY_KEY].to_string(),
            payout_preimage: released[generic::RELEASE_PREIMAGE].to_string(),
        })
    }

    async fn sign_dlc_batch(
        &self,
        session: &DlcKeygenSession,
        signing_data: &SigningData,
        params: &ContractParameters,
        players: Vec<UserId>,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (mut state, checkpoint) = self.checkpoint(session).await?;
        let subsets = outcome_subsets_for_payouts(
            &params.outcome_payouts,
            &players,
            &session.authorization_manifest.manifest.coordinator_user_id,
            &session.authorization_manifest.manifest.subset_definitions,
        )?;
        let candidate = DlcBatchBuilder::new(signing_data)
            .with_outcome_subsets(&subsets)
            .build()?;
        let semantics: Vec<_> = candidate
            .items
            .iter()
            .map(|item| {
                let mode = match item.mode() {
                    keymeld_sdk::BatchSigningMode::Regular => serde_json::Value::Null,
                    keymeld_sdk::BatchSigningMode::Adaptor { configs } => {
                        serde_json::json!(configs
                            .iter()
                            .map(|c| (&c.adaptor_type, &c.adaptor_points, &c.hints))
                            .collect::<Vec<_>>())
                    }
                };
                (item.message(), item.subset_id(), item.taproot_tweak(), mode)
            })
            .collect();
        let input_digest = authorization_digest(
            "coordinator-confidential-dlc-plan-v1",
            &(&players, semantics),
        )?;
        if let Some(plan) = &state.signing {
            if plan.input_digest != input_digest {
                return Err(invalid("Signing retry changed the accepted DLC messages"));
            }
        } else {
            state.signing = Some(SigningPlan {
                input_digest,
                session_id: SessionId::new_v7(),
                batch: candidate,
                prior_preparations: BTreeMap::new(),
            });
            checkpoint.finish(state.clone()).await?;
        }
        let mut plan = state.signing.as_ref().expect("saved plan").clone();
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let mut driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        let roster = driver.restore_keygen(&state.registrations).await?;
        if driver.is_signing_aborted(&plan.session_id) {
            // A lost nonce round cannot be resumed. Preserve the exact approved
            // messages and use the latest sealed preparation for each signer to
            // authorize a new session under explicit repetition consent.
            for user in state.policies.keys() {
                let enclave = session.recipient_authorization.user_enclave_assignments[user];
                let stage = format!("escrow/sign/prepare/{user}/{}", plan.session_id);
                if let Some(outcome) = driver.command_outcome(&stage, enclave) {
                    if let EnclaveOutcome::Musig(MusigOutcome::Keygen(KeygenOutcome::Escrow(
                        prepared,
                    ))) = &outcome.response
                    {
                        plan.prior_preparations
                            .insert(user.clone(), *prepared.clone());
                    }
                }
            }
            plan.session_id = SessionId::new_v7();
            drop(driver);
            state.signing = Some(plan.clone());
            state.journal = journal.clone();
            checkpoint.finish(state.clone()).await?;
            driver = self
                .connect(session, &state, &credentials, &mut journal, &checkpoint)
                .await?;
        }
        driver
            .prepare_signing_batch(&plan.session_id, &plan.batch.items)
            .await?;
        for (user, policy) in &state.policies {
            let scope = scope_for_participant(&roster, &plan.batch.items, user)?;
            let expected_scope_digest = authorization_digest("escrow-signing-scope-v1", &scope)?;
            let binding = state
                .bindings
                .get(user)
                .ok_or_else(|| invalid("DLC consent must be bound before signing"))?;
            let attempt = ActionAttempt {
                attempt_id: plan.session_id.uuid(),
                signing_session_id: Some(plan.session_id.clone()),
            };
            let request = PrepareEscrowRequest {
                schema_version: escrow::SCHEMA_VERSION,
                binding_receipt: binding.sealed_state.clone(),
                action_id: generic::SIGN_CONTRACT.into(),
                attempt: attempt.clone(),
                action: None,
                action_parameters: Payload::encode(&generic::ActionParameters::SignContract {
                    scope,
                })?,
                prior_preparation_receipts: plan
                    .prior_preparations
                    .get(user)
                    .map(|prepared| vec![prepared.sealed_state.clone()])
                    .unwrap_or_default(),
            };
            let prepared = escrow_request(
                &mut driver,
                session,
                &state,
                &credentials,
                user,
                &format!("escrow/sign/prepare/{user}/{}", plan.session_id),
                Operation::Prepare,
                Some(generic::SIGN_CONTRACT),
                Some(attempt.clone()),
                &request,
            )
            .await?;
            let execute = ExecuteEscrowRequest {
                schema_version: escrow::SCHEMA_VERSION,
                prepared_receipt: prepared.sealed_state,
                proof: ConditionProof::VerifierEvidence {
                    evidence: Payload::default(),
                },
            };
            let response = escrow_request(
                &mut driver,
                session,
                &state,
                &credentials,
                user,
                &format!("escrow/sign/execute/{user}/{}", plan.session_id),
                Operation::Execute,
                Some(generic::SIGN_CONTRACT),
                Some(attempt),
                &execute,
            )
            .await?;
            let ExecutionOutput::SigningPermit {
                signing_session_id,
                scope_digest,
            } = response.output.decode()?
            else {
                return Err(invalid("Enclave did not authorize contract signing"));
            };
            if signing_session_id != plan.session_id
                || scope_digest != expected_scope_digest
                || response.context.request.policy_digest != policy.policy.digest()?
            {
                return Err(invalid(
                    "Signing permit differs from the accepted policy and session",
                ));
            }
        }
        let encrypted = driver
            .sign_prepared_batch(
                &plan.session_id,
                self.settings.signing_session_expiry_secs,
                &[],
            )
            .await?;
        let results = driver.decrypt_batch_results(&encrypted)?;
        let signatures = plan.batch.parse_results(&results)?;
        drop(driver);
        state.roster = Some(roster);
        state.journal = journal;
        checkpoint.finish(state).await?;
        Ok(signatures)
    }
}
