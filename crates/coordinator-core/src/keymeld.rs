//! Shared browser and synth registration protocol. Never encrypt to an
//! unattested gateway key, and never send slot or signing credentials to clients.

use crate::RegistrationAssignment;
use keymeld_sdk::{
    types::RegistrationContext, AttestationPolicy, KeyMeldClient, SdkError, SessionId,
    UserCredentials, UserId,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct PreparedRegistration {
    pub encrypted_private_key: String,
    pub auth_pubkey: String,
    pub context: RegistrationContext,
}

/// Verify a fresh enclave attestation before transferring the participant key.
/// The assignment and PCR pins must come from the coordinator's authenticated
/// ticket response, never from the Keymeld gateway being verified.
pub async fn prepare_registration(
    private_key: &[u8; 32],
    assignment: &RegistrationAssignment,
) -> Result<PreparedRegistration, SdkError> {
    if assignment.manifest_hash.len() != 32 {
        return Err(SdkError::InvalidInput(
            "Invalid session manifest digest".into(),
        ));
    }
    let measurements = assignment
        .trusted_pcrs
        .iter()
        .map(|(index, value)| {
            hex::decode(value)
                .map(|bytes| (*index, bytes))
                .map_err(|_| SdkError::InvalidInput("Invalid trusted PCR measurement".into()))
        })
        .collect::<Result<_, _>>()?;
    let policy = AttestationPolicy::new(measurements)?;
    let user_id = UserId::from(assignment.user_id);
    let client = KeyMeldClient::builder(&assignment.gateway_url, user_id.clone())
        .attestation_policy(policy)
        .build()?;
    // The SDK generates the nonce locally and verifies the original COSE bytes.
    let enclave = client
        .health()
        .get_enclave_key(assignment.enclave_id)
        .await?;
    if enclave.public_key != assignment.enclave_public_key
        || enclave.key_epoch != assignment.enclave_key_epoch
    {
        return Err(SdkError::InvalidInput(
            "Enclave assignment changed; request a new ticket registration context".into(),
        ));
    }
    let credentials = UserCredentials::from_private_key(private_key)?;
    let context = RegistrationContext {
        keygen_session_id: SessionId::new(&assignment.session_id),
        manifest_hash: assignment.manifest_hash.clone(),
        user_id,
        enclave_id: assignment.enclave_id.into(),
        enclave_key_epoch: assignment.enclave_key_epoch,
        public_key: credentials.public_key_bytes(),
        auth_pubkey: credentials.derive_session_auth_pubkey(&assignment.session_id)?,
        // Coordinator holds a separate signing credential for unattended DLCs.
        require_signing_approval: false,
    };
    let encrypted_private_key =
        credentials.prepare_registration(context.clone(), &enclave.public_key)?;
    Ok(PreparedRegistration {
        encrypted_private_key,
        auth_pubkey: hex::encode(&context.auth_pubkey),
        context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, routing::get, Json, Router};
    use serde_json::json;
    use std::{collections::BTreeMap, time::Duration};
    use tokio::{
        net::TcpListener,
        sync::{mpsc, oneshot},
        time::timeout,
    };
    use uuid::Uuid;

    fn assignment(gateway_url: String) -> RegistrationAssignment {
        RegistrationAssignment {
            session_id: Uuid::now_v7().to_string(),
            user_id: Uuid::now_v7(),
            manifest_hash: vec![7; 32],
            enclave_id: 1,
            enclave_key_epoch: 1,
            enclave_public_key:
                "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798".into(),
            gateway_url,
            trusted_pcrs: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn missing_trusted_measurements_rejects_registration_before_network_access() {
        let error = prepare_registration(&[1; 32], &assignment("http://127.0.0.1:1".into()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Pin PCR0"), "{error}");
    }

    #[tokio::test]
    async fn forged_enclave_attestation_never_produces_a_registration_envelope() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut assignment = assignment(format!("http://{}", listener.local_addr().unwrap()));
        assignment.trusted_pcrs.insert(0, hex::encode([1; 48]));
        let (requests, mut received) = mpsc::channel(2);
        let public_key = assignment.enclave_public_key.clone();
        let app = Router::new().route(
            "/api/v1/enclaves/1/public-key",
            get(move |Query(query): Query<BTreeMap<String, String>>| {
                let requests = requests.clone();
                let public_key = public_key.clone();
                async move {
                    requests.try_send(query["nonce"].clone()).unwrap();
                    Json(json!({
                        "enclave_id": 1, "public_key": public_key,
                        "attestation_document": "a0", "pcr_measurements": {},
                        "timestamp": 0, "healthy": true, "key_epoch": 1,
                    }))
                }
            }),
        );
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        for _ in 0..2 {
            let result = timeout(
                Duration::from_secs(5),
                prepare_registration(&[1; 32], &assignment),
            )
            .await
            .unwrap();
            assert!(result.is_err());
        }
        let first = timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hex::decode(&first).unwrap().len(), 32);
        assert_eq!(hex::decode(&second).unwrap().len(), 32);
        assert_ne!(
            first, second,
            "Each attestation lookup must use a fresh challenge"
        );
        shutdown.send(()).unwrap();
        timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
