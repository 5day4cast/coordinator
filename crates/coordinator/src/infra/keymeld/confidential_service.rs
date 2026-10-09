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
    confidential_session::{
        CheckpointFuture, ConfidentialCheckpoint, ConfidentialJournal, ConfidentialSession,
    },
};

#[path = "deposit_sessions.rs"]
mod deposit_sessions;

/// Keeps the command journal in memory only, for work inside an Arkade batch.
///
/// A batch that fails is retried with a new commitment transaction, new messages, and fresh
/// attempts, so nothing signed inside the batch needs to be replayed after a restart.
/// Persisting the whole protocol state twice per command would also cost more with every
/// command, which a batch's session window cannot afford.
struct EphemeralCheckpoint;

impl ConfidentialCheckpoint for EphemeralCheckpoint {
    fn save<'a>(&'a self, _journal: &'a ConfidentialJournal) -> CheckpointFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

/// Keeps the command journal durably until [`KeygenCheckpoint::keep_in_memory`], then only in
/// memory, for a refund: its keygen commands must survive a restart, its signing commands need not.
///
/// The enclaves bind a session to the route of the journal that started it. A journal that lost
/// its keygen commands starts over on a new route, and the enclaves refuse it. A refund's signing
/// commands are fresh attempts each time, so, as in a batch, they are not kept.
struct KeygenCheckpoint<'a> {
    durable: &'a DurableCheckpoint,
    keep: std::sync::atomic::AtomicBool,
}

