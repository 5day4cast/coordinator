use crate::config::KeymeldSettings;
use async_trait::async_trait;
use keymeld_sdk::{
    dlctix::{
        dlctix::{ContractParameters, OutcomeIndex, SigningData},
        DlcBatchBuilder, DlcSignatureResults, DlcSubsetBuilder,
    },
    prelude::*,
    PollingConfig,
};
use log::{debug, info};
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
    pub registered_participants: usize,
    pub expected_participants: usize,
    pub is_completed: bool,
}

/// Trait for Keymeld signing operations
#[async_trait]
pub trait Keymeld: Send + Sync {
    /// Initialize a keygen session without waiting for participants
    /// This creates the session, registers the coordinator, and returns immediately
    /// Users will register themselves via the WASM SDK after payment
    async fn init_keygen_session(
        &self,
        competition_id: Uuid,
        contract_params: &ContractParameters,
        player_user_ids: Vec<UserId>,
    ) -> Result<DlcKeygenSession, KeymeldError>;

    /// Wait for keygen to complete and get the aggregate key
    /// Called after all participants have registered
    async fn wait_for_keygen_completion(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<Vec<u8>, KeymeldError>;

    /// Get the status of a keygen session (for polling registrations)
    async fn get_keygen_status(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<KeygenSessionStatus, KeymeldError>;

    /// Sign a DLC contract batch
    async fn sign_dlc_batch(
        &self,
        keygen_session: &DlcKeygenSession,
        signing_data: &SigningData,
    ) -> Result<DlcSignatureResults, KeymeldError>;

    /// Check if Keymeld is enabled
    fn is_enabled(&self) -> bool;
}

/// Holds the state of a DLC keygen session
#[derive(Debug, Clone)]
pub struct DlcKeygenSession {
    pub session_id: SessionId,
    pub session_secret: [u8; 32],
    pub aggregate_key: Vec<u8>,
    /// Mapping from outcome index to subset ID (for signing)
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
    /// Aggregate public key bytes (hex encoded)
    #[serde(with = "hex_vec")]
    pub aggregate_key: Vec<u8>,
    /// Mapping from outcome index to subset ID
    pub outcome_subset_ids: BTreeMap<usize, Uuid>,
}

impl StoredDlcKeygenSession {
    /// Create a stored session by encrypting the session secret
    pub fn from_session(session: DlcKeygenSession, encrypted_session_secret: String) -> Self {
        Self {
            session_id: session.session_id.to_string(),
            encrypted_session_secret,
            aggregate_key: session.aggregate_key,
            outcome_subset_ids: session.outcome_subset_ids,
        }
    }

    /// Convert to DlcKeygenSession with the decrypted session secret
    pub fn to_session(&self, session_secret: [u8; 32]) -> DlcKeygenSession {
        DlcKeygenSession {
            session_id: SessionId::new(&self.session_id),
            session_secret,
            aggregate_key: self.aggregate_key.clone(),
            outcome_subset_ids: self.outcome_subset_ids.clone(),
        }
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

        let client = KeyMeldClient::builder(&settings.gateway_url, user_id.clone())
            .credentials(credentials)
            .polling_config(polling_config)
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

    async fn init_keygen_session(
        &self,
        competition_id: Uuid,
        contract_params: &ContractParameters,
        player_user_ids: Vec<UserId>,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        let client = self.get_client()?;

        info!(
            "Initializing keygen session for competition {} with {} players",
            competition_id,
            player_user_ids.len()
        );

        // Build subset definitions for DLC outcomes
        let subset_info = DlcSubsetBuilder::new(
            contract_params,
            self.coordinator_user_id.clone(),
            player_user_ids.clone(),
        )
        .build()
        .map_err(|e| KeymeldError::Session(format!("Failed to build subsets: {}", e)))?;

        // All participants: coordinator + all players
        let mut all_participants = vec![self.coordinator_user_id.clone()];
        all_participants.extend(player_user_ids);

        let keygen_options = KeygenOptions::default()
            .timeout(self.settings.keygen_timeout_secs)
            .tweak(TaprootTweak::None);

        // Create the keygen session with subset definitions
        let mut keygen_session = client
            .keygen()
            .create_session_with_subsets(all_participants, subset_info.definitions, keygen_options)
            .await?;

        // Register ourselves (coordinator) as a participant
        keygen_session
            .register_self(RegisterOptions::default())
            .await?;

        info!(
            "Keygen session {} initialized, waiting for user registrations",
            keygen_session.session_id()
        );

        // Return immediately without waiting for other participants
        // The aggregate_key will be empty until completion - it's set when wait_for_keygen_completion is called
        Ok(DlcKeygenSession {
            session_id: keygen_session.session_id().clone(),
            session_secret: keygen_session.export_session_secret(),
            aggregate_key: vec![], // Will be populated when keygen completes
            outcome_subset_ids: subset_info.outcome_subset_ids,
        })
    }

    async fn wait_for_keygen_completion(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<Vec<u8>, KeymeldError> {
        let client = self.get_client()?;

        info!(
            "Waiting for keygen session {} to complete",
            session.session_id
        );

        // Restore session from credentials
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;

        let mut restored_session = client
            .keygen()
            .restore_session(session.session_id.clone(), credentials)
            .await?;

        // Wait for all participants to register and keygen to complete
        let _aggregate_key = restored_session.wait_for_completion().await?;

        // Decrypt the aggregate public key
        let aggregate_key = restored_session.decrypt_aggregate_key()?;

        info!(
            "Keygen session {} completed with aggregate key",
            session.session_id
        );

        Ok(aggregate_key)
    }

    async fn sign_dlc_batch(
        &self,
        keygen_session: &DlcKeygenSession,
        signing_data: &SigningData,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        let client = self.get_client()?;

        info!(
            "Creating DLC signing session for keygen {}",
            keygen_session.session_id
        );

        // Restore the keygen session from stored credentials
        let credentials = SessionCredentials::from_session_secret(&keygen_session.session_secret)?;

        let restored_keygen = client
            .keygen()
            .restore_session(keygen_session.session_id.clone(), credentials)
            .await?;

        // Build batch items for all DLC transactions
        let dlc_batch = DlcBatchBuilder::new(signing_data)
            .with_outcome_subsets(&keygen_session.outcome_subset_ids)
            .build()
            .map_err(|e| KeymeldError::Signing(format!("Failed to build batch: {}", e)))?;

        let signing_options = SigningOptions::default().timeout(self.settings.signing_timeout_secs);

        // Create signing session - need to clone items since sign_batch consumes them
        let batch_items = dlc_batch.items.clone();
        let mut signing_session = client
            .signer()
            .sign_batch(&restored_keygen, batch_items, signing_options)
            .await?;

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
        let credentials = SessionCredentials::from_session_secret(&session.session_secret)?;

        let restored_session = client
            .keygen()
            .restore_session(session.session_id.clone(), credentials)
            .await?;

        let status_kind = restored_session.status();
        let is_completed = matches!(
            status_kind,
            keymeld_sdk::prelude::KeygenStatusKind::Completed
        );

        // Get available slots to count registrations
        let slots = client
            .keygen()
            .get_available_slots(&session.session_id)
            .await?;
        let registered = slots.available_slots.iter().filter(|s| s.claimed).count();
        let expected = slots.available_slots.len();

        Ok(KeygenSessionStatus {
            session_id: session.session_id.to_string(),
            status: status_kind.as_ref().to_string(),
            registered_participants: registered,
            expected_participants: expected,
            is_completed,
        })
    }
}

/// Mock implementation for testing without Keymeld
pub struct MockKeymeld;

#[async_trait]
impl Keymeld for MockKeymeld {
    fn is_enabled(&self) -> bool {
        false
    }

    async fn init_keygen_session(
        &self,
        _competition_id: Uuid,
        _contract_params: &ContractParameters,
        _player_user_ids: Vec<UserId>,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        Err(KeymeldError::NotEnabled)
    }

    async fn wait_for_keygen_completion(
        &self,
        _session: &DlcKeygenSession,
    ) -> Result<Vec<u8>, KeymeldError> {
        Err(KeymeldError::NotEnabled)
    }

    async fn get_keygen_status(
        &self,
        _session: &DlcKeygenSession,
    ) -> Result<KeygenSessionStatus, KeymeldError> {
        Err(KeymeldError::NotEnabled)
    }

    async fn sign_dlc_batch(
        &self,
        _keygen_session: &DlcKeygenSession,
        _signing_data: &SigningData,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        Err(KeymeldError::NotEnabled)
    }
}

/// Create a Keymeld service based on settings
pub fn create_keymeld_service(
    settings: KeymeldSettings,
    coordinator_user_id: Uuid,
    coordinator_private_key: &[u8; 32],
) -> Result<Arc<dyn Keymeld>, KeymeldError> {
    if settings.enabled {
        Ok(Arc::new(KeymeldService::new(
            settings,
            coordinator_user_id,
            coordinator_private_key,
        )?))
    } else {
        Ok(Arc::new(MockKeymeld))
    }
}
