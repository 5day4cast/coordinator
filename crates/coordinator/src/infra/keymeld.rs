use crate::config::KeymeldSettings;
use crate::infra::db::DBConnection;
use async_trait::async_trait;
use coordinator_core::RegistrationAssignment;
use coordinator_escrow::authorization::PayoutPolicy;
use dlctix::{Outcome, OutcomeIndex, PayoutWeights};
use keymeld_core::authorization::EnclaveRecipientAuthorization;
use keymeld_core::crypto::SessionSecret;
use keymeld_core::escrow::SignedEscrowPolicy;
#[cfg(test)]
mod confidential_integration_tests;
mod confidential_service;
mod confidential_store;
pub use coordinator_escrow::{
    payout::ContractCommitment,
    payout_capabilities::PayoutCapabilities,
    payout_protocol::{
        PayoutContractBoundResponse, PayoutPreparedResponse, PayoutSecrets, PreparePayoutRequest,
        ReleasePayoutRequest,
    },
};
pub use keymeld_sdk::types::SubsetDefinition;
use keymeld_sdk::{
    dlctix::{
        dlctix::{ContractParameters, SigningData},
        DlcBatchBuilder, DlcSignatureResults,
    },
    prelude::*,
    types::{RegistrationAuthorization, RegistrationContext, SignedRoster, SignedSessionManifest},
    PollingConfig,
};

