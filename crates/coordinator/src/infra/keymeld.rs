use crate::config::KeymeldSettings;
use async_trait::async_trait;
use coordinator_core::RegistrationAssignment;
use dlctix::{Outcome, OutcomeIndex, PayoutWeights};
use keymeld_core::authorization::EnclaveRecipientAuthorization;
pub use keymeld_sdk::types::SubsetDefinition;
use keymeld_sdk::{
    dlctix::{
        dlctix::{ContractParameters, SigningData},
        DlcBatchBuilder, DlcSignatureResults,
    },
    prelude::*,
    types::{
        RegisterKeygenParticipantRequest, RegistrationAuthorization, RegistrationContext,
        SignedRoster, SignedSessionManifest,
    },
    PollingConfig,
};
use log::{debug, error, info};
use nostr_sdk::{nips::nip44, Keys};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

/// Error type for Keymeld operations
#[derive(Debug, thiserror::Error)]
pub enum KeymeldError {
    #[error("Keymeld SDK error: {0}")]
    Sdk(#[from] SdkError),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("Signing error: {0}")]
    Signing(String),

    #[error("Keymeld is not enabled")]
    NotEnabled,
}

/// Status of a keygen session for polling
#[derive(Debug, Clone)]
pub struct KeygenSessionStatus {
    pub session_id: String,
    pub status: String,
    pub is_completed: bool,
}

/// Data required to register a participant on their behalf
/// The user generates this data client-side and submits it with their entry
#[derive(Debug, Clone)]
pub struct ParticipantRegistrationData {
    /// User's ephemeral private key encrypted to the keymeld enclave's public key
    pub encrypted_private_key: String,
    /// User's ephemeral public key (hex-encoded, will be decoded to bytes)
    pub public_key: String,
    /// User's auth public key derived from their ephemeral private key (hex-encoded)
    /// This is derived client-side using: derive_session_auth_pubkey(session_id)
    pub auth_pubkey: String,
    pub context: RegistrationContext,
}

/// Pre-computed DLC subset definitions for keygen session creation.
/// Generated at competition creation time based on ranking permutations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlcSubsetInfo {
    /// Subset definitions to pass to keygen session creation
    pub definitions: Vec<SubsetDefinition>,
    /// Mapping from outcome index to subset ID (needed for signing)
    pub outcome_subset_ids: BTreeMap<OutcomeIndex, Uuid>,
}

/// Select an authorized subset by its actual payout recipients. Oracle outcomes
/// use entry-ID order, while participant slots were allocated earlier in ticket order.
fn outcome_subsets_for_payouts(
    payouts: &BTreeMap<Outcome, PayoutWeights>,
    player_user_ids: &[UserId],
    coordinator: &UserId,
    definitions: &[keymeld_core::protocol::SubsetDefinition],
) -> Result<BTreeMap<OutcomeIndex, Uuid>, KeymeldError> {
    let mut authorized = BTreeMap::new();
    for definition in definitions {
        let mut participants = definition.participants.clone();
        participants.sort();
        authorized
            .entry(participants)
            .or_insert(definition.subset_id);
    }
    let mut subsets = BTreeMap::new();
    for (outcome, weights) in payouts {
        let Outcome::Attestation(index) = outcome else {
            continue;
        };
        let mut participants = vec![coordinator.clone()];
        for (&player, &weight) in weights {
            if weight > 0 {
                participants.push(
                    player_user_ids
                        .get(player)
                        .ok_or_else(|| KeymeldError::Signing("Invalid payout player index".into()))?
                        .clone(),
                );
            }
        }
        participants.sort();
        let subset = authorized.get(&participants).ok_or_else(|| {
            KeymeldError::Signing(format!(
                "No authorized subset for outcome {index} recipients"
            ))
        })?;
        subsets.insert(*index, *subset);
    }
    Ok(subsets)
}

/// Trait for Keymeld signing operations
#[async_trait]
pub trait Keymeld: Send + Sync {
    /// Initialize a keygen session at competition creation time with subset definitions.
    /// This creates the session with all participant user_ids (ticket_ids), subset definitions,
    /// and registers the coordinator.
    ///
    /// The subset_info contains pre-computed subset definitions based on ranking permutations.
    /// These MUST be provided at keygen time - they cannot be added later.
    async fn init_keygen_session(
        &self,
        competition_id: Uuid,
        player_user_ids: Vec<UserId>,
        subset_info: DlcSubsetInfo,
    ) -> Result<DlcKeygenSession, KeymeldError>;

