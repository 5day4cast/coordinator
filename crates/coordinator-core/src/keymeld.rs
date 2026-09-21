//! Shared browser and synth registration protocol. Never encrypt to an
//! unattested gateway key, and never send slot or signing credentials to clients.

use crate::RegistrationAssignment;
pub use coordinator_escrow::{
    authorization::PayoutPolicy, escrow::SignedEscrowPolicy, payout, payout_protocol,
};
use coordinator_escrow::{
    escrow::{ApplicationContext, EscrowContext, PublicKeyBytes, Recipient},
    generic,
};
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
    /// Participant consent returned to the authorized Coordinator application.
    /// Keymeld transport carries this only inside enclave-addressed ciphertext.
    pub escrow_policy: Option<SignedEscrowPolicy>,
}

/// Verify a fresh enclave attestation before transferring the participant key.
/// The assignment and PCR pins must come from the coordinator's authenticated
/// ticket response, never from the Keymeld gateway being verified.
pub async fn prepare_registration(
    private_key: &[u8; 32],
    assignment: &RegistrationAssignment,
) -> Result<PreparedRegistration, SdkError> {
    if assignment.payout_policy.is_some() {
        return Err(SdkError::InvalidInput(
            "Payout registration requires explicit wallet consent".into(),
        ));
    }
    prepare_registration_inner(private_key, assignment, None).await
}

/// Encrypt the wallet's existing payout preimage with its approved policy.
/// The browser must validate the policy against its consent before calling.
pub async fn prepare_payout_registration(
    private_key: &[u8; 32],
    payout_preimage: &[u8; 32],
    assignment: &RegistrationAssignment,
) -> Result<PreparedRegistration, SdkError> {
    let policy: PayoutPolicy = serde_json::from_str(
        assignment
            .payout_policy
            .as_deref()
            .ok_or_else(|| SdkError::InvalidInput("Missing payout policy".into()))?,
    )
    .map_err(|e| SdkError::InvalidInput(format!("Invalid payout policy: {e}")))?;
    prepare_registration_inner(private_key, assignment, Some((policy, payout_preimage))).await
}

/// Verify new generic consent against the accepted ticket and participant context.
/// A legacy payout signature is never accepted as permission for the v2 policy.
pub fn verify_registration_policy(
    context: &RegistrationContext,
    expected: Option<&PayoutPolicy>,
    signed: Option<&SignedEscrowPolicy>,
) -> Result<(), SdkError> {
    let (expected, signed) = match (expected, signed) {
        (None, None) => return Ok(()),
        (Some(expected), Some(signed)) => (expected, signed),
        _ => {
            return Err(SdkError::InvalidInput(
                "Entry requires its exact fresh escrow consent".into(),
            ))
        }
    };
    let terms = payout::ContractAuthorization::from_policy(expected)
        .map_err(|error| SdkError::InvalidInput(error.to_string()))?;
    let expected_policy = generic::participant_policy(
        EscrowContext {
            keygen_session_id: context.keygen_session_id.clone(),
            user_id: context.user_id.clone(),
            escrow_id: terms.entry_id,
            manifest_digest: context.manifest_hash.as_slice().try_into().map_err(|_| {
                SdkError::InvalidInput("Invalid registration manifest digest".into())
            })?,
            application: ApplicationContext::commit(
                generic::VERIFIER_ID.into(),
                generic::VERIFIER_VERSION,
                &[],
            )
            .map_err(|error| SdkError::InvalidInput(error.to_string()))?,
        },
        PublicKeyBytes::new(&context.public_key)
            .map_err(|error| SdkError::InvalidInput(error.to_string()))?,
        expected.clone(),
        Recipient {
            encryption_public_key: PublicKeyBytes::new(&terms.market_maker.pubkey.serialize())
                .map_err(|error| SdkError::InvalidInput(error.to_string()))?,
        },
    )
    .map_err(|error| SdkError::InvalidInput(error.to_string()))?;
    if signed.policy != expected_policy {
        return Err(SdkError::InvalidInput(
            "Participant escrow permission differs from the accepted ticket".into(),
        ));
    }
    signed
        .verify(&expected_policy.context, &context.public_key)
        .map_err(|error| SdkError::InvalidInput(error.to_string()))
}

