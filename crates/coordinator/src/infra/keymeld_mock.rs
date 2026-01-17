use async_trait::async_trait;
use keymeld_sdk::{
    dlctix::{
        dlctix::{ContractParameters, SigningData},
        DlcSignatureResults,
    },
    prelude::*,
};
use uuid::Uuid;

use super::keymeld::{
    DlcKeygenSession, KeygenSessionStatus, Keymeld, KeymeldError, ParticipantRegistrationData,
};

/// Mock implementation for testing without Keymeld
/// Returns valid mock data to enable E2E testing of the entry flow
pub struct MockKeymeld;

#[async_trait]
impl Keymeld for MockKeymeld {
    fn is_enabled(&self) -> bool {
        // Return true so the entry flow works with mock data
        true
    }

    async fn init_keygen_session(
        &self,
        competition_id: Uuid,
        _player_user_ids: Vec<UserId>,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        // Return a mock session with deterministic values
        let session_id = SessionId::from(competition_id);
        let session_secret = [0u8; 32]; // Mock secret
                                        // Mock aggregate key (33 bytes compressed public key format)
        let aggregate_key = vec![
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x01,
        ];

        Ok(DlcKeygenSession {
            session_id,
            session_secret,
            aggregate_key,
        })
    }

    async fn wait_for_keygen_completion(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<Vec<u8>, KeymeldError> {
        // Return the mock aggregate key immediately
        Ok(session.aggregate_key.clone())
    }

    async fn get_keygen_status(
        &self,
        session: &DlcKeygenSession,
    ) -> Result<KeygenSessionStatus, KeymeldError> {
        // Always return completed for mock
        Ok(KeygenSessionStatus {
            session_id: session.session_id.to_string(),
            status: "completed".to_string(),
            registered_participants: 1,
            expected_participants: 1,
            is_completed: true,
        })
    }

    async fn sign_dlc_batch(
        &self,
        _keygen_session: &DlcKeygenSession,
        _signing_data: &SigningData,
        _contract_params: &ContractParameters,
        _player_user_ids: Vec<UserId>,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        // For E2E tests, we don't need actual signatures
        // Return empty results - the signing flow won't be tested in E2E
        Err(KeymeldError::NotEnabled)
    }

    async fn register_participant(
        &self,
        _session: &DlcKeygenSession,
        _user_id: UserId,
        _registration_data: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        // Mock registration always succeeds
        Ok(())
    }

    async fn get_user_enclave_pubkey(
        &self,
        _session: &DlcKeygenSession,
        _user_id: UserId,
    ) -> Result<String, KeymeldError> {
        // Return a mock enclave public key (hex-encoded 33-byte compressed pubkey)
        // This is a valid format that the client can use for encryption
        Ok("02deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string())
    }
}