    /// Register a participant on their behalf using pre-computed registration data.
    /// The user generates the crypto material client-side and submits it with their entry.
    /// The coordinator forwards this data to keymeld to complete the registration.
    async fn register_participant(
        &self,
        session: &DlcKeygenSession,
        user_id: UserId,
        registration_data: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError>;

    /// Wait for keygen to complete and get the aggregate key
    /// Called after all participants have registered
    async fn wait_for_keygen_completion(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<SignedRoster, KeymeldError>;

    /// Get the status of a keygen session (for polling registrations)
    async fn get_keygen_status(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<KeygenSessionStatus, KeymeldError>;

    /// Sign a DLC contract batch.
    /// This builds subset definitions from contract_params at signing time.
    async fn sign_dlc_batch(
        &self,
        keygen_session: &DlcKeygenSession,
        signing_data: &SigningData,
        contract_params: &ContractParameters,
        player_user_ids: Vec<UserId>,
    ) -> Result<DlcSignatureResults, KeymeldError>;

    /// Check if Keymeld is enabled
    fn is_enabled(&self) -> bool;

    /// Get the coordinator's user ID for subset definitions
    fn coordinator_user_id(&self) -> UserId;

    /// Get a user's assigned enclave public key for a keygen session.
    /// Users need this to encrypt their private keys for registration.
    /// Each user is assigned to a specific enclave when the session is created.
    async fn get_registration_assignment(
        &self,
        session: &DlcKeygenSession,
        user_id: UserId,
    ) -> Result<RegistrationAssignment, KeymeldError>;
}

/// Holds private authority credentials; never format or serialize the live session.
#[derive(Clone)]
pub struct DlcKeygenSession {
    pub session_id: SessionId,
    pub session_secret: [u8; 32],
    pub authorization_manifest: SignedSessionManifest,
    pub recipient_authorization: EnclaveRecipientAuthorization,
    pub signing_authority: AuthorizationCredentials,
    pub registration_authorities: BTreeMap<UserId, AuthorizationCredentials>,
    pub aggregate_key: Vec<u8>,
    /// Mapping from outcome index to subset ID - needed for signing.
    /// Set at keygen creation time from DlcSubsetInfo.
    pub outcome_subset_ids: BTreeMap<OutcomeIndex, Uuid>,
}

/// Serializable version of DlcKeygenSession for database storage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDlcKeygenSession {
    /// Session ID as string (UUID format)
    pub session_id: String,
    /// Session secret encrypted with NIP-44 to coordinator's nostr pubkey
    /// This ensures the secret is never stored in plaintext
    pub encrypted_session_secret: String,
    pub authorization_manifest: SignedSessionManifest,
    pub recipient_authorization: EnclaveRecipientAuthorization,
    pub encrypted_signing_authority: String,
    pub encrypted_registration_authorities: BTreeMap<UserId, String>,
    /// Aggregate public key bytes (hex encoded)
    #[serde(with = "hex_vec")]
    pub aggregate_key: Vec<u8>,
    /// Mapping from outcome index to subset ID - needed for signing
    #[serde(default)]
    pub outcome_subset_ids: BTreeMap<OutcomeIndex, Uuid>,
}

impl StoredDlcKeygenSession {
    pub fn from_session(session: &DlcKeygenSession, keys: &Keys) -> Result<Self, KeymeldError> {
        session.validate_credentials()?;
        let encrypt = |secret: &[u8; 32]| {
            nip44::encrypt(
                keys.secret_key(),
                &keys.public_key(),
                hex::encode(secret),
                nip44::Version::V2,
            )
            .map_err(|e| {
                KeymeldError::Session(format!("Failed to encrypt session credentials: {e}"))
            })
        };
        Ok(Self {
            session_id: session.session_id.to_string(),
            encrypted_session_secret: encrypt(&session.session_secret)?,
            authorization_manifest: session.authorization_manifest.clone(),
            recipient_authorization: session.recipient_authorization.clone(),
            encrypted_signing_authority: encrypt(&session.signing_authority.export_secret())?,
            encrypted_registration_authorities: session
                .registration_authorities
                .iter()
                .map(|(user, credential)| Ok((user.clone(), encrypt(&credential.export_secret())?)))
                .collect::<Result<_, KeymeldError>>()?,
            aggregate_key: session.aggregate_key.clone(),
            outcome_subset_ids: session.outcome_subset_ids.clone(),
        })
    }

    pub fn to_session(&self, keys: &Keys) -> Result<DlcKeygenSession, KeymeldError> {
        let decrypt = |encrypted: &str| -> Result<[u8; 32], KeymeldError> {
            let secret =
                nip44::decrypt(keys.secret_key(), &keys.public_key(), encrypted).map_err(|e| {
                    KeymeldError::Session(format!("Failed to decrypt session credentials: {e}"))
                })?;
            hex::decode(secret)
                .map_err(|e| KeymeldError::Session(e.to_string()))?
                .try_into()
                .map_err(|_| KeymeldError::Session("Invalid credential length".into()))
        };
        let session = DlcKeygenSession {
            session_id: SessionId::new(&self.session_id),
            session_secret: decrypt(&self.encrypted_session_secret)?,
            authorization_manifest: self.authorization_manifest.clone(),
            recipient_authorization: self.recipient_authorization.clone(),
            signing_authority: AuthorizationCredentials::from_secret(&decrypt(
                &self.encrypted_signing_authority,
            )?)?,
            registration_authorities: self
                .encrypted_registration_authorities
                .iter()
                .map(|(user, encrypted)| {
                    Ok((
                        user.clone(),
                        AuthorizationCredentials::from_secret(&decrypt(encrypted)?)?,
                    ))
                })
                .collect::<Result<_, KeymeldError>>()?,
            aggregate_key: self.aggregate_key.clone(),
            outcome_subset_ids: self.outcome_subset_ids.clone(),
        };
        session.validate_credentials()?;
        Ok(session)
    }
}

impl DlcKeygenSession {
    fn validate_credentials(&self) -> Result<(), KeymeldError> {
        self.recipient_authorization
            .verify(&self.authorization_manifest)
            .map_err(SdkError::from)?;
        let manifest = &self.authorization_manifest.manifest;
        let credentials = SessionCredentials::from_session_secret(&self.session_secret)?;
        if manifest.keygen_session_id != self.session_id
            || manifest.session_public_key != credentials.public_key_bytes()
            || manifest.signing_pubkey != self.signing_authority.public_key_bytes()
            || manifest.participant_verifiers.len() != self.registration_authorities.len()
            || self
                .registration_authorities
                .iter()
                .any(|(user, credential)| {
                    manifest.participant_verifiers.get(user) != Some(&credential.public_key_bytes())
                })
        {
            return Err(KeymeldError::Session(
                "Stored credentials do not match the pinned manifest".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_registration(
        &self,
        user_id: &UserId,
        data: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        let context = &data.context;
        let expected_key =
            hex::decode(&data.public_key).map_err(|e| KeymeldError::Session(e.to_string()))?;
        let expected_auth =
            hex::decode(&data.auth_pubkey).map_err(|e| KeymeldError::Session(e.to_string()))?;
        if context.keygen_session_id != self.session_id
            || &context.user_id != user_id
            || context.manifest_hash
                != self
                    .authorization_manifest
                    .digest()
                    .map_err(SdkError::from)?
            || self
                .recipient_authorization
                .user_enclave_assignments
                .get(user_id)
                != Some(&context.enclave_id)
            || context.public_key != expected_key
            || context.auth_pubkey != expected_auth
            || context.require_signing_approval
        {
            return Err(KeymeldError::Session(
                "Registration context does not match the accepted entry and authorized slot".into(),
            ));
        }
        Ok(())
    }

    fn verify_restored_recipients(&self, restored: &KeygenSession<'_>) -> Result<(), KeymeldError> {
        if restored.recipient_authorization() != Some(&self.recipient_authorization) {
            return Err(KeymeldError::Session(
                "Gateway changed the pinned enclave recipients".into(),
            ));
        }
        Ok(())
    }
}

/// Hex serialization for variable-size byte vectors
mod hex_vec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        hex::decode(&s).map_err(serde::de::Error::custom)
    }
}

/// Production Keymeld service implementation
pub struct KeymeldService {
    settings: KeymeldSettings,
    client: Option<KeyMeldClient>,
    coordinator_user_id: UserId,
}

impl KeymeldService {
    pub fn new(
        settings: KeymeldSettings,
        coordinator_user_id: Uuid,
        coordinator_private_key: &[u8; 32],
    ) -> Result<Self, KeymeldError> {
        if !settings.enabled {
            return Ok(Self {
                settings,
                client: None,
                coordinator_user_id: UserId::from(coordinator_user_id),
            });
        }

        let user_id = UserId::from(coordinator_user_id);
        let credentials = UserCredentials::from_private_key(coordinator_private_key)
            .map_err(|e| KeymeldError::Config(format!("Failed to create credentials: {}", e)))?;

        let polling_config = PollingConfig {
            max_attempts: settings.max_polling_attempts,
            initial_delay: Duration::from_millis(settings.initial_polling_delay_ms),
            max_delay: Duration::from_millis(settings.max_polling_delay_ms),
            backoff_multiplier: settings.polling_backoff_multiplier,
            jitter: 0.25,
        };

        let builder = KeyMeldClient::builder(&settings.gateway_url, user_id.clone())
            .credentials(credentials)
            .polling_config(polling_config);
        let builder = if settings.dangerous_trust_unattested_enclaves {
            if !settings.trusted_pcrs.is_empty() {
                return Err(KeymeldError::Config(
                    "Trusted enclave measurements and unattested trust are mutually exclusive"
                        .into(),
                ));
            }
            // Simulated enclaves produce no Nitro evidence. Settings validation
            // refuses this on mainnet before the service is built.
            builder.dangerous_trust_unattested_enclaves()
        } else {
            let measurements = settings
                .trusted_pcrs
                .iter()
                .map(|(pcr, value)| (pcr.to_string(), value.clone()))
                .collect();
            let attestation =
                AttestationPolicy::from_hex_measurements(&measurements).map_err(|e| {
                    KeymeldError::Config(format!("Invalid trusted enclave measurements: {e}"))
                })?;
            builder.attestation_policy(attestation)
        };
        let client = builder
            .build()
            .map_err(|e| KeymeldError::Config(format!("Failed to build client: {}", e)))?;

        Ok(Self {
            settings,
            client: Some(client),
            coordinator_user_id: user_id,
        })
    }

    fn get_client(&self) -> Result<&KeyMeldClient, KeymeldError> {
        self.client.as_ref().ok_or(KeymeldError::NotEnabled)
    }
}

#[async_trait]
impl Keymeld for KeymeldService {
    fn is_enabled(&self) -> bool {
        self.settings.enabled && self.client.is_some()
    }

    fn coordinator_user_id(&self) -> UserId {
        self.coordinator_user_id.clone()
    }

    async fn init_keygen_session(
        &self,
        competition_id: Uuid,
        player_user_ids: Vec<UserId>,
        subset_info: DlcSubsetInfo,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        let client = self.get_client()?;

        info!(
            "Initializing keygen session for competition {} with {} players and {} subset definitions",
            competition_id,
            player_user_ids.len(),
            subset_info.definitions.len()
        );

        // All participants: coordinator + all players
        let mut all_participants = vec![self.coordinator_user_id.clone()];
        all_participants.extend(player_user_ids);
        let participant_ids = all_participants.clone();

        let keygen_options = KeygenOptions::default()
            .timeout(self.settings.keygen_session_expiry_secs)
            .tweak(TaprootTweak::None)
            .approval(false);

        // Create the keygen session WITH subset definitions
        // These subsets define which participants sign each outcome's split transactions
        let mut keygen_session = client
            .keygen()
            .create_session_with_subsets(all_participants, subset_info.definitions, keygen_options)
            .await?;

        // Register ourselves (coordinator) as a participant
        keygen_session
            .register_self(RegisterOptions::default().approval(false))
            .await?;

        let session_id = keygen_session.session_id().clone();
        let session_secret = keygen_session.export_session_secret();
        let authorization_manifest = keygen_session.authorization_manifest().clone();
        let recipient_authorization = keygen_session
            .recipient_authorization()
            .cloned()
            .ok_or_else(|| {
                KeymeldError::Session("Missing enclave recipient authorization".into())
            })?;
        let signing_authority = keygen_session
            .authorization_credentials()
            .cloned()
            .ok_or_else(|| KeymeldError::Session("Missing signing authority".into()))?;
        let registration_authorities = participant_ids
            .into_iter()
            .map(|user| {
                let credential = keygen_session
                    .registration_credentials(&user)
                    .cloned()
                    .ok_or_else(|| {
                        KeymeldError::Session(format!("Missing registration authority for {user}"))
                    })?;
                Ok((user, credential))
            })
            .collect::<Result<_, KeymeldError>>()?;

        info!(
            "Keygen session {} initialized with {} subsets, waiting for user registrations",
            session_id,
            subset_info.outcome_subset_ids.len()
        );

        // Verify the session exists on the server before returning
        // This catches cases where the SDK returns success but the server didn't persist
        let credentials = SessionCredentials::from_session_secret(&session_secret)?;
        let restored = client
            .keygen()
            .restore_session(
                session_id.clone(),
                credentials,
                authorization_manifest.clone(),
            )
            .await
            .map_err(|e| {
                error!(
                    "Failed to verify keygen session {} exists on server: {}",
                    session_id, e
                );
                e
            })?;

        if restored.recipient_authorization() != Some(&recipient_authorization) {
            return Err(KeymeldError::Session(
                "Gateway changed the initialized enclave recipients".into(),
            ));
        }
        info!(
            "Verified keygen session {} exists on server with status: {}",
            session_id,
            restored.status().as_ref()
        );

        // Return immediately without waiting for other participants
        // The aggregate_key will be empty until completion - it's set when wait_for_keygen_completion is called
        Ok(DlcKeygenSession {
            session_id,
            session_secret,
            authorization_manifest,
            recipient_authorization,
            signing_authority,
            registration_authorities,
            aggregate_key: vec![], // Will be populated when keygen completes
            outcome_subset_ids: subset_info.outcome_subset_ids,
        })
    }

    async fn wait_for_keygen_completion(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<SignedRoster, KeymeldError> {
        let client = self.get_client()?;

        info!(
            "Waiting for keygen session {} to complete",
            session.session_id
        );

        // Restore session from credentials
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;

        let mut restored_session = client
            .keygen()
            .restore_session(
                session.session_id.clone(),
                credentials,
                session.authorization_manifest.clone(),
            )
            .await?;

        session.verify_restored_recipients(&restored_session)?;

        // Wait for all participants to register and keygen to complete
        let _aggregate_key = restored_session.wait_for_completion().await?;

        let roster = restored_session.verify_roster()?;

        info!(
            "Keygen session {} completed with aggregate key",
            session.session_id
        );

        Ok(roster)
    }

    async fn sign_dlc_batch(
        &self,
        keygen_session: &DlcKeygenSession,
        signing_data: &SigningData,
        contract_params: &ContractParameters,
        player_user_ids: Vec<UserId>,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        let client = self.get_client()?;

        info!(
            "Creating DLC signing session for keygen {} with {} outcome subsets",
            keygen_session.session_id,
            keygen_session.outcome_subset_ids.len()
        );

        // Restore the keygen session from stored credentials
        let credentials = SessionCredentials::from_session_secret(&keygen_session.session_secret)?;

        let restored_keygen = client
            .keygen()
            .restore_session_with_authority(
                keygen_session.session_id.clone(),
                credentials,
                keygen_session.authorization_manifest.clone(),
                keygen_session.signing_authority.clone(),
            )
            .await?;

        keygen_session.verify_restored_recipients(&restored_keygen)?;
        restored_keygen.verify_roster()?;

        // Reuse the authorized participant subsets with the contract's actual winners.
        // Entries can be submitted in a different order than their ticket allocation.
        let outcome_subsets = outcome_subsets_for_payouts(
            &contract_params.outcome_payouts,
            &player_user_ids,
            &keygen_session
                .authorization_manifest
                .manifest
                .coordinator_user_id,
            &keygen_session
                .authorization_manifest
                .manifest
                .subset_definitions,
        )?;
        let dlc_batch = DlcBatchBuilder::new(signing_data)
            .with_outcome_subsets(&outcome_subsets)
            .build()
            .map_err(|e| KeymeldError::Signing(format!("Failed to build batch: {}", e)))?;

        let signing_options =
            SigningOptions::default().timeout(self.settings.signing_session_expiry_secs);

        // Create signing session - need to clone items since sign_batch consumes them
        let mut attempts = 0;
        let mut signing_session = loop {
            attempts += 1;
            match client
                .signer()
                .sign_batch(
                    &restored_keygen,
                    dlc_batch.items.clone(),
                    signing_options.clone(),
                )
                .await
            {
                Ok(session) => break session,
                Err(SdkError::Api(keymeld_sdk::ApiError::RateLimited { retry_after_secs }))
                    if attempts < 3 && retry_after_secs <= 30 =>
                {
                    tokio::time::sleep(Duration::from_secs(retry_after_secs)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };

        debug!(
            "Signing session {} created, waiting for completion",
            signing_session.session_id()
        );

        // Wait for signing to complete
        let signature_results = signing_session.wait_for_completion().await?;

        // Parse results into DLC signature format
        let dlc_signatures = dlc_batch.parse_results(&signature_results)?;

        info!(
            "Signing session {} completed with {} outcome signatures and {} split signatures",
            signing_session.session_id(),
            dlc_signatures.outcome_signatures.len(),
            dlc_signatures.split_signatures.len()
        );

        Ok(dlc_signatures)
    }

    async fn get_keygen_status(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<KeygenSessionStatus, KeymeldError> {
        let client = self.get_client()?;

        // Restore session credentials to make authenticated status request
        // This calls /api/v1/keygen/{session_id}/status which returns the current status
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;

        let restored_session = client
            .keygen()
            .restore_session(
                session.session_id.clone(),
                credentials,
                session.authorization_manifest.clone(),
            )
            .await?;

        session.verify_restored_recipients(&restored_session)?;
        let status_kind = restored_session.status();
        let is_completed = matches!(
            status_kind,
            keymeld_sdk::prelude::KeygenStatusKind::Completed
        );

        Ok(KeygenSessionStatus {
            session_id: session.session_id.to_string(),
            status: status_kind.as_ref().to_string(),
            is_completed,
        })
    }

    async fn register_participant(
        &self,
        session: &DlcKeygenSession,
        user_id: UserId,
        data: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        session.validate_registration(&user_id, data)?;
        let client = self.get_client()?;
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let slots = client
            .keygen()
            .get_available_slots(&session.session_id, &credentials)
            .await?;
        let slot = slots
            .available_slots
            .iter()
            .find(|slot| slot.user_id == user_id)
            .ok_or_else(|| KeymeldError::Session("Participant slot is missing".into()))?;
        if slot.enclave_id != data.context.enclave_id {
            return Err(KeymeldError::Session(
                "Gateway changed participant assignment".into(),
            ));
        }
        let enclave = client
            .health()
            .get_enclave_key(slot.enclave_id.as_u32())
            .await?;
        if enclave.key_epoch != data.context.enclave_key_epoch
            || session
                .recipient_authorization
                .recipient_public_keys
                .get(&slot.enclave_id)
                != Some(
                    &hex::decode(&enclave.public_key)
                        .map_err(|e| KeymeldError::Session(e.to_string()))?,
                )
        {
            return Err(KeymeldError::Session(
                "Enclave key or epoch changed; a fresh registration envelope is required".into(),
            ));
        }
        let authority = session
            .registration_authorities
            .get(&user_id)
            .ok_or_else(|| KeymeldError::Session("Registration authority is missing".into()))?;
        let registration_authorization = RegistrationAuthorization::sign(
            &authority.export_secret(),
            data.context.clone(),
            &data.encrypted_private_key,
        )
        .map_err(SdkError::from)?;
        let encrypted_session_data = credentials.encrypt(
            &serde_json::to_vec(&serde_json::json!({"participant_public_keys": {user_id.to_string(): data.context.public_key}}))
                .map_err(|e| KeymeldError::Session(e.to_string()))?,
            "keygen_participant_session",
        )?;
        let request = RegisterKeygenParticipantRequest {
            registration_authorization,
            keygen_session_id: session.session_id.clone(),
            user_id: user_id.clone(),
            encrypted_private_key: data.encrypted_private_key.clone(),
            public_key: data.context.public_key.clone(),
            encrypted_session_data,
            enclave_public_key: enclave.public_key,
            enclave_key_epoch: data.context.enclave_key_epoch,
            require_signing_approval: false,
            auth_pubkey: data.context.auth_pubkey.clone(),
        };
        for attempt in 0..3 {
            let signature = credentials.sign_session_request(&session.session_id.to_string())?;
            let result = client
                .http()
                .post::<_, keymeld_sdk::types::RegisterKeygenParticipantResponse>(
                    &format!(
                        "{}/api/v1/keygen/{}/participants",
                        self.settings.gateway_url, session.session_id
                    ),
                    &request,
                    &[("X-Session-Signature", &signature)],
                )
                .await;
            match result {
                Ok(response) => {
                    if response.keygen_session_id != session.session_id
                        || response.user_id != user_id
                        || response.assigned_enclave_id != data.context.enclave_id
                        || response.require_signing_approval
                    {
                        return Err(KeymeldError::Session(
                            "Gateway returned a different registration".into(),
                        ));
                    }
                    return Ok(());
                }
                Err(SdkError::Api(keymeld_sdk::ApiError::RateLimited { retry_after_secs }))
                    if attempt < 2 && retry_after_secs <= 30 =>
                {
                    tokio::time::sleep(Duration::from_secs(retry_after_secs)).await;
                }
                Err(error) => {
                    // An uncertain HTTP outcome is successful only when the signed roster confirms this exact envelope.
                    let restored = client
                        .keygen()
                        .restore_session(
                            session.session_id.clone(),
                            SessionCredentials::from_session_secret(&session.session_secret)?,
                            session.authorization_manifest.clone(),
                        )
                        .await;
                    if let Ok(restored) = restored {
                        session.verify_restored_recipients(&restored)?;
                        if matches!(restored.status(), KeygenStatusKind::Completed) {
                            let roster = restored.verify_roster()?;
                            if let Some(accepted) = roster.roster.registrations.get(&user_id) {
                                if accepted.context == data.context
                                    && accepted.ciphertext_hash
                                        == request.registration_authorization.ciphertext_hash
                                {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    return Err(error.into());
                }
            }
        }
        Err(KeymeldError::Session(
            "Registration retry budget exhausted".into(),
        ))
    }

    async fn get_registration_assignment(
        &self,
        session: &DlcKeygenSession,
        user_id: UserId,
    ) -> Result<RegistrationAssignment, KeymeldError> {
        let client = self.get_client()?;
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;
        let slots = client
            .keygen()
            .get_available_slots(&session.session_id, &credentials)
            .await?;
        let slot = slots
            .available_slots
            .iter()
            .find(|slot| slot.user_id == user_id && !slot.claimed)
            .ok_or_else(|| KeymeldError::Session("No unclaimed participant slot".into()))?;
        let enclave = client
            .health()
            .get_enclave_key(slot.enclave_id.as_u32())
            .await?;
        if session
            .recipient_authorization
            .user_enclave_assignments
            .get(&user_id)
            != Some(&slot.enclave_id)
            || session
                .recipient_authorization
                .recipient_public_keys
                .get(&slot.enclave_id)
                != Some(
                    &hex::decode(&enclave.public_key)
                        .map_err(|e| KeymeldError::Session(e.to_string()))?,
                )
        {
            return Err(KeymeldError::Session(
                "Gateway changed the authorized enclave assignment".into(),
            ));
        }
        Ok(RegistrationAssignment {
            session_id: session.session_id.to_string(),
            user_id: Uuid::parse_str(&user_id.to_string())
                .map_err(|e| KeymeldError::Session(e.to_string()))?,
            manifest_hash: session
                .authorization_manifest
                .digest()
                .map_err(SdkError::from)?,
            enclave_id: slot.enclave_id.as_u32(),
            enclave_key_epoch: enclave.key_epoch,
            enclave_public_key: enclave.public_key,
            gateway_url: self
                .settings
                .public_gateway_url
                .clone()
                .unwrap_or_else(|| self.settings.gateway_url.clone()),
            trusted_pcrs: self.settings.trusted_pcrs.clone(),
            dangerous_trust_unattested_enclaves: self.settings.dangerous_trust_unattested_enclaves,
        })
    }
}

/// Create a Keymeld service based on settings
pub fn create_keymeld_service(
    settings: KeymeldSettings,
    coordinator_user_id: Uuid,
    coordinator_private_key: &[u8; 32],
) -> Result<Arc<dyn Keymeld>, KeymeldError> {
    Ok(Arc::new(KeymeldService::new(
        settings,
        coordinator_user_id,
        coordinator_private_key,
    )?))
}

#[cfg(test)]
mod payout_subset_tests {
    use super::*;

    #[test]
    fn signing_subsets_follow_reordered_winners_and_keep_the_authorized_ids() {
        let coordinator = UserId::new_v7();
        let players = vec![UserId::new_v7(), UserId::new_v7(), UserId::new_v7()];
        let definitions: Vec<_> = [vec![0, 1], vec![0, 2], vec![1, 2], vec![0, 1, 2]]
            .into_iter()
            .map(|winners| keymeld_core::protocol::SubsetDefinition {
                subset_id: Uuid::now_v7(),
                participants: std::iter::once(coordinator.clone())
                    .chain(winners.into_iter().map(|index| players[index].clone()))
                    .collect(),
            })
            .collect();
        let payouts = BTreeMap::from([
            (Outcome::Attestation(0), BTreeMap::from([(1, 60), (2, 40)])),
            (Outcome::Attestation(1), BTreeMap::from([(0, 40), (2, 60)])),
            (
                Outcome::Attestation(2),
                BTreeMap::from([(0, 34), (1, 33), (2, 33)]),
            ),
            (Outcome::Expiry, BTreeMap::from([(0, 1), (1, 1), (2, 1)])),
        ]);
        let mapped =
            outcome_subsets_for_payouts(&payouts, &players, &coordinator, &definitions).unwrap();
        assert_eq!(
            mapped,
            BTreeMap::from([
                (0, definitions[2].subset_id),
                (1, definitions[1].subset_id),
                (2, definitions[3].subset_id),
            ])
        );
        assert!(
            outcome_subsets_for_payouts(&payouts, &players, &coordinator, &definitions[..2])
                .is_err()
        );
        let invalid = BTreeMap::from([(Outcome::Attestation(0), BTreeMap::from([(3, 100)]))]);
        assert!(
            outcome_subsets_for_payouts(&invalid, &players, &coordinator, &definitions).is_err()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keymeld_sdk::types::SessionAuthorizationManifest;

    fn session() -> DlcKeygenSession {
        let coordinator = UserId::new_v7();
        let player = UserId::new_v7();
        let session_id = SessionId::new_v7();
        let creator = AuthorizationCredentials::from_secret(&[1; 32]).unwrap();
        let signing = AuthorizationCredentials::from_secret(&[2; 32]).unwrap();
        let credentials = SessionCredentials::from_session_secret(&[3; 32]).unwrap();
        let registrations = BTreeMap::from([
            (
                coordinator.clone(),
                AuthorizationCredentials::from_secret(&[4; 32]).unwrap(),
            ),
            (
                player.clone(),
                AuthorizationCredentials::from_secret(&[5; 32]).unwrap(),
            ),
        ]);
        let manifest = SignedSessionManifest::sign(
            SessionAuthorizationManifest {
                keygen_session_id: session_id.clone(),
                coordinator_user_id: coordinator.clone(),
                creator_pubkey: creator.public_key_bytes(),
                signing_pubkey: signing.public_key_bytes(),
                session_public_key: credentials.public_key_bytes(),
                participant_verifiers: registrations
                    .iter()
                    .map(|(user, authority)| (user.clone(), authority.public_key_bytes()))
                    .collect(),
                timeout_secs: 300,
                max_signing_sessions: None,
                encrypted_taproot_tweak: "unused-in-persistence-test".into(),
                subset_definitions: vec![],
            },
            &creator.export_secret(),
        )
        .unwrap();
        let enclave = EnclaveId::new(1);
        let recipients = EnclaveRecipientAuthorization::sign(
            &manifest,
            BTreeMap::from([(coordinator, enclave), (player, enclave)]),
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
            session_secret: [3; 32],
            authorization_manifest: manifest,
            recipient_authorization: recipients,
            signing_authority: signing,
            registration_authorities: registrations,
            aggregate_key: vec![],
            outcome_subset_ids: BTreeMap::new(),
        }
    }

    #[test]
    fn persisted_session_encrypts_and_restores_independent_authorities() {
        let session = session();
        let keys = Keys::generate();
        let stored = StoredDlcKeygenSession::from_session(&session, &keys).unwrap();
        let encoded = serde_json::to_string(&stored).unwrap();
        for secret in [
            session.session_secret,
            session.signing_authority.export_secret(),
            [4; 32],
            [5; 32],
        ] {
            assert!(!encoded.contains(&hex::encode(secret)));
        }
        let stored: StoredDlcKeygenSession = serde_json::from_str(&encoded).unwrap();
        let restored = stored.to_session(&keys).unwrap();
        assert_eq!(restored.session_secret, session.session_secret);
        assert_eq!(
            restored.signing_authority.export_secret(),
            session.signing_authority.export_secret()
        );
        for (user, authority) in session.registration_authorities {
            assert_eq!(
                restored.registration_authorities[&user].export_secret(),
                authority.export_secret()
            );
        }
        assert_eq!(
            restored.authorization_manifest.digest().unwrap(),
            session.authorization_manifest.digest().unwrap()
        );
        assert_eq!(
            restored.recipient_authorization,
            session.recipient_authorization
        );
        assert!(stored.to_session(&Keys::generate()).is_err());
    }

    #[test]
    fn legacy_or_substituted_authority_records_cannot_restore() {
        let session = session();
        let keys = Keys::generate();
        let mut stored = StoredDlcKeygenSession::from_session(&session, &keys).unwrap();
        let mut legacy = serde_json::to_value(&stored).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("authorization_manifest");
        assert!(serde_json::from_value::<StoredDlcKeygenSession>(legacy).is_err());
        stored.encrypted_signing_authority = stored.encrypted_session_secret.clone();
        assert!(stored.to_session(&keys).is_err());
    }

    #[test]
    fn registration_context_binds_ticket_key_manifest_assignment_and_delegation() {
        let session = session();
        let user = session
            .registration_authorities
            .keys()
            .next()
            .unwrap()
            .clone();
        let key = UserCredentials::from_private_key(&[7; 32]).unwrap();
        let context = RegistrationContext {
            keygen_session_id: session.session_id.clone(),
            user_id: user.clone(),
            manifest_hash: session.authorization_manifest.digest().unwrap(),
            enclave_id: EnclaveId::new(1),
            enclave_key_epoch: 1,
            public_key: key.public_key_bytes(),
            auth_pubkey: key
                .derive_session_auth_pubkey(&session.session_id.to_string())
                .unwrap(),
            require_signing_approval: false,
        };
        let data = ParticipantRegistrationData {
            encrypted_private_key: "unused-in-context-test".into(),
            public_key: hex::encode(&context.public_key),
            auth_pubkey: hex::encode(&context.auth_pubkey),
            context,
        };
        session.validate_registration(&user, &data).unwrap();
        assert!(session
            .validate_registration(&UserId::new_v7(), &data)
            .is_err());
        let mut changed = data.clone();
        changed.context.public_key = UserCredentials::from_private_key(&[8; 32])
            .unwrap()
            .public_key_bytes();
        assert!(session.validate_registration(&user, &changed).is_err());
        changed = data.clone();
        changed.context.manifest_hash[0] ^= 1;
        assert!(session.validate_registration(&user, &changed).is_err());
        changed = data.clone();
        changed.context.enclave_id = EnclaveId::new(2);
        assert!(session.validate_registration(&user, &changed).is_err());
        changed = data;
        changed.context.require_signing_approval = true;
        assert!(session.validate_registration(&user, &changed).is_err());
    }

    #[test]
    fn enabled_service_requires_trusted_measurements_or_explicit_simulation() {
        let settings = KeymeldSettings {
            enabled: true,
            ..KeymeldSettings::default()
        };
        assert!(KeymeldService::new(settings, Uuid::now_v7(), &[1; 32]).is_err());
        let disabled =
            KeymeldService::new(KeymeldSettings::default(), Uuid::now_v7(), &[1; 32]).unwrap();
        assert!(!disabled.is_enabled());
        let simulation = KeymeldService::new(
            KeymeldSettings {
                enabled: true,
                dangerous_trust_unattested_enclaves: true,
                ..KeymeldSettings::default()
            },
            Uuid::now_v7(),
            &[1; 32],
        )
        .unwrap();
        assert!(simulation.is_enabled());
        assert!(KeymeldService::new(
            KeymeldSettings {
                enabled: true,
                dangerous_trust_unattested_enclaves: true,
                trusted_pcrs: BTreeMap::from([(0, "ab".repeat(48))]),
                ..KeymeldSettings::default()
            },
            Uuid::now_v7(),
            &[1; 32],
        )
        .is_err());
    }
    struct GatewayFixture {
        session_id: SessionId,
        user_id: UserId,
        manifest: SignedSessionManifest,
        enclave_public_key: String,
        proofs: std::sync::Mutex<Vec<String>>,
    }

    async fn slots(
        axum::extract::State(state): axum::extract::State<Arc<GatewayFixture>>,
        headers: axum::http::HeaderMap,
    ) -> axum::Json<keymeld_sdk::GetAvailableSlotsResponse> {
        verify_request(&state, &headers);
        axum::Json(keymeld_sdk::GetAvailableSlotsResponse {
            session_id: state.session_id.clone(),
            total_slots: 1,
            claimed_slots: 0,
            available_slots: vec![keymeld_sdk::AvailableUserSlot {
                user_id: state.user_id.clone(),
                enclave_id: EnclaveId::new(1),
                signer_index: 1,
                claimed: false,
            }],
        })
    }

    async fn enclave_key(
        axum::extract::State(state): axum::extract::State<Arc<GatewayFixture>>,
    ) -> axum::Json<keymeld_sdk::EnclavePublicKeyResponse> {
        axum::Json(keymeld_sdk::EnclavePublicKeyResponse {
            enclave_id: 1,
            public_key: state.enclave_public_key.clone(),
            attestation_document: String::new(),
            pcr_measurements: Default::default(),
            timestamp: 0,
            healthy: true,
            key_epoch: 1,
        })
    }

    fn verify_request(state: &GatewayFixture, headers: &axum::http::HeaderMap) {
        let proof = keymeld_core::request_auth::RequestAuth::parse(
            headers["X-Session-Signature"].to_str().unwrap(),
        )
        .unwrap();
        proof
            .verify(
                keymeld_core::request_auth::AuthKind::Session,
                &state.session_id.to_string(),
                "",
                &keymeld_core::PublicKey::from_slice(&state.manifest.manifest.session_public_key)
                    .unwrap(),
                keymeld_core::request_auth::now_timestamp_secs().unwrap(),
            )
            .unwrap();
    }

    async fn register(
        axum::extract::State(state): axum::extract::State<Arc<GatewayFixture>>,
        headers: axum::http::HeaderMap,
        axum::Json(request): axum::Json<RegisterKeygenParticipantRequest>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        verify_request(&state, &headers);
        request
            .registration_authorization
            .verify(&state.manifest, &request.encrypted_private_key)
            .unwrap();
        assert_eq!(request.user_id, state.user_id);
        assert!(!request.require_signing_approval);
        let attempt = {
            let mut proofs = state.proofs.lock().unwrap();
            proofs.push(headers["X-Session-Signature"].to_str().unwrap().to_owned());
            proofs.len()
        };
        if attempt == 1 {
            return (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", "0")],
            )
                .into_response();
        }
        axum::Json(keymeld_sdk::RegisterKeygenParticipantResponse {
            keygen_session_id: state.session_id.clone(),
            user_id: state.user_id.clone(),
            status: KeygenStatusKind::CollectingParticipants,
            participants_registered: 2,
            expected_participants: 2,
            signer_index: 1,
            assigned_enclave_id: EnclaveId::new(1),
            require_signing_approval: false,
        })
        .into_response()
    }

    #[tokio::test]
    async fn delegated_registration_uses_slot_authority_and_refreshes_auth_after_rate_limit() {
        let session = session();
        let user = session
            .registration_authorities
            .keys()
            .next()
            .unwrap()
            .clone();
        let enclave_public_key =
            hex::encode(&session.recipient_authorization.recipient_public_keys[&EnclaveId::new(1)]);
        let fixture = Arc::new(GatewayFixture {
            session_id: session.session_id.clone(),
            user_id: user.clone(),
            manifest: session.authorization_manifest.clone(),
            enclave_public_key: enclave_public_key.clone(),
            proofs: Default::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = axum::Router::new()
            .route("/api/v1/keygen/{session}/slots", axum::routing::get(slots))
            .route(
                "/api/v1/enclaves/{enclave}/public-key",
                axum::routing::get(enclave_key),
            )
            .route(
                "/api/v1/keygen/{session}/participants",
                axum::routing::post(register),
            )
            .layer(tower_http::decompression::RequestDecompressionLayer::new().gzip(true))
            .with_state(fixture.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        // Only this synthetic transport test bypasses hardware evidence. Production construction requires PCR pins.
        let client = KeyMeldClient::builder(
            &url,
            session
                .authorization_manifest
                .manifest
                .coordinator_user_id
                .clone(),
        )
        .dangerous_trust_unattested_enclaves()
        .build()
        .unwrap();
        let service = KeymeldService {
            settings: KeymeldSettings {
                gateway_url: url,
                enabled: true,
                ..KeymeldSettings::default()
            },
            client: Some(client),
            coordinator_user_id: session
                .authorization_manifest
                .manifest
                .coordinator_user_id
                .clone(),
        };
        let key = UserCredentials::from_private_key(&[7; 32]).unwrap();
        let context = RegistrationContext {
            keygen_session_id: session.session_id.clone(),
            user_id: user.clone(),
            manifest_hash: session.authorization_manifest.digest().unwrap(),
            enclave_id: EnclaveId::new(1),
            enclave_key_epoch: 1,
            public_key: key.public_key_bytes(),
            auth_pubkey: key
                .derive_session_auth_pubkey(&session.session_id.to_string())
                .unwrap(),
            require_signing_approval: false,
        };
        let data = ParticipantRegistrationData {
            encrypted_private_key: key
                .prepare_registration(context.clone(), &enclave_public_key)
                .unwrap(),
            public_key: hex::encode(&context.public_key),
            auth_pubkey: hex::encode(&context.auth_pubkey),
            context,
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            service.register_participant(&session, user, &data),
        )
        .await;
        server.abort();
        let _ = server.await;
        result.unwrap().unwrap();
        let proofs = fixture.proofs.lock().unwrap();
        assert_eq!(proofs.len(), 2);
        assert_ne!(proofs[0], proofs[1]);
    }
}
