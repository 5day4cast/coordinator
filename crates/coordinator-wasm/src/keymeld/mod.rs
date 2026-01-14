//! Keymeld client integration for WASM
//!
//! This module provides browser-side Keymeld SDK integration for:
//! - Joining keygen sessions as a participant
//! - Participating in DLC signing sessions
//! - Managing session state in browser storage
//! - Server-side registration crypto (auth pubkey derivation, enclave encryption)

use keymeld_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;
use wasm_bindgen::prelude::*;

/// Derive the keymeld auth pubkey from a private key and session ID.
///
/// This is used for server-side keymeld registration where the coordinator
/// registers participants on their behalf. The user derives their auth_pubkey
/// client-side and sends it to the coordinator along with their encrypted private key.
///
/// @param private_key_hex - The user's ephemeral private key (hex encoded, 32 bytes)
/// @param session_id - The keymeld keygen session ID
/// @returns The derived auth public key (hex encoded)
#[wasm_bindgen]
pub fn derive_keymeld_auth_pubkey(
    private_key_hex: &str,
    session_id: &str,
) -> Result<String, JsValue> {
    let private_key_bytes: [u8; 32] = hex::decode(private_key_hex)
        .map_err(|e| JsValue::from_str(&format!("Invalid private key hex: {}", e)))?
        .try_into()
        .map_err(|_| JsValue::from_str("Private key must be 32 bytes"))?;

    let credentials = UserCredentials::from_private_key(&private_key_bytes)
        .map_err(|e| JsValue::from_str(&format!("Failed to create credentials: {}", e)))?;

    let auth_pubkey = credentials
        .derive_session_auth_pubkey(session_id)
        .map_err(|e| JsValue::from_str(&format!("Failed to derive auth pubkey: {}", e)))?;

    Ok(hex::encode(auth_pubkey))
}

/// Encrypt a private key for submission to a keymeld enclave.
///
/// This is used for server-side keymeld registration. The user encrypts their
/// ephemeral private key to the enclave's public key, allowing the coordinator
/// to forward it to keymeld without being able to read the key.
///
/// @param private_key_hex - The user's ephemeral private key (hex encoded, 32 bytes)
/// @param enclave_pubkey_hex - The keymeld enclave's public key (hex encoded)
/// @returns The encrypted private key (hex encoded)
#[wasm_bindgen]
pub fn encrypt_private_key_for_enclave(
    private_key_hex: &str,
    enclave_pubkey_hex: &str,
) -> Result<String, JsValue> {
    let private_key_bytes: [u8; 32] = hex::decode(private_key_hex)
        .map_err(|e| JsValue::from_str(&format!("Invalid private key hex: {}", e)))?
        .try_into()
        .map_err(|_| JsValue::from_str("Private key must be 32 bytes"))?;

    let credentials = UserCredentials::from_private_key(&private_key_bytes)
        .map_err(|e| JsValue::from_str(&format!("Failed to create credentials: {}", e)))?;

    let encrypted = credentials
        .encrypt_private_key_for_enclave(enclave_pubkey_hex)
        .map_err(|e| JsValue::from_str(&format!("Failed to encrypt for enclave: {}", e)))?;

    Ok(encrypted)
}

/// Errors that can occur during Keymeld operations
#[derive(Debug, Error)]
pub enum KeymeldClientError {
    #[error("SDK error: {0}")]
    Sdk(#[from] SdkError),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("JavaScript error: {0}")]
    JsError(String),
}

impl From<KeymeldClientError> for JsValue {
    fn from(err: KeymeldClientError) -> Self {
        JsValue::from_str(&err.to_string())
    }
}

/// Configuration for connecting to a Keymeld gateway
#[derive(Debug, Clone, Serialize, Deserialize)]
#[wasm_bindgen]
pub struct KeymeldClientConfig {
    /// The Keymeld gateway URL
    gateway_url: String,
    /// Maximum polling attempts for session completion
    max_polling_attempts: u32,
    /// Initial polling delay in milliseconds
    initial_polling_delay_ms: u64,
    /// Maximum polling delay in milliseconds
    max_polling_delay_ms: u64,
    /// Backoff multiplier for polling
    polling_backoff_multiplier: f64,
}

#[wasm_bindgen]
impl KeymeldClientConfig {
    #[wasm_bindgen(constructor)]
    pub fn new(gateway_url: String) -> Self {
        Self {
            gateway_url,
            max_polling_attempts: 60,
            initial_polling_delay_ms: 500,
            max_polling_delay_ms: 5000,
            polling_backoff_multiplier: 1.5,
        }
    }