impl<'a> KeygenCheckpoint<'a> {
    fn new(durable: &'a DurableCheckpoint) -> Self {
        Self {
            durable,
            keep: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn keep_in_memory(&self) {
        self.keep.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ConfidentialCheckpoint for KeygenCheckpoint<'_> {
    fn save<'a>(&'a self, journal: &'a ConfidentialJournal) -> CheckpointFuture<'a> {
        if self.keep.load(std::sync::atomic::Ordering::SeqCst) {
            self.durable.save(journal)
        } else {
            Box::pin(async { Ok(()) })
        }
    }
}

/// Whether Keymeld was already sent this session's roster: some enclave journaled the batch that
/// registers its participants. After that no participant can join, since an enclave would never
/// be sent them.
fn roster_sent(session: &DlcKeygenSession, journal: &ConfidentialJournal) -> bool {
    session
        .recipient_authorization
        .recipient_public_keys
        .keys()
        .any(|enclave| {
            journal
                .recorded_command("keygen/register", *enclave)
                .is_some()
        })
}

/// Whether every participant the manifest authorizes is registered, as when a competition filled.
fn roster_complete(session: &DlcKeygenSession, state: &ProtocolState) -> bool {
    session
        .authorization_manifest
        .manifest
        .participant_verifiers
        .keys()
        .all(|user| state.registrations.contains_key(user))
}
use std::collections::BTreeSet;
use tokio::sync::{Mutex, OwnedMutexGuard};
use zeroize::Zeroizing;

fn invalid(message: impl Into<String>) -> KeymeldError {
    KeymeldError::Session(message.into())
}

/// The gateway's enclaves, sorted, for spreading participants across them.
async fn available_enclaves(client: &KeyMeldClient) -> Result<Vec<EnclaveId>, KeymeldError> {
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
    Ok(available)
}
fn encoded<T: Serialize>(value: &T) -> Result<Vec<u8>, KeymeldError> {
    serde_json::to_vec(value).map_err(|error| invalid(error.to_string()))
}
fn decode_hex(value: &str) -> Result<Vec<u8>, KeymeldError> {
    hex::decode(value).map_err(|_| invalid("Invalid protocol hex encoding"))
}

/// Run `work` on each enclave's share of `items`, the shares side by side, and return the
/// results in the order of `items`.
///
/// Each enclave serializes one session's commands, but the enclaves are independent and the
/// gateway only relays. Each share keeps its order and gets its own copy of `lane`, for a journal
/// only work inside an Arkade batch may split: see [`EphemeralCheckpoint`]. Every share runs to
/// its end, so no request is abandoned in flight; if any fails, so does the whole call, with the
/// first failure in enclave order, and no result is returned.
async fn per_enclave<S, T, R, F, Fut>(
    items: Vec<(EnclaveId, T)>,
    lane: &S,
    work: F,
) -> Result<Vec<R>, KeymeldError>
where
    S: Clone,
    F: Fn(S, Vec<T>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<R>, KeymeldError>>,
{
    let count = items.len();
    let mut shares: BTreeMap<EnclaveId, (Vec<usize>, Vec<T>)> = BTreeMap::new();
    for (index, (enclave, item)) in items.into_iter().enumerate() {
        let (indexes, items) = shares.entry(enclave).or_default();
        indexes.push(index);
        items.push(item);
    }
    let runs = shares.into_values().map(|(indexes, items)| {
        let run = work(lane.clone(), items);
        async move { (indexes, run.await) }
    });
    let mut ordered: Vec<Option<R>> = std::iter::repeat_with(|| None).take(count).collect();
    for (indexes, results) in futures::future::join_all(runs).await {
        let results = results?;
        if results.len() != indexes.len() {
            return Err(invalid(
                "An enclave's share returned the wrong number of results",
            ));
        }
        for (index, result) in indexes.into_iter().zip(results) {
            ordered[index] = Some(result);
        }
    }
    ordered
        .into_iter()
        .map(|result| result.ok_or_else(|| invalid("Enclave omitted an assigned result")))
        .collect()
}

impl KeymeldService {
    /// Prepare and execute one unbound escrow action under the refund's permission, signing as
    /// `user`: a refund's transaction or batch intent, or a proof deleting a queued intent. `stage` names it in
    /// the journal. Returns each signed input's index with its BIP340 signature.
    async fn sign_unbound_escrow(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        stage: &str,
        parameters: generic::ActionParameters,
    ) -> Result<(Vec<(usize, [u8; 64])>, Payload), KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (mut state, durable) = self.checkpoint(session).await?;
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let checkpoint = KeygenCheckpoint::new(&durable);
        let mut driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        if roster_complete(session, &state) {
            driver.restore_keygen(&state.registrations).await?;
        } else {
            // A pool that never filled has no complete roster to form a key from. Keymeld
            // registers the participants present and signs each one's refund with their own
            // entry key. Repeating this only replays the requests journaled the first time.
            driver.register_partial_roster(&state.registrations).await?;
        }
        checkpoint.keep_in_memory();
        let participant_key = &state
            .policies
            .get(&user)
            .ok_or_else(|| invalid("Escrow participant has no accepted policy"))?
            .policy
            .participant_public_key;
        // Each transaction of a refund is signed in its own attempt, as is a retry with a new
        // invoice, and each delete proof; the verifier checks every one.
        let attempt = ActionAttempt {
            attempt_id: Uuid::now_v7(),
            signing_session_id: None,
        };
        let prepare = PrepareEscrowRequest {
            schema_version: escrow::SCHEMA_VERSION,
            // A refund is unbound: the pool it would have funded may never have formed, so the
            // enclave derives this action's binding from the player's own policy.
            binding_receipt: Payload::default(),
            action_id: generic::SIGN_ARK_REFUND.into(),
            attempt: attempt.clone(),
            action: None,
            action_parameters: Payload::encode(&parameters)?,
            prior_preparation_receipts: vec![],
        };
        let prepared = escrow_request(
            &mut driver,
            session,
            &state,
            &credentials,
            &user,
            &format!("escrow/{stage}/prepare/{user}/{}", attempt.attempt_id),
            Operation::Prepare,
            Some(generic::SIGN_ARK_REFUND),
            Some(attempt.clone()),
            &prepare,
        )
        .await?;
        let output = prepared.output.clone();
        let inputs: Vec<usize> = if stage == "refund-invoice" {
            vec![0]
        } else {
            output.decode()?
        };
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
            &user,
            &format!("escrow/{stage}/execute/{user}/{}", attempt.attempt_id),
            Operation::Execute,
            Some(generic::SIGN_ARK_REFUND),
            Some(attempt),
            &execute,
        )
        .await?;
        let ExecutionOutput::Bip340Signatures {
            public_key,
            signatures,
        } = response.output.decode()?
        else {
            return Err(invalid("Enclave did not sign the escrow action"));
        };
        if &public_key != participant_key || signatures.len() != inputs.len() {
            return Err(invalid(
                "Escrow signatures differ from the authorized action",
            ));
        }
        let signatures = inputs
            .into_iter()
            .zip(signatures)
            .map(|(input, signature)| {
                let bytes: [u8; 64] = signature
                    .signature
                    .try_into()
                    .map_err(|_| invalid("Invalid BIP340 signature length"))?;
                Ok((input, bytes))
            })
            .collect::<Result<Vec<_>, KeymeldError>>()?;
        Ok((signatures, output))
    }

    fn database(&self) -> Result<&DBConnection, KeymeldError> {
        self.db
            .as_ref()
            .ok_or_else(|| invalid("Confidential signing requires durable application storage"))
    }
    async fn lock_session(&self, id: &SessionId) -> OwnedMutexGuard<()> {
        self.session_lock(id).await.lock_owned().await
    }

    async fn session_lock(&self, id: &SessionId) -> Arc<Mutex<()>> {
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
        lock
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
            &state,
        )?;
        Ok((state, checkpoint))
    }
    async fn connect<'a>(
        &'a self,
        session: &'a DlcKeygenSession,
        state: &ProtocolState,
        credentials: &'a SessionCredentials,
        journal: &'a mut ConfidentialJournal,
        checkpoint: &'a dyn ConfidentialCheckpoint,
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
    // A deposit's policy names its deposit scope rather than this session, so the command
    // names the session it acts in; a session's own policies leave it out.
    let keygen_session_id = (policy.policy.context.keygen_session_id != session.session_id)
        .then(|| session.session_id.clone());
    let context = RequestContext {
        schema_version: escrow::SCHEMA_VERSION,
        operation,
        escrow: policy.policy.context.clone(),
        policy_digest: policy.policy.digest()?,
        request_id: Uuid::nil(), // Template identity is excluded from semantic retries.
        action_id: action.map(str::to_owned),
        attempt,
        keygen_session_id,
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

/// Prepare and execute `user`'s escrow spend in the pool's session. Returns each signed input's
/// index with its BIP340 signature.
async fn sign_ark_spend(
    driver: &mut ConfidentialSession<'_>,
    session: &DlcKeygenSession,
    state: &ProtocolState,
    credentials: &SessionCredentials,
    user: UserId,
    spend: coordinator_escrow::ark::ArkEscrowSpend,
) -> Result<Vec<(usize, [u8; 64])>, KeymeldError> {
    let binding = state
        .bindings
        .get(&user)
        .ok_or_else(|| invalid("The pool must be bound before its escrows are spent"))?;
    let participant_key = &state
        .policies
        .get(&user)
        .ok_or_else(|| invalid("Escrow participant has no accepted policy"))?
        .policy
        .participant_public_key;
    // Every batch attempt signs new transactions, so each spend is a fresh attempt.
    let attempt = ActionAttempt {
        attempt_id: Uuid::now_v7(),
        signing_session_id: None,
    };
    let prepare = PrepareEscrowRequest {
        schema_version: escrow::SCHEMA_VERSION,
        binding_receipt: binding.sealed_state.clone(),
        action_id: generic::SIGN_ARK_ESCROW.into(),
        attempt: attempt.clone(),
        action: None,
        action_parameters: Payload::encode(&generic::ActionParameters::SignArkEscrow { spend })?,
        prior_preparation_receipts: vec![],
    };
    let prepared = escrow_request(
        driver,
        session,
        state,
        credentials,
        &user,
        &format!("escrow/ark/prepare/{user}/{}", attempt.attempt_id),
        Operation::Prepare,
        Some(generic::SIGN_ARK_ESCROW),
        Some(attempt.clone()),
        &prepare,
    )
    .await?;
    let inputs: Vec<usize> = prepared.output.decode()?;
    let execute = ExecuteEscrowRequest {
        schema_version: escrow::SCHEMA_VERSION,
        prepared_receipt: prepared.sealed_state,
        proof: ConditionProof::VerifierEvidence {
            evidence: Payload::default(),
        },
    };
    let response = escrow_request(
        driver,
        session,
        state,
        credentials,
        &user,
        &format!("escrow/ark/execute/{user}/{}", attempt.attempt_id),
        Operation::Execute,
        Some(generic::SIGN_ARK_ESCROW),
        Some(attempt),
        &execute,
    )
    .await?;
    let ExecutionOutput::Bip340Signatures {
        public_key,
        signatures,
    } = response.output.decode()?
    else {
        return Err(invalid("Enclave did not sign the escrow spend"));
    };
    if &public_key != participant_key || signatures.len() != inputs.len() {
        return Err(invalid(
            "Escrow signatures differ from the authorized spend",
        ));
    }
    inputs
        .into_iter()
        .zip(signatures)
        .map(|(input, signature)| {
            let bytes: [u8; 64] = signature
                .signature
                .try_into()
                .map_err(|_| invalid("Invalid BIP340 signature length"))?;
            Ok((input, bytes))
        })
        .collect()
}

/// Have `user`'s enclave permit signing their share of the plan's batch: prepare and execute
/// SIGN_CONTRACT, and check the permit it returns.
#[allow(clippy::too_many_arguments)]
async fn permit_contract(
    driver: &mut ConfidentialSession<'_>,
    session: &DlcKeygenSession,
    state: &ProtocolState,
    credentials: &SessionCredentials,
    roster: &SignedRoster,
    plan: &SigningPlan,
    ark_funding: &Option<coordinator_escrow::ark::ArkFunding>,
    user: &UserId,
    policy: &SignedEscrowPolicy,
) -> Result<(), KeymeldError> {
    let scope = scope_for_participant(roster, &plan.batch.items, user)?;
    let expected_scope_digest = authorization_digest("escrow-signing-scope-v1", &scope)?;
    let binding = state
        .bindings
        .get(user)
        .ok_or_else(|| invalid("DLC consent must be bound before signing"))?;
    let attempt = ActionAttempt {
        attempt_id: plan.session_id.uuid(),
        signing_session_id: Some(plan.session_id.clone()),
    };
    // The verifier derives the signers and adaptor points, so only the ids and digests go.
    let mut request = PrepareEscrowRequest {
        schema_version: escrow::SCHEMA_VERSION,
        binding_receipt: binding.sealed_state.clone(),
        action_id: generic::SIGN_CONTRACT.into(),
        attempt: attempt.clone(),
        action: None,
        action_parameters: Payload::encode(&generic::ActionParameters::SignContractCompact {
            items: generic::ContractItem::compact(&scope),
            ark_funding: ark_funding.clone(),
        })?,
        prior_preparation_receipts: plan
            .prior_preparations
            .get(user)
            .map(|prepared| vec![prepared.sealed_state.clone()])
            .unwrap_or_default(),
    };
    let stage = format!("escrow/sign/prepare/{user}/{}", plan.session_id);
    let prepared = match escrow_request(
        driver,
        session,
        state,
        credentials,
        user,
        &stage,
        Operation::Prepare,
        Some(generic::SIGN_CONTRACT),
        Some(attempt.clone()),
        &request,
    )
    .await
    {
        // Both forms authorize the same action, so the full one is sent where needed.
        Err(error) if needs_full_scope(&error) => {
            request.action_parameters =
                Payload::encode(&generic::ActionParameters::SignContract {
                    scope,
                    ark_funding: ark_funding.clone(),
                })?;
            escrow_request(
                driver,
                session,
                state,
                credentials,
                user,
                &stage,
                Operation::Prepare,
                Some(generic::SIGN_CONTRACT),
                Some(attempt.clone()),
                &request,
            )
            .await?
        }
        result => result?,
    };
    let execute = ExecuteEscrowRequest {
        schema_version: escrow::SCHEMA_VERSION,
        prepared_receipt: prepared.sealed_state,
        proof: ConditionProof::VerifierEvidence {
            evidence: Payload::default(),
        },
    };
    let response = escrow_request(
        driver,
        session,
        state,
        credentials,
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
    Ok(())
}

/// Whether a contract signing preparation must be sent with its full scope: a verifier older
/// than the compact scope refused it, or an earlier release journaled this stage with the full
/// scope, which a retry must repeat exactly.
fn needs_full_scope(error: &KeymeldError) -> bool {
    let error = error.to_string();
    generic::refuses_compact_scope(&error)
        || error.contains("Confidential retry changed its original inputs")
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
        let available = available_enclaves(client).await?;
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
                deposit_scope: None,
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
        if roster_sent(session, &state.journal) {
            return Err(KeymeldError::RosterFixed(user.to_string()));
        }
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        admit_native_registration(driver, &user, data.context.enclave_id, &native).await?;
        state.registrations.insert(user.clone(), native);
        if let Some(policy) = &data.escrow_policy {
            state.policies.insert(user, policy.clone());
        }
        state.journal = journal;
        checkpoint.finish(&state).await?;
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
        let driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        let roster = restore_roster(driver, &state.registrations).await?;
        state.roster = Some(roster.clone());
        state.journal = journal;
        checkpoint.finish(&state).await?;
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
            gateway_url: self.settings.browser_gateway_url().to_owned(),
            trusted_pcrs: self.settings.trusted_pcrs.clone(),
            dangerous_trust_unattested_enclaves: self.settings.dangerous_trust_unattested_enclaves,
            payout_policy: None,
        })
    }

    async fn deposit_assignment(
        &self,
        deposit_session_id: SessionId,
        deposit_digest: [u8; 32],
        user: UserId,
    ) -> Result<RegistrationAssignment, KeymeldError> {
        let client = self.get_client()?;
        let available = available_enclaves(client).await?;
        // Spread by ticket rather than by a counter, so asking again for the same ticket gives
        // the same enclave.
        let enclave = available[(user.uuid().as_u128() % available.len() as u128) as usize];
        let pinned = ConfidentialTransport::new(client).attest(enclave).await?;
        Ok(RegistrationAssignment {
            session_id: deposit_session_id.to_string(),
            user_id: user.uuid(),
            manifest_hash: deposit_digest.to_vec(),
            enclave_id: enclave.as_u32(),
            enclave_key_epoch: pinned.key_epoch(),
            enclave_public_key: hex::encode(pinned.public_key()),
            gateway_url: self.settings.browser_gateway_url().to_owned(),
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
        self.bind_contract(session, contract, expected, None).await
    }

    async fn bind_payout_contract_with_statement(
        &self,
        session: &DlcKeygenSession,
        contract: &ContractCommitment,
        expected: &BTreeMap<UserId, PayoutPolicy>,
        statement: coordinator_escrow::oracle_statement::SignedStatement,
    ) -> Result<Vec<PayoutContractBoundResponse>, KeymeldError> {
        self.bind_contract(session, contract, expected, Some(statement))
            .await
    }

    async fn validate_deposit(
        &self,
        scope: DepositScopeRequest,
        user: UserId,
        registration: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        self.check_deposit(scope, user, registration).await
    }

    async fn init_deposit_session(
        &self,
        session_id: Uuid,
        scope: DepositScopeRequest,
        members: Vec<(UserId, EnclaveId)>,
        subsets: DlcSubsetInfo,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        self.create_deposit_session(session_id, scope, members, subsets)
            .await
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
            checkpoint.finish(&state).await?;
        }
        let settlement_plan = state.settlements[&user].clone();
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        let response = EscrowPhase {
            driver,
            session,
            state: &state,
            credentials: &credentials,
        }
        .prepare_payout(&user, request, &binding, &settlement_plan)
        .await?;
        state.journal = journal;
        checkpoint.finish(&state).await?;
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
        // A deposit's policy names its deposit scope, and the command the session it acts in.
        if settlement.claim_id != request.claim_id
            || receipts.preimage_preparation.context.request.escrow.user_id != user
            || receipts.preimage_preparation.context.request.session_id() != &session.session_id
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
        let driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        let released = EscrowPhase {
            driver,
            session,
            state: &state,
            credentials: &credentials,
        }
        .release_payout(self, &user, request.claim_id, &receipts, &evidence)
        .await?;
        state.journal = journal;
        checkpoint.finish(&state).await?;
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
        self.sign_dlc_batch_with(session, signing_data, params, players, None)
            .await
    }

    async fn sign_ark_dlc_batch(
        &self,
        session: &DlcKeygenSession,
        signing_data: &SigningData,
        params: &ContractParameters,
        players: Vec<UserId>,
        ark_funding: coordinator_escrow::ark::ArkFunding,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        self.sign_dlc_batch_with(session, signing_data, params, players, Some(ark_funding))
            .await
    }

    async fn sign_ark_escrow(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        spend: coordinator_escrow::ark::ArkEscrowSpend,
    ) -> Result<Vec<(usize, [u8; 64])>, KeymeldError> {
        let mut signed = self.sign_ark_escrows(session, vec![(user, spend)]).await?;
        Ok(signed.remove(0))
    }

    async fn sign_ark_escrows(
        &self,
        session: &DlcKeygenSession,
        spends: Vec<(UserId, coordinator_escrow::ark::ArkEscrowSpend)>,
    ) -> Result<Vec<Vec<(usize, [u8; 64])>>, KeymeldError> {
        let _guard = self.lock_session(&session.session_id).await;
        let (mut state, _) = self.checkpoint(session).await?;
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let driver = self
            .connect(
                session,
                &state,
                &credentials,
                &mut journal,
                &EphemeralCheckpoint,
            )
            .await?;
        restore_roster(driver, &state.registrations).await?;
        let spends = spends
            .into_iter()
            .map(|(user, spend)| {
                let enclave = *session
                    .recipient_authorization
                    .user_enclave_assignments
                    .get(&user)
                    .ok_or_else(|| invalid("Participant enclave is missing"))?;
                Ok((enclave, (user, spend)))
            })
            .collect::<Result<Vec<_>, KeymeldError>>()?;
        // The journal is in memory only, so each enclave's spends can run on their own copy.
        let (state, credentials) = (&state, &credentials);
        per_enclave(spends, &journal, move |mut journal, spends| async move {
            let mut driver = self
                .connect(
                    session,
                    state,
                    credentials,
                    &mut journal,
                    &EphemeralCheckpoint,
                )
                .await?;
            let mut signed = Vec::with_capacity(spends.len());
            for (user, spend) in spends {
                signed.push(
                    sign_ark_spend(&mut driver, session, state, credentials, user, spend).await?,
                );
            }
            Ok(signed)
        })
        .await
    }

    async fn request_ark_refund_invoice(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        owed_sats: u64,
    ) -> Result<String, KeymeldError> {
        let (signatures, output) = self
            .sign_unbound_escrow(
                session,
                user.clone(),
                "refund-invoice",
                generic::ActionParameters::RequestArkRefundInvoice { owed_sats },
            )
            .await?;
        let [(_, signature)] = signatures.as_slice() else {
            return Err(invalid(
                "Refund invoice requires one authorization signature",
            ));
        };
        let invoice: String = output.decode()?;
        let authorization = hex::encode(signature);
        let saved_invoice = invoice.clone();
        let keygen_id = session.session_id.to_string();
        let user_id = user.to_string();
        self.database()?.execute_write(move |pool| async move {
            sqlx::query("INSERT INTO refund_invoice_authorizations (keygen_session_id, user_id, invoice, authorization) VALUES (?, ?, ?, ?) ON CONFLICT(keygen_session_id, user_id, invoice) DO UPDATE SET authorization = excluded.authorization")
                .bind(keygen_id).bind(user_id).bind(saved_invoice).bind(authorization).execute(&pool).await?;
            Ok(())
        }).await.map_err(|e| invalid(format!("Cannot persist refund invoice authorization: {e}")))?;
        Ok(invoice)
    }

    async fn sign_ark_refund(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        spend: coordinator_escrow::ark::ArkEscrowSpend,
        invoice: String,
        fee_sats: u64,
    ) -> Result<[u8; 64], KeymeldError> {
        let invoice_authorization: String = sqlx::query_scalar(
            "SELECT authorization FROM refund_invoice_authorizations WHERE keygen_session_id = ? AND user_id = ? AND invoice = ?",
        ).bind(session.session_id.to_string()).bind(user.to_string()).bind(&invoice).fetch_optional(self.database()?.read()).await
            .map_err(|e| invalid(format!("Cannot load refund invoice authorization: {e}")))?
            .ok_or_else(|| invalid("Refund invoice has no authenticated recipient receipt"))?;
        let parameters = generic::ActionParameters::RefundArkEscrow {
            spend,
            invoice,
            invoice_authorization,
            fee_sats,
        };
        let (signed, _) = self
            .sign_unbound_escrow(session, user, "refund", parameters)
            .await?;
        let [(_, signature)] = signed.as_slice() else {
            return Err(invalid("A refund signs one transaction at a time"));
        };
        Ok(*signature)
    }

    async fn sign_ark_refund_intent(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        spend: coordinator_escrow::ark::ArkEscrowSpend,
        invoice: String,
        fee_sats: u64,
    ) -> Result<Vec<(usize, [u8; 64])>, KeymeldError> {
        let invoice_authorization: String = sqlx::query_scalar(
            "SELECT authorization FROM refund_invoice_authorizations WHERE keygen_session_id = ? AND user_id = ? AND invoice = ?",
        ).bind(session.session_id.to_string()).bind(user.to_string()).bind(&invoice).fetch_optional(self.database()?.read()).await
            .map_err(|e| invalid(format!("Cannot load refund invoice authorization: {e}")))?
            .ok_or_else(|| invalid("Refund invoice has no authenticated recipient receipt"))?;
        let parameters = generic::ActionParameters::RefundArkEscrow {
            spend,
            invoice,
            invoice_authorization,
            fee_sats,
        };
        self.sign_unbound_escrow(session, user, "refund-intent", parameters)
            .await
            .map(|(signed, _)| signed)
    }

    async fn sign_ark_intent_delete(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        spend: coordinator_escrow::ark::ArkEscrowSpend,
    ) -> Result<Vec<(usize, [u8; 64])>, KeymeldError> {
        let parameters = generic::ActionParameters::DeleteArkIntent { spend };
        self.sign_unbound_escrow(session, user, "intent-delete", parameters)
            .await
            .map(|(signed, _)| signed)
    }
}

impl KeymeldService {
    /// Bind the contract for every participant. A queued competition's pool carries the oracle's
    /// signed statement of its event, from which the verifier derives each player's contract.
    async fn bind_contract(
        &self,
        session: &DlcKeygenSession,
        contract: &ContractCommitment,
        expected: &BTreeMap<UserId, PayoutPolicy>,
        statement: Option<coordinator_escrow::oracle_statement::SignedStatement>,
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
        let driver = self
            .connect(session, &state, &credentials, &mut journal, &checkpoint)
            .await?;
        let binding = generic::ContractBinding {
            statement,
            contract: contract.clone(),
        };
        let (roster, responses) = EscrowPhase {
            driver,
            session,
            state: &state,
            credentials: &credentials,
        }
        .bind_contract(binding)
        .await?;
        state.bindings = responses
            .iter()
            .map(|bound| (bound.user_id.clone(), bound.response.clone()))
            .collect();
        state.roster = Some(roster);
        state.journal = journal;
        checkpoint.finish(&state).await?;
        Ok(responses)
    }
}

impl KeymeldService {
    async fn sign_dlc_batch_with(
        &self,
        session: &DlcKeygenSession,
        signing_data: &SigningData,
        params: &ContractParameters,
        players: Vec<UserId>,
        ark_funding: Option<coordinator_escrow::ark::ArkFunding>,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        let started = std::time::Instant::now();
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
        // An Arkade attempt signs inside its batch, with a fresh plan and an in-memory journal.
        let durable = ark_funding.is_none();
        let saver: &dyn ConfidentialCheckpoint = if durable {
            &checkpoint
        } else {
            state.signing = None;
            &EphemeralCheckpoint
        };
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
            if durable {
                checkpoint.finish(&state).await?;
            }
        }
        let mut plan = state
            .signing
            .as_ref()
            .ok_or_else(|| invalid("Signing plan was not saved"))?
            .clone();
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = std::mem::take(&mut state.journal);
        let mut driver = self
            .connect(session, &state, &credentials, &mut journal, saver)
            .await?;
        let roster = driver.restore_keygen(&state.registrations).await?;
        if driver.is_signing_aborted(&plan.session_id) {
            renew_aborted_signing(driver, session, &state, &mut plan);
            state.signing = Some(plan.clone());
            state.journal = journal.clone();
            if durable {
                checkpoint.finish(&state).await?;
            }
            driver = self
                .connect(session, &state, &credentials, &mut journal, saver)
                .await?;
        }
        let signatures = if durable {
            EscrowPhase {
                driver,
                session,
                state: &state,
                credentials: &credentials,
            }
            .sign_durable_batch(
                &roster,
                &plan,
                &ark_funding,
                self.settings.signing_session_expiry_secs,
            )
            .await?
        } else {
            // Inside a batch the journal is in memory only, so each enclave's permits can run on
            // their own copy of it, side by side. Signing needs only what was journaled before
            // them, the route and the batch prepared above, so the copies are not merged back:
            // their permit commands are dropped with the journal when the batch ends.
            let setup = started.elapsed();
            let round_started = std::time::Instant::now();
            prepare_signing_round(driver, &plan).await?;
            let prepare = round_started.elapsed();
            let permits_started = std::time::Instant::now();
            let permits = state
                .policies
                .iter()
                .map(|(user, policy)| {
                    let enclave = *session
                        .recipient_authorization
                        .user_enclave_assignments
                        .get(user)
                        .ok_or_else(|| invalid("Participant enclave is missing"))?;
                    Ok((enclave, (user, policy)))
                })
                .collect::<Result<Vec<_>, KeymeldError>>()?;
            let (state, credentials, roster, plan, ark_funding) =
                (&state, &credentials, &roster, &plan, &ark_funding);
            per_enclave(permits, &journal, move |mut journal, permits| async move {
                let mut driver = self
                    .connect(session, state, credentials, &mut journal, saver)
                    .await?;
                for (user, policy) in &permits {
                    permit_contract(
                        &mut driver,
                        session,
                        state,
                        credentials,
                        roster,
                        plan,
                        ark_funding,
                        user,
                        policy,
                    )
                    .await?;
                }
                Ok(vec![(); permits.len()])
            })
            .await?;
            let permit_time = permits_started.elapsed();
            let sign_started = std::time::Instant::now();
            let driver = self
                .connect(session, state, credentials, &mut journal, saver)
                .await?;
            let signatures =
                finish_signing_batch(driver, plan, self.settings.signing_session_expiry_secs)
                    .await?;
            log::info!("Arkade DLC signing {} participants, {} messages: setup {:.3}s, prepare {:.3}s, permits {:.3}s, MuSig {:.3}s, total {:.3}s",
                state.policies.len(), plan.batch.items.len(), setup.as_secs_f64(), prepare.as_secs_f64(),
                permit_time.as_secs_f64(), sign_started.elapsed().as_secs_f64(), started.elapsed().as_secs_f64());
            signatures
        };
        if durable {
            state.roster = Some(roster);
            state.journal = journal;
            checkpoint.finish(&state).await?;
        }
        Ok(signatures)
    }
}

fn renew_aborted_signing(
    driver: ConfidentialSession<'_>,
    session: &DlcKeygenSession,
    state: &ProtocolState,
    plan: &mut SigningPlan,
) {
    // A lost nonce round cannot be resumed. Preserve the exact approved
    // messages and use the latest sealed preparation for each signer to
    // authorize a new session under explicit repetition consent.
    for user in state.policies.keys() {
        let enclave = session.recipient_authorization.user_enclave_assignments[user];
        let stage = format!("escrow/sign/prepare/{user}/{}", plan.session_id);
        if let Some(outcome) = driver.command_outcome(&stage, enclave) {
            if let EnclaveOutcome::Musig(MusigOutcome::Keygen(KeygenOutcome::Escrow(prepared))) =
                &outcome.response
            {
                plan.prior_preparations
                    .insert(user.clone(), *prepared.clone());
            }
        }
    }
    plan.session_id = SessionId::new_v7();
}

async fn prepare_signing_round(
    mut driver: ConfidentialSession<'_>,
    plan: &SigningPlan,
) -> Result<(), KeymeldError> {
    driver
        .prepare_signing_batch(&plan.session_id, &plan.batch.items)
        .await?;
    Ok(())
}

async fn finish_signing_batch(
    mut driver: ConfidentialSession<'_>,
    plan: &SigningPlan,
    expiry_secs: u64,
) -> Result<DlcSignatureResults, KeymeldError> {
    let encrypted = driver
        .sign_prepared_batch(&plan.session_id, expiry_secs, &[])
        .await?;
    let results = driver.decrypt_batch_results(&encrypted)?;
    Ok(plan.batch.parse_results(&results)?)
}

/// Admit the exact registration, releasing the driver before persisting its journal.
async fn admit_native_registration(
    mut driver: ConfidentialSession<'_>,
    user: &UserId,
    enclave: EnclaveId,
    native: &NativeRegistration,
) -> Result<(), KeymeldError> {
    // The admission is journaled before it is sent. Only an authenticated enclave rejection
    // lets the slot try again, with a corrected registration; otherwise one refused
    // registration would lock the slot for good.
    let admission = format!("admit/{user}");
    if driver.command_was_rejected(&admission, enclave) {
        driver.clear_rejected_command(&admission, enclave).await?;
    }
    driver.validate_registration(native).await?;
    Ok(())
}

async fn restore_roster(
    mut driver: ConfidentialSession<'_>,
    registrations: &BTreeMap<UserId, NativeRegistration>,
) -> Result<SignedRoster, KeymeldError> {
    Ok(driver.restore_keygen(registrations).await?)
}

/// One connected operation phase. Returning consumes its driver and ends the
/// journal borrow before the caller updates or commits protocol state.
struct EscrowPhase<'driver, 'state> {
    driver: ConfidentialSession<'driver>,
    session: &'state DlcKeygenSession,
    state: &'state ProtocolState,
    credentials: &'state SessionCredentials,
}

impl EscrowPhase<'_, '_> {
    async fn sign_durable_batch(
        mut self,
        roster: &SignedRoster,
        plan: &SigningPlan,
        ark_funding: &Option<coordinator_escrow::ark::ArkFunding>,
        expiry_secs: u64,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        let (session, state, credentials) = (self.session, self.state, self.credentials);
        self.driver
            .prepare_signing_batch(&plan.session_id, &plan.batch.items)
            .await?;
        for (user, policy) in &state.policies {
            permit_contract(
                &mut self.driver,
                session,
                state,
                credentials,
                roster,
                plan,
                ark_funding,
                user,
                policy,
            )
            .await?;
        }
        finish_signing_batch(self.driver, plan, expiry_secs).await
    }
    async fn prepare_payout(
        mut self,
        user: &UserId,
        request: PreparePayoutRequest,
        binding: &EscrowResponse,
        settlement_plan: &SettlementPlan,
    ) -> Result<PayoutPreparedResponse, KeymeldError> {
        let (session, state, credentials) = (self.session, self.state, self.credentials);
        self.driver.restore_keygen(&state.registrations).await?;
        let attempt = ActionAttempt {
            attempt_id: request.claim_id,
            signing_session_id: None,
        };
        let parameters = Payload::encode(&generic::ActionParameters::PrepareSettlement {
            claim_id: request.claim_id,
            contract_signatures: request.contract_signatures,
            attestation: request.attestation,
            method: request.method,
            ark_funding: request.ark_funding,
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
            &mut self.driver,
            session,
            state,
            credentials,
            user,
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
            &mut self.driver,
            session,
            state,
            credentials,
            user,
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
        verify_prepared_payout_origin(session, user, &response)?;
        Ok(response)
    }
    async fn release_payout(
        mut self,
        service: &KeymeldService,
        user: &UserId,
        claim_id: Uuid,
        receipts: &PreparedPayoutReceipts,
        evidence: &generic::PaymentEvidence,
    ) -> Result<BTreeMap<&'static str, Zeroizing<String>>, KeymeldError> {
        let (session, state, credentials) = (self.session, self.state, self.credentials);
        self.driver.restore_keygen(&state.registrations).await?;
        let mut released = BTreeMap::new();
        for (permission, prepared) in [
            (generic::RELEASE_PREIMAGE, &receipts.preimage_preparation),
            (generic::RELEASE_ENTRY_KEY, &receipts.key_preparation),
        ] {
            let execute = ExecuteEscrowRequest {
                schema_version: escrow::SCHEMA_VERSION,
                prepared_receipt: prepared.sealed_state.clone(),
                proof: ConditionProof::VerifierEvidence {
                    evidence: Payload::encode(evidence)?,
                },
            };
            let response = escrow_request(
                &mut self.driver,
                session,
                state,
                credentials,
                user,
                &format!("escrow/execute/{user}/{}/{permission}", claim_id),
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
                    && public_key == &state.policies[user].policy.participant_public_key =>
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
                != service.user_credentials.public_key_bytes()
            {
                return Err(invalid("Released escrow belongs to a different recipient"));
            }
            let value = service
                .user_credentials
                .decrypt_ecies(ciphertext.as_bytes())?;
            if value.len() != 32 {
                return Err(invalid("Invalid released secret length"));
            }
            released.insert(permission, Zeroizing::new(hex::encode(&*value)));
        }
        Ok(released)
    }
    async fn bind_contract(
        mut self,
        binding: generic::ContractBinding,
    ) -> Result<(SignedRoster, Vec<PayoutContractBoundResponse>), KeymeldError> {
        let (session, state, credentials) = (self.session, self.state, self.credentials);
        let roster = self.driver.restore_keygen(&state.registrations).await?;
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
                &mut self.driver,
                session,
                state,
                credentials,
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
        Ok((roster, responses))
    }
}

#[cfg(test)]
#[path = "confidential_service_tests.rs"]
mod tests;