async fn prepare_registration_inner(
    private_key: &[u8; 32],
    assignment: &RegistrationAssignment,
    payout: Option<(PayoutPolicy, &[u8; 32])>,
) -> Result<PreparedRegistration, SdkError> {
    if assignment.manifest_hash.len() != 32 {
        return Err(SdkError::InvalidInput(
            "Invalid session manifest digest".into(),
        ));
    }
    let user_id = UserId::from(assignment.user_id);
    let builder = KeyMeldClient::builder(&assignment.gateway_url, user_id.clone());
    let client = if assignment.dangerous_trust_unattested_enclaves {
        if !assignment.trusted_pcrs.is_empty() {
            return Err(SdkError::InvalidInput(
                "Trusted PCR measurements and unattested trust are mutually exclusive".into(),
            ));
        }
        // Simulated enclaves return no Nitro evidence. The coordinator sets this
        // only from its own development configuration and refuses it on mainnet.
        builder.dangerous_trust_unattested_enclaves()
    } else {
        let measurements = assignment
            .trusted_pcrs
            .iter()
            .map(|(index, value)| {
                hex::decode(value)
                    .map(|bytes| (*index, bytes))
                    .map_err(|_| SdkError::InvalidInput("Invalid trusted PCR measurement".into()))
            })
            .collect::<Result<_, _>>()?;
        builder.attestation_policy(AttestationPolicy::new(measurements)?)
    }
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
    let (encrypted_private_key, escrow_policy) = match payout {
        Some((policy, preimage)) => {
            let terms = payout::ContractAuthorization::from_policy(&policy)
                .map_err(|error| SdkError::InvalidInput(error.to_string()))?;
            let escrow =
                generic::registration(
                    EscrowContext {
                        keygen_session_id: context.keygen_session_id.clone(),
                        user_id: context.user_id.clone(),
                        escrow_id: terms.entry_id,
                        manifest_digest: assignment.manifest_hash.as_slice().try_into().map_err(
                            |_| SdkError::InvalidInput("Invalid manifest digest".into()),
                        )?,
                        application: ApplicationContext::commit(
                            generic::VERIFIER_ID.into(),
                            generic::VERIFIER_VERSION,
                            &[],
                        )
                        .map_err(|error| SdkError::InvalidInput(error.to_string()))?,
                    },
                    private_key,
                    policy,
                    preimage,
                    Recipient {
                        encryption_public_key: PublicKeyBytes::new(
                            &terms.market_maker.pubkey.serialize(),
                        )
                        .map_err(|error| SdkError::InvalidInput(error.to_string()))?,
                    },
                )
                .map_err(|error| SdkError::InvalidInput(error.to_string()))?;
            let signed = escrow.policy.clone();
            (
                credentials.prepare_registration_with_escrow(
                    context.clone(),
                    &enclave.public_key,
                    escrow,
                )?,
                Some(signed),
            )
        }
        None => (
            credentials.prepare_registration(context.clone(), &enclave.public_key)?,
            None,
        ),
    };
    Ok(PreparedRegistration {
        encrypted_private_key,
        auth_pubkey: hex::encode(&context.auth_pubkey),
        context,
        escrow_policy,
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
            dangerous_trust_unattested_enclaves: false,
            payout_policy: None,
        }
    }

    fn escrow_consent_fixture() -> (RegistrationContext, PayoutPolicy, SignedEscrowPolicy) {
        use payout::dlctix::{
            bitcoin::{Amount, FeeRate, Network},
            secp::Scalar,
            EventLockingConditions, MarketMaker, Outcome,
        };
        let credentials = UserCredentials::from_private_key(&[2; 32]).unwrap();
        let context = RegistrationContext {
            keygen_session_id: SessionId::new_v7(),
            manifest_hash: vec![7; 32],
            user_id: UserId::new_v7(),
            enclave_id: 1.into(),
            enclave_key_epoch: 1,
            public_key: credentials.public_key_bytes(),
            auth_pubkey: credentials.public_key_bytes(),
            require_signing_approval: false,
        };
        let terms = payout::ContractAuthorization {
            competition_id: Uuid::now_v7(),
            entry_id: Uuid::now_v7(),
            network: Network::Regtest,
            player_index: 0,
            player_count: 1,
            ticket_hash: [5; 32],
            payout_hash: coordinator_escrow::escrow::sha256(&[6; 32]),
            market_maker: MarketMaker {
                pubkey: Scalar::from_slice(&[3; 32]).unwrap().base_point_mul(),
            },
            event: EventLockingConditions {
                locking_points: vec![Scalar::from_slice(&[4; 32])
                    .unwrap()
                    .base_point_mul()
                    .into()],
                expiry: Some(500_000),
            },
            outcome_payouts: BTreeMap::from([(
                Outcome::Attestation(0),
                BTreeMap::from([(0, 100)]),
            )]),
            funding_value: Amount::from_sat(100_000),
            relative_locktime_block_delta: 72,
            max_fee_rate: FeeRate::from_sat_per_vb_u32(1),
        };
        let policy = PayoutPolicy {
            automatic_lightning_address: None,
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            contract_terms: serde_json::to_string(&terms).unwrap(),
            ark_escrow: None,
        };
        let registration = generic::registration(
            EscrowContext {
                keygen_session_id: context.keygen_session_id.clone(),
                user_id: context.user_id.clone(),
                escrow_id: terms.entry_id,
                manifest_digest: [7; 32],
                application: ApplicationContext::commit("placeholder".into(), 1, &[]).unwrap(),
            },
            &[2; 32],
            policy.clone(),
            &[6; 32],
            Recipient {
                encryption_public_key: PublicKeyBytes::new(&terms.market_maker.pubkey.serialize())
                    .unwrap(),
            },
        )
        .unwrap();
        (context, policy, registration.policy.clone())
    }

    #[test]
    fn enrollment_requires_fresh_exact_generic_permissions_and_signature() {
        let (context, policy, signed) = escrow_consent_fixture();
        verify_registration_policy(&context, Some(&policy), Some(&signed)).unwrap();
        assert!(verify_registration_policy(&context, Some(&policy), None).is_err());
        assert!(verify_registration_policy(&context, None, Some(&signed)).is_err());
        for field in 0..5 {
            let mut changed = signed.clone();
            match field {
                0 => changed.signature[0] ^= 1,
                1 => changed.policy.context.user_id = UserId::new_v7(),
                2 => changed.policy.context.escrow_id = Uuid::now_v7(),
                3 => {
                    changed.policy.grants.remove(generic::RELEASE_ENTRY_KEY);
                }
                _ => {
                    changed
                        .policy
                        .grants
                        .get_mut(generic::RELEASE_PREIMAGE)
                        .unwrap()
                        .condition = coordinator_escrow::escrow::Condition::Unconditional
                }
            }
            assert!(
                verify_registration_policy(&context, Some(&policy), Some(&changed)).is_err(),
                "mutation {field}"
            );
        }
        let mut changed = policy.clone();
        changed.automatic_lightning_address = Some("attacker@wallet.example".into());
        assert!(verify_registration_policy(&context, Some(&changed), Some(&signed)).is_err());
        let mut changed_context = context.clone();
        changed_context.manifest_hash[0] ^= 1;
        assert!(
            verify_registration_policy(&changed_context, Some(&policy), Some(&signed)).is_err()
        );
    }

    fn simulated_enclave(public_key: String) -> Router {
        Router::new().route(
            "/api/v1/enclaves/1/public-key",
            get(move || {
                let public_key = public_key.clone();
                async move {
                    Json(json!({
                        "enclave_id": 1, "public_key": public_key,
                        "attestation_document": "", "pcr_measurements": {},
                        "timestamp": 0, "healthy": true, "key_epoch": 1,
                    }))
                }
            }),
        )
    }

    async fn prepare(
        assignment: &RegistrationAssignment,
    ) -> Result<PreparedRegistration, SdkError> {
        timeout(
            Duration::from_secs(5),
            prepare_registration(&[1; 32], assignment),
        )
        .await
        .expect("registration preparation timed out")
    }

    #[tokio::test]
    async fn simulated_enclaves_need_explicit_unattested_trust_without_pins() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut assignment = assignment(format!("http://{}", listener.local_addr().unwrap()));
        let app = simulated_enclave(assignment.enclave_public_key.clone());
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });

        assignment.trusted_pcrs.insert(0, hex::encode([1; 48]));
        assert!(prepare(&assignment).await.is_err());

        assignment.trusted_pcrs.clear();
        assignment.dangerous_trust_unattested_enclaves = true;
        let prepared = prepare(&assignment).await.unwrap();
        assert_eq!(prepared.context.user_id, UserId::from(assignment.user_id));
        assert_eq!(prepared.context.enclave_key_epoch, 1);
        assert!(!prepared.context.require_signing_approval);
        assert!(!prepared.encrypted_private_key.is_empty());

        assignment.trusted_pcrs.insert(0, hex::encode([1; 48]));
        assert!(prepare(&assignment).await.is_err());

        shutdown.send(()).unwrap();
        timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// Exercises a real simulated keymeld gateway started by `run-keymeld`.
    #[tokio::test]
    #[ignore = "requires COORDINATOR_TEST_KEYMELD_URL pointing at a simulated keymeld gateway"]
    async fn live_simulated_gateway_accepts_unattested_registration_envelopes() {
        let gateway_url = std::env::var("COORDINATOR_TEST_KEYMELD_URL").unwrap();
        let client = KeyMeldClient::builder(&gateway_url, UserId::new_v7())
            .dangerous_trust_unattested_enclaves()
            .build()
            .unwrap();
        let enclave = client.health().get_enclave_key(0).await.unwrap();
        let mut assignment = assignment(gateway_url);
        assignment.enclave_id = 0;
        assignment.enclave_public_key = enclave.public_key;
        assignment.enclave_key_epoch = enclave.key_epoch;
        assert!(prepare_registration(&[1; 32], &assignment).await.is_err());
        assignment.dangerous_trust_unattested_enclaves = true;
        let prepared = prepare_registration(&[1; 32], &assignment).await.unwrap();
        assert_eq!(
            prepared.context.enclave_key_epoch,
            assignment.enclave_key_epoch
        );
        assert!(!prepared.encrypted_private_key.is_empty());
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