    #[wasm_bindgen(getter)]
    pub fn gateway_url(&self) -> String {
        self.gateway_url.clone()
    }

    #[wasm_bindgen(setter)]
    pub fn set_gateway_url(&mut self, url: String) {
        self.gateway_url = url;
    }

    /// Set polling configuration
    #[wasm_bindgen]
    pub fn with_polling(
        mut self,
        max_attempts: u32,
        initial_delay_ms: u64,
        max_delay_ms: u64,
    ) -> Self {
        self.max_polling_attempts = max_attempts;
        self.initial_polling_delay_ms = initial_delay_ms;
        self.max_polling_delay_ms = max_delay_ms;
        self
    }
}

/// Client-side Keymeld session participant
///
/// This is used by players to join keygen and signing sessions
/// created by the coordinator.
#[wasm_bindgen]
pub struct KeymeldParticipant {
    #[allow(dead_code)]
    config: KeymeldClientConfig,
    client: Option<KeyMeldClient>,
    user_id: UserId,
}

#[wasm_bindgen]
impl KeymeldParticipant {
    /// Create a new Keymeld participant from a private key
    ///
    /// The private key is used to derive the user's identity and credentials.
    /// This should be the user's ephemeral private key for the DLC.
    #[wasm_bindgen(constructor)]
    pub fn new(
        config: KeymeldClientConfig,
        user_id_hex: &str,
        private_key_hex: &str,
    ) -> Result<KeymeldParticipant, JsValue> {
        let user_id = Uuid::parse_str(user_id_hex)
            .map_err(|e| KeymeldClientError::Config(format!("Invalid user ID: {}", e)))?;

        let private_key_bytes: [u8; 32] = hex::decode(private_key_hex)
            .map_err(|e| KeymeldClientError::Config(format!("Invalid private key hex: {}", e)))?
            .try_into()
            .map_err(|_| KeymeldClientError::Config("Private key must be 32 bytes".to_string()))?;

        let user_id = UserId::from(user_id);
        let credentials = UserCredentials::from_private_key(&private_key_bytes).map_err(|e| {
            KeymeldClientError::Config(format!("Failed to create credentials: {}", e))
        })?;

        let polling_config = PollingConfig {
            max_attempts: config.max_polling_attempts,
            initial_delay: Duration::from_millis(config.initial_polling_delay_ms),
            max_delay: Duration::from_millis(config.max_polling_delay_ms),
            backoff_multiplier: config.polling_backoff_multiplier,
            jitter: 0.25,
        };

        let client = KeyMeldClient::builder(&config.gateway_url, user_id.clone())
            .credentials(credentials)
            .polling_config(polling_config)
            .build()
            .map_err(|e| KeymeldClientError::Config(format!("Failed to build client: {}", e)))?;

        Ok(Self {
            config,
            client: Some(client),
            user_id,
        })
    }

    /// Get the user ID as a hex string
    #[wasm_bindgen(getter)]
    pub fn user_id(&self) -> String {
        self.user_id.to_string()
    }

    /// Join a keygen session created by the coordinator
    ///
    /// This registers the participant in the session and waits for keygen to complete.
    /// The session_secret is provided by the coordinator when creating the session.
    /// Returns the keygen result including the aggregate key.
    #[wasm_bindgen]
    pub async fn join_keygen_session(
        &self,
        session_id: &str,
        session_secret_hex: &str,
    ) -> Result<JsValue, JsValue> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| KeymeldClientError::Config("Client not initialized".to_string()))?;

        let session_id = SessionId::new(session_id);

        let session_secret: [u8; 32] = hex::decode(session_secret_hex)
            .map_err(|e| KeymeldClientError::Config(format!("Invalid session secret: {}", e)))?
            .try_into()
            .map_err(|_| {
                KeymeldClientError::Config("Session secret must be 32 bytes".to_string())
            })?;

        // Join the existing session with the provided secret
        let mut keygen_session = client
            .keygen()
            .join_session(session_id, &session_secret, JoinOptions::default())
            .await
            .map_err(KeymeldClientError::from)?;

        // Register ourselves as a participant
        keygen_session
            .register_self(RegisterOptions::default())
            .await
            .map_err(KeymeldClientError::from)?;

        // Wait for keygen to complete
        keygen_session
            .wait_for_completion()
            .await
            .map_err(KeymeldClientError::from)?;