use nostr::{nips::nip44, Keys};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;
use zeroize::Zeroize;

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

    #[error("Keymeld protocol error: {0}")]
    Protocol(#[from] keymeld_core::KeyMeldError),

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
    /// Accepted policy from durable entry storage, checked against the sealed envelope.
    pub payout_policy: Option<PayoutPolicy>,
    pub escrow_policy: Option<SignedEscrowPolicy>,
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
    async fn payout_capabilities(&self) -> Result<PayoutCapabilities, KeymeldError>;

    async fn bind_payout_contract(
        &self,
        session: &DlcKeygenSession,
        contract: &ContractCommitment,
        expected_policies: &BTreeMap<UserId, PayoutPolicy>,
    ) -> Result<Vec<PayoutContractBoundResponse>, KeymeldError>;

    async fn prepare_payout(
        &self,
        session: &DlcKeygenSession,
        user_id: UserId,
        request: PreparePayoutRequest,
    ) -> Result<PayoutPreparedResponse, KeymeldError>;

    async fn release_payout(
        &self,
        session: &DlcKeygenSession,
        user_id: UserId,
        request: ReleasePayoutRequest,
    ) -> Result<PayoutSecrets, KeymeldError>;

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

    /// Sign an Arkade-funded pool's contract, once the batch's commitment transaction fixes its
    /// funding outpoint. The contract must have been bound with a null funding outpoint.
    async fn sign_ark_dlc_batch(
        &self,
        keygen_session: &DlcKeygenSession,
        signing_data: &SigningData,
        contract_params: &ContractParameters,
        player_user_ids: Vec<UserId>,
        ark_funding: coordinator_escrow::ark::ArkFunding,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        let _ = (
            keygen_session,
            signing_data,
            contract_params,
            player_user_ids,
            ark_funding,
        );
        Err(KeymeldError::Signing(
            "Arkade-funded pools are not supported".into(),
        ))
    }

    /// Sign a player's Arkade escrow spend with their deposited entry key.
    ///
    /// The Coordinator verifier checks the spend's transactions and derives every digest.
    /// Each call is a fresh attempt. Returns each signed input's index with its BIP340 signature.
    async fn sign_ark_escrow(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        spend: coordinator_escrow::ark::ArkEscrowSpend,
    ) -> Result<Vec<(usize, [u8; 64])>, KeymeldError> {
        let _ = (session, user, spend);
        Err(KeymeldError::Signing(
            "Arkade escrows are not supported".into(),
        ))
    }

    /// Sign several players' escrow spends in one Keymeld session round trip.
    /// Returns each spend's signed inputs, in order.
    async fn sign_ark_escrows(
        &self,
        session: &DlcKeygenSession,
        spends: Vec<(UserId, coordinator_escrow::ark::ArkEscrowSpend)>,
    ) -> Result<Vec<Vec<(usize, [u8; 64])>>, KeymeldError> {
        let mut signed = Vec::with_capacity(spends.len());
        for (user, spend) in spends {
            signed.push(self.sign_ark_escrow(session, user, spend).await?);
        }
        Ok(signed)
    }

    /// Sign a player's refund of an escrow whose competition never kicked off.
    ///
    /// Unlike a pool's escrow spends this needs no binding: the pool may never have formed. The
    /// Coordinator verifier authorizes it from the player's own registered policy, checking that
    /// `invoice` belongs to their Lightning Address and that the refund pays a swap committed to
    /// it. `fee_sats` is what the swap service keeps, capped by that policy.
    ///
    /// A refund takes two transactions, signed in turn; see `RefundPurpose`.
    async fn sign_ark_refund(
        &self,
        session: &DlcKeygenSession,
        user: UserId,
        spend: coordinator_escrow::ark::ArkEscrowSpend,
        invoice: String,
        fee_sats: u64,
    ) -> Result<[u8; 64], KeymeldError> {
        let _ = (session, user, spend, invoice, fee_sats);
        Err(KeymeldError::Signing(
            "Arkade refunds are not supported".into(),
        ))
    }

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

impl Drop for DlcKeygenSession {
    fn drop(&mut self) {
        self.session_secret.zeroize();
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

fn verify_prepared_payout_origin(
    session: &DlcKeygenSession,
    user_id: &UserId,
    response: &PayoutPreparedResponse,
) -> Result<(), KeymeldError> {
    let enclave_id = session
        .recipient_authorization
        .user_enclave_assignments
        .get(user_id)
        .ok_or_else(|| {
            KeymeldError::Session("Payout participant has no authorized enclave".into())
        })?;
    if response.enclave_id != *enclave_id
        || response.keygen_session_id != session.session_id
        || &response.user_id != user_id
    {
        return Err(KeymeldError::Session(
            "Payout response names a different assigned enclave or participant".into(),
        ));
    }
    let public_key = session
        .recipient_authorization
        .recipient_public_keys
        .get(enclave_id)
        .ok_or_else(|| KeymeldError::Session("Payout enclave has no pinned public key".into()))?;
    response.verify(public_key).map_err(SdkError::from)?;
    Ok(())
}

/// Production Keymeld service implementation
pub struct KeymeldService {
    settings: KeymeldSettings,
    client: Option<KeyMeldClient>,
    coordinator_user_id: UserId,
    db: Option<DBConnection>,
    store_key: SessionSecret,
    user_credentials: UserCredentials,
    session_locks: tokio::sync::Mutex<BTreeMap<SessionId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
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
                db: None,
                store_key: SessionSecret::from_bytes(*coordinator_private_key),
                user_credentials: UserCredentials::from_private_key(coordinator_private_key)?,
                session_locks: Default::default(),
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
            db: None,
            store_key: SessionSecret::from_bytes(*coordinator_private_key),
            user_credentials: UserCredentials::from_private_key(coordinator_private_key)?,
            session_locks: Default::default(),
        })
    }

    fn get_client(&self) -> Result<&KeyMeldClient, KeymeldError> {
        self.client.as_ref().ok_or(KeymeldError::NotEnabled)
    }
}

/// Create a Keymeld service based on settings
pub fn create_keymeld_service(
    settings: KeymeldSettings,
    coordinator_user_id: Uuid,
    coordinator_private_key: &[u8; 32],
    database: DBConnection,
) -> Result<Arc<dyn Keymeld>, KeymeldError> {
    let mut service = KeymeldService::new(settings, coordinator_user_id, coordinator_private_key)?;
    service.db = Some(database);
    Ok(Arc::new(service))
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
        for (user, authority) in &session.registration_authorities {
            assert_eq!(
                restored.registration_authorities[user].export_secret(),
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
            payout_policy: None,
            escrow_policy: None,
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
}
