//! Sessions whose registrations are key deposits: a queued competition's players deposit their
//! entry keys under the competition's terms before any session exists. Keymeld registers a deposit
//! in any session whose manifest names the same deposit scope, on the enclave it was sealed to.
//!
//! - Checking a deposit at entry: a manifest made only for the check, holding the coordinator and
//!   the ticket, with refund evidence. The enclave checks the sealed registration statelessly;
//!   nothing is stored.
//! - A pool: the coordinator and the pool's members, with evidence of how the pool was formed.
//! - Refunds of a queue that never formed, or of tickets no pool took: the coordinator and those
//!   tickets, with refund evidence, under which nothing can be bound.

use super::*;
use keymeld_core::authorization::DepositScope;

fn deposit_scope(scope: &DepositScopeRequest) -> DepositScope {
    DepositScope {
        deposit_session_id: scope.deposit_session_id.clone(),
        deposit_digest: scope.deposit_digest.to_vec(),
        evidence: scope.evidence.clone(),
    }
}

impl KeymeldService {
    /// A new deposit-scoped session: the coordinator and `members`, each member on the enclave
    /// its deposit was sealed to and the coordinator on the first member's, with each enclave's
    /// current key and epoch pinned. Nothing is sent to Keymeld or stored.
    async fn new_deposit_session(
        &self,
        id: SessionId,
        scope: &DepositScopeRequest,
        members: &[(UserId, EnclaveId)],
        subsets: DlcSubsetInfo,
    ) -> Result<(DlcKeygenSession, BTreeMap<EnclaveId, u64>), KeymeldError> {
        let client = self.get_client()?;
        let (_, coordinator_enclave) = members
            .first()
            .ok_or_else(|| invalid("A deposit session needs a member"))?;
        let mut assignments =
            BTreeMap::from([(self.coordinator_user_id.clone(), *coordinator_enclave)]);
        for (user, enclave) in members {
            if assignments.insert(user.clone(), *enclave).is_some() {
                return Err(invalid("Duplicate deposit session participant"));
            }
        }
        let credentials = SessionCredentials::generate()?;
        let authority = AuthorizationCredentials::generate()?;
        let mut registration_authorities = BTreeMap::new();
        for user in assignments.keys() {
            registration_authorities.insert(user.clone(), AuthorizationCredentials::generate()?);
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
                encrypted_taproot_tweak: credentials.encrypt(
                    &serde_json::to_vec(&TaprootTweak::None)
                        .map_err(|error| invalid(error.to_string()))?,
                    "taproot_tweak",
                )?,
                subset_definitions: subsets
                    .definitions
                    .iter()
                    .map(|subset| keymeld_core::protocol::SubsetDefinition {
                        subset_id: subset.subset_id,
                        participants: subset.participants.clone(),
                    })
                    .collect(),
                deposit_scope: Some(deposit_scope(scope)),
            },
            &authority.export_secret(),
        )?;
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
            session_id: id,
            session_secret: credentials.export_session_secret(),
            authorization_manifest: manifest,
            recipient_authorization: recipients,
            signing_authority: authority,
            registration_authorities,
            aggregate_key: Vec::new(),
            outcome_subset_ids: subsets.outcome_subset_ids,
        };
        Ok((session, epochs))
    }

    /// The coordinator's own registration in a deposit-scoped session, bound to the scope as
    /// every registration of the session is.
    fn coordinator_deposit_registration(
        &self,
        session: &DlcKeygenSession,
        epochs: &BTreeMap<EnclaveId, u64>,
    ) -> Result<NativeRegistration, KeymeldError> {
        let (scope_id, scope_digest) = session
            .authorization_manifest
            .registration_scope()
            .map_err(SdkError::from)?;
        let enclave = *session
            .recipient_authorization
            .user_enclave_assignments
            .get(&self.coordinator_user_id)
            .ok_or_else(|| invalid("The coordinator has no enclave in the deposit session"))?;
        let context = RegistrationContext {
            keygen_session_id: scope_id.clone(),
            manifest_hash: scope_digest,
            user_id: self.coordinator_user_id.clone(),
            enclave_id: enclave,
            enclave_key_epoch: *epochs
                .get(&enclave)
                .ok_or_else(|| invalid("The coordinator's enclave has no pinned epoch"))?,
            public_key: self.user_credentials.public_key_bytes(),
            auth_pubkey: self
                .user_credentials
                .derive_session_auth_pubkey(&scope_id.to_string())?,
            require_signing_approval: false,
        };
        // A deposit-scoped session accepts only envelopes sealed as deposits, the coordinator's
        // own included.
        let data = ParticipantRegistrationData {
            encrypted_private_key: self.user_credentials.prepare_deposit_registration(
                context.clone(),
                &hex::encode(&session.recipient_authorization.recipient_public_keys[&enclave]),
            )?,
            public_key: hex::encode(&context.public_key),
            auth_pubkey: hex::encode(&context.auth_pubkey),
            context,
            payout_policy: None,
            escrow_policy: None,
        };
        Self::native_registration(session, &self.coordinator_user_id, &data)
    }

    /// Have the enclave a deposit was sealed to check it, under a manifest made for the check.
    pub(super) async fn check_deposit(
        &self,
        scope: DepositScopeRequest,
        user: UserId,
        data: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        let enclave = data.context.enclave_id;
        let (session, epochs) = self
            .new_deposit_session(
                SessionId::new_v7(),
                &scope,
                &[(user.clone(), enclave)],
                DlcSubsetInfo {
                    definitions: vec![],
                    outcome_subset_ids: BTreeMap::new(),
                },
            )
            .await?;
        if epochs.get(&enclave) != Some(&data.context.enclave_key_epoch) {
            return Err(invalid(
                "The deposit was sealed to an enclave key that has changed",
            ));
        }
        let native = Self::native_registration(&session, &user, data)?;
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let mut journal = ConfidentialJournal::default();
        let mut driver = ConfidentialSession::connect(
            self.get_client()?,
            &session.authorization_manifest,
            &session.recipient_authorization,
            &epochs,
            &credentials,
            &session.signing_authority,
            &session.signing_authority,
            &mut journal,
            &EphemeralCheckpoint,
        )
        .await?;
        driver.validate_registration(&native).await?;
        Ok(())
    }

    /// Create, or on a retry load, a deposit-scoped session and its durable protocol state, with
    /// the coordinator registered.
    pub(super) async fn create_deposit_session(
        &self,
        session_id: Uuid,
        scope: DepositScopeRequest,
        members: Vec<(UserId, EnclaveId)>,
        subsets: DlcSubsetInfo,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        let id = SessionId::from(session_id);
        let _guard = self.lock_session(&id).await;
        let db = self.database()?;
        let keys = self.storage_keys()?;
        if let Some(Loaded { state, .. }) =
            confidential_store::load(db, &self.store_key, &id).await?
        {
            let session = state.session.to_session(&keys)?;
            let manifest = &session.authorization_manifest.manifest;
            let assignments = &session.recipient_authorization.user_enclave_assignments;
            // Subset ids are fresh on every attempt; their participants must not change.
            let same_subsets = manifest.subset_definitions.len() == subsets.definitions.len()
                && manifest
                    .subset_definitions
                    .iter()
                    .zip(&subsets.definitions)
                    .all(|(stored, wanted)| stored.participants == wanted.participants);
            if manifest.deposit_scope.as_ref() != Some(&deposit_scope(&scope))
                || manifest.participant_verifiers.len() != members.len() + 1
                || members
                    .iter()
                    .any(|(user, enclave)| assignments.get(user) != Some(enclave))
                || !same_subsets
            {
                return Err(invalid(
                    "Deposit session retry changed its scope, members or subsets",
                ));
            }
            return Ok(session);
        }
        let (session, epochs) = self
            .new_deposit_session(id.clone(), &scope, &members, subsets)
            .await?;
        let native = self.coordinator_deposit_registration(&session, &epochs)?;
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
}