        // Export session secret for later signing
        let exported_secret = keygen_session.export_session_secret();
        let aggregate_key = keygen_session
            .decrypt_aggregate_key()
            .map_err(KeymeldClientError::from)?;

        // Return session info as JSON
        let result = KeygenResult {
            session_id: keygen_session.session_id().to_string(),
            session_secret: hex::encode(exported_secret),
            aggregate_key: hex::encode(aggregate_key),
        };

        serde_wasm_bindgen::to_value(&result)
            .map_err(|e| KeymeldClientError::Serialization(e.to_string()).into())
    }

    /// Participate in a signing session
    ///
    /// The coordinator initiates signing using the keygen session credentials.
    /// Participants with the session secret automatically contribute their
    /// partial signatures when the keymeld enclave processes the batch.
    /// This method restores the session and confirms participation is ready.
    #[wasm_bindgen]
    pub async fn participate_in_signing(
        &self,
        keygen_session_id: &str,
        session_secret_hex: &str,
    ) -> Result<JsValue, JsValue> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| KeymeldClientError::Config("Client not initialized".to_string()))?;

        let session_secret: [u8; 32] = hex::decode(session_secret_hex)
            .map_err(|e| KeymeldClientError::Config(format!("Invalid session secret: {}", e)))?
            .try_into()
            .map_err(|_| {
                KeymeldClientError::Config("Session secret must be 32 bytes".to_string())
            })?;

        let keygen_session_id = SessionId::new(keygen_session_id);
        let credentials = SessionCredentials::from_session_secret(&session_secret)
            .map_err(KeymeldClientError::from)?;

        // Restore the keygen session to verify credentials are valid
        let restored_keygen = client
            .keygen()
            .restore_session(keygen_session_id.clone(), credentials)
            .await
            .map_err(KeymeldClientError::from)?;

        let aggregate_key = restored_keygen
            .decrypt_aggregate_key()
            .map_err(KeymeldClientError::from)?;

        let result = SigningResult {
            success: true,
            message: format!("Ready for signing with session {}", keygen_session_id),
            aggregate_key: Some(hex::encode(aggregate_key)),
        };

        serde_wasm_bindgen::to_value(&result)
            .map_err(|e| KeymeldClientError::Serialization(e.to_string()).into())
    }

    /// Poll for signing session status
    ///
    /// Check if the coordinator has initiated a signing session and get its status.
    #[wasm_bindgen]
    pub async fn get_signing_status(
        &self,
        keygen_session_id: &str,
        session_secret_hex: &str,
    ) -> Result<JsValue, JsValue> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| KeymeldClientError::Config("Client not initialized".to_string()))?;

        let session_secret: [u8; 32] = hex::decode(session_secret_hex)
            .map_err(|e| KeymeldClientError::Config(format!("Invalid session secret: {}", e)))?
            .try_into()
            .map_err(|_| {
                KeymeldClientError::Config("Session secret must be 32 bytes".to_string())
            })?;

        let keygen_session_id = SessionId::new(keygen_session_id);
        let credentials = SessionCredentials::from_session_secret(&session_secret)
            .map_err(KeymeldClientError::from)?;

        let restored_keygen = client
            .keygen()
            .restore_session(keygen_session_id, credentials)
            .await
            .map_err(KeymeldClientError::from)?;

        let keygen_status = restored_keygen.status();
        let status = SessionStatus {
            session_id: restored_keygen.session_id().to_string(),
            status: keygen_status.as_ref().to_string(),
            is_complete: matches!(keygen_status, KeygenStatusKind::Completed),
        };

        serde_wasm_bindgen::to_value(&status)
            .map_err(|e| KeymeldClientError::Serialization(e.to_string()).into())
    }
}

/// Result from joining a keygen session
#[derive(Debug, Serialize, Deserialize)]
pub struct KeygenResult {
    /// The session ID
    pub session_id: String,
    /// The session secret (hex encoded) - save this for signing
    pub session_secret: String,
    /// The aggregate public key (hex encoded)
    pub aggregate_key: String,
}

/// Result from participating in signing
#[derive(Debug, Serialize, Deserialize)]
pub struct SigningResult {
    /// Whether participation was successful
    pub success: bool,
    /// Status message
    pub message: String,
    /// Aggregate public key if available
    pub aggregate_key: Option<String>,
}

/// Session status information
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStatus {
    /// The session ID
    pub session_id: String,
    /// Current status (reserved, collecting_participants, completed, failed)
    pub status: String,
    /// Whether the session has completed
    pub is_complete: bool,
}
