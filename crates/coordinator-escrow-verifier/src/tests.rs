use super::*;
use coordinator_escrow::generic::{
    registration, CONTRACT_RULE, RELEASE_ENTRY_KEY, RELEASE_PREIMAGE, SETTLEMENT_RULE,
    SIGN_CONTRACT,
};
use coordinator_escrow::payout::dlctix::{
    self,
    bitcoin::{hashes::Hash, Amount, FeeRate, Network, OutPoint},
    secp::{Point, Scalar},
    ContractParameters, EventLockingConditions, MarketMaker, Outcome, Player, TicketedDLC,
};
use coordinator_escrow::payout::lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use coordinator_escrow::payout_protocol::SignedInvoiceAuthorization;
use keymeld_core::authorization::SessionAuthorizationManifest;
use keymeld_core::escrow::{
    ActionAttempt, ApplicationContext, EscrowContext, Recipient, ScopeSigner,
};
use keymeld_core::protocol::SubsetDefinition;
use std::time::Duration;

fn public(secret: u8) -> Vec<u8> {
    secp256k1::PublicKey::from_secret_key(
        &secp256k1::Secp256k1::new(),
        &secp256k1::SecretKey::from_byte_array([secret; 32]).unwrap(),
    )
    .serialize()
    .to_vec()
}
struct Fixture {
    manifest: SignedSessionManifest,
    policy: SignedEscrowPolicy,
    policies: BTreeMap<UserId, SignedEscrowPolicy>,
    keys: BTreeMap<UserId, PublicKeyBytes>,
    contract: ContractCommitment,
}
fn fixture(automatic: bool) -> Fixture {
    let user = UserId::new_v7();
    let maker = UserId::new_v7();
    let manifest = SignedSessionManifest::sign(
        SessionAuthorizationManifest {
            keygen_session_id: SessionId::new_v7(),
            coordinator_user_id: maker.clone(),
            creator_pubkey: public(11),
            signing_pubkey: public(12),
            session_public_key: public(10),
            participant_verifiers: BTreeMap::from([
                (user.clone(), public(13)),
                (maker.clone(), public(17)),
            ]),
            timeout_secs: 3600,
            max_signing_sessions: None,
            encrypted_taproot_tweak: "encrypted".into(),
            subset_definitions: vec![SubsetDefinition {
                subset_id: Uuid::from_u128(1),
                participants: vec![user.clone()],
            }],
        },
        &[11; 32],
    )
    .unwrap();
    let params = ContractParameters {
        market_maker: MarketMaker {
            pubkey: Scalar::from_slice(&[18; 32]).unwrap().base_point_mul(),
        },
        players: vec![Player {
            pubkey: Scalar::from_slice(&[14; 32]).unwrap().base_point_mul(),
            ticket_hash: payout::sha256(&[5; 32]),
            payout_hash: payout::sha256(&[6; 32]),
        }],
        event: EventLockingConditions {
            locking_points: vec![Scalar::from_slice(&[4; 32])
                .unwrap()
                .base_point_mul()
                .into()],
            expiry: Some(500_000),
        },
        outcome_payouts: BTreeMap::from([
            (Outcome::Attestation(0), BTreeMap::from([(0, 100)])),
            (Outcome::Expiry, BTreeMap::from([(0, 100)])),
        ]),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value: Amount::from_sat(100_000),
        relative_locktime_block_delta: 72,
    };
    let terms = ContractAuthorization {
        competition_id: Uuid::now_v7(),
        entry_id: Uuid::now_v7(),
        network: Network::Regtest,
        player_index: 0,
        player_count: 1,
        ticket_hash: params.players[0].ticket_hash,
        payout_hash: params.players[0].payout_hash,
        market_maker: params.market_maker.clone(),
        event: params.event.clone(),
        outcome_payouts: params.outcome_payouts.clone(),
        funding_value: params.funding_value,
        relative_locktime_block_delta: 72,
        max_fee_rate: params.fee_rate,
    };
    let context = EscrowContext {
        keygen_session_id: manifest.manifest.keygen_session_id.clone(),
        user_id: user.clone(),
        escrow_id: terms.entry_id,
        manifest_digest: manifest.digest().unwrap().try_into().unwrap(),
        application: ApplicationContext::commit("placeholder".into(), 1, &[]).unwrap(),
    };
    let registration = registration(
        context,
        &[14; 32],
        PayoutPolicy {
            automatic_lightning_address: automatic.then(|| "alice+prize@wallet.example".into()),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            contract_terms: serde_json::to_string(&terms).unwrap(),
        },
        &[6; 32],
        Recipient {
            encryption_public_key: PublicKeyBytes::new(&public(18)).unwrap(),
        },
    )
    .unwrap();
    let policy = registration.policy.clone();
    Fixture {
        manifest,
        policies: BTreeMap::from([(user.clone(), policy.clone())]),
        policy,
        keys: BTreeMap::from([
            (user, PublicKeyBytes::new(&public(14)).unwrap()),
            (maker, PublicKeyBytes::new(&public(18)).unwrap()),
        ]),
        contract: ContractCommitment {
            contract_parameters: params,
            funding_outpoint: OutPoint::null(),
        },
    }
}
impl Fixture {
    fn bind(&self, verifier: &CoordinatorVerifier) -> Payload {
        verifier
            .bind(
                BindView {
                    manifest: &self.manifest,
                    policy: &self.policy,
                    participant_policies: &self.policies,
                    participant_public_keys: &self.keys,
                },
                &Payload::encode(&ContractBinding {
                    contract: self.contract.clone(),
                })
                .unwrap(),
            )
            .unwrap()
    }
    fn settlement_parameters(&self, attempt: &ActionAttempt) -> Payload {
        self.settlement_parameters_with_invoice(attempt, invoice())
    }
    fn settlement_parameters_with_invoice(
        &self,
        attempt: &ActionAttempt,
        invoice: String,
    ) -> Payload {
        let (_, terms) = policy(&self.policy).unwrap();
        let authorization = SignedInvoiceAuthorization::sign(
            &[14; 32],
            InvoiceAuthorizationContext {
                keygen_session_id: self.manifest.manifest.keygen_session_id.clone(),
                user_id: self.policy.policy.context.user_id.clone(),
                claim_id: attempt.attempt_id,
                competition_id: terms.competition_id,
                entry_id: terms.entry_id,
                contract_digest: payout::contract_digest(&self.contract).unwrap(),
                invoice_digest: payout::invoice_digest(&invoice),
                amount_msat: 100_000_000,
                expires_at: now().unwrap() + 600,
            },
        )
        .unwrap();
        Payload::encode(&ActionParameters::PrepareSettlement {
            claim_id: attempt.attempt_id,
            contract_signatures: serde_json::to_string(&sign_contract(&self.contract)).unwrap(),
            attestation: hex::encode([4; 32]),
            method: PayoutMethod::Invoice {
                invoice,
                authorization,
            },
        })
        .unwrap()
    }
    fn prepare_view<'a>(
        &'a self,
        bound: &'a Payload,
        attempt: &'a ActionAttempt,
        permission: &'a str,
        prior: &'a BTreeMap<String, PreparedAction>,
    ) -> PreparationView<'a> {
        PreparationView {
            manifest: &self.manifest,
            policy: &self.policy,
            rule: if permission == SIGN_CONTRACT {
                CONTRACT_RULE
            } else {
                SETTLEMENT_RULE
            },
            permission_id: permission,
            attempt,
            bound_state: bound,
            participant_public_keys: &self.keys,
            prior_preparations: prior,
        }
    }
    fn execute_view<'a>(
        &'a self,
        bound: &'a Payload,
        attempt: &'a ActionAttempt,
        permission: &'a str,
    ) -> ExecutionView<'a> {
        ExecutionView {
            manifest: &self.manifest,
            policy: &self.policy,
            rule: if permission == SIGN_CONTRACT {
                CONTRACT_RULE
            } else {
                SETTLEMENT_RULE
            },
            permission_id: permission,
            attempt,
            bound_state: bound,
        }
    }
    fn scope(&self) -> SigningScope {
        let batch = payout::signing_requirements(&self.contract)
            .unwrap()
            .into_iter()
            .map(|(message, requirement)| {
                let signers = requirement
                    .signers
                    .iter()
                    .map(|key| {
                        let (user, public_key) = self
                            .keys
                            .iter()
                            .find(|(_, value)| value.as_bytes() == key)
                            .unwrap();
                        ScopeSigner {
                            user_id: user.clone(),
                            public_key: public_key.clone(),
                        }
                    })
                    .collect::<Vec<_>>();
                escrow::SigningItem {
                    item_id: Uuid::now_v7(),
                    message_digest: escrow::sha256(&message),
                    subset_id: (signers.len() == 1).then_some(Uuid::from_u128(1)),
                    signers,
                    tweak: KeyTweak::None,
                    adaptor: requirement
                        .adaptor_point
                        .map(|point| AdaptorContext::Single {
                            adaptor_id: Uuid::now_v7(),
                            point: PublicKeyBytes::new(&point).unwrap(),
                        })
                        .unwrap_or(AdaptorContext::None),
                }
            })
            .collect();
        SigningScope {
            session_tweak: KeyTweak::None,
            batch,
        }
    }
}
fn sign_contract(contract: &ContractCommitment) -> ContractSignatures {
    use dlctix::musig2::{self, AggNonce, KeyAggContext, PartialSignature, SecNonce};
    let dlc = TicketedDLC::new(
        contract.contract_parameters.clone(),
        contract.funding_outpoint,
    )
    .unwrap();
    let data = dlc.signing_data().unwrap();
    let secrets: BTreeMap<Point, Scalar> = [14, 18]
        .into_iter()
        .map(|n| {
            let sk = Scalar::from_slice(&[n; 32]).unwrap();
            (sk.base_point_mul(), sk)
        })
        .collect();
    let mut signatures = ContractSignatures {
        expiry_tx_signature: None,
        outcome_tx_signatures: BTreeMap::new(),
        split_tx_signatures: BTreeMap::new(),
    };
    let sign = |signers: Vec<Point>, hash: &[u8; 32], adaptor: Option<dlctix::secp::MaybePoint>| {
        let ctx = KeyAggContext::new(signers.clone()).unwrap();
        let nonces: Vec<_> = signers
            .iter()
            .map(|pk| {
                SecNonce::generate(
                    &mut rand::rng(),
                    secrets[pk],
                    ctx.aggregated_pubkey::<Point>(),
                    hash,
                    [],
                )
            })
            .collect();
        let aggregate = AggNonce::sum(nonces.iter().map(|n| n.public_nonce()));
        let partials: Vec<PartialSignature> = signers
            .iter()
            .zip(nonces)
            .map(|(pk, nonce)| match adaptor {
                Some(point) => {
                    musig2::adaptor::sign_partial(&ctx, secrets[pk], nonce, &aggregate, point, hash)
                        .unwrap()
                }
                None => musig2::sign_partial(&ctx, secrets[pk], nonce, &aggregate, hash).unwrap(),
            })
            .collect();
        (ctx, aggregate, partials)
    };
    for (outcome, hash) in &data.outcome_sighashes {
        let adaptor = data.adaptor_point(outcome).unwrap();
        let (ctx, nonce, partials) = sign(data.funding_signers.clone(), hash, adaptor);
        match outcome {
            Outcome::Attestation(index) => {
                signatures.outcome_tx_signatures.insert(
                    *index,
                    musig2::adaptor::aggregate_partial_signatures(
                        &ctx,
                        &nonce,
                        adaptor.unwrap(),
                        partials,
                        hash,
                    )
                    .unwrap(),
                );
            }
            Outcome::Expiry => {
                signatures.expiry_tx_signature = Some(
                    musig2::aggregate_partial_signatures(&ctx, &nonce, partials, hash).unwrap(),
                )
            }
        }
    }
    for (win, hash) in &data.split_sighashes {
        let (ctx, nonce, partials) = sign(data.split_signers[&win.outcome].clone(), hash, None);
        signatures.split_tx_signatures.insert(
            *win,
            musig2::aggregate_partial_signatures(&ctx, &nonce, partials, hash).unwrap(),
        );
    }
    signatures
}
fn invoice() -> String {
    invoice_with_preimage([9; 32])
}
fn invoice_with_preimage(preimage: [u8; 32]) -> String {
    use dlctix::bitcoin::{
        hashes::sha256,
        secp256k1::{Secp256k1, SecretKey},
    };
    InvoiceBuilder::new(Currency::Regtest)
        .amount_milli_satoshis(100_000_000)
        .description("Direct fallback invoice".into())
        .payment_hash(sha256::Hash::from_byte_array(payout::sha256(&preimage)))
        .payment_secret(PaymentSecret([10; 32]))
        .duration_since_epoch(Duration::from_secs(now().unwrap()))
        .expiry_time(Duration::from_secs(600))
        .min_final_cltv_expiry_delta(18)
        .build_signed(|hash| {
            Secp256k1::new()
                .sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[11; 32]).unwrap())
        })
        .unwrap()
        .to_string()
}

#[test]
fn registration_rejects_disabled_automatic_policy_but_retains_recovery() {
    let verifier = CoordinatorVerifier::default();
    let fallback = fixture(false);
    assert!(verifier
        .validate_registration(RegistrationView {
            manifest: &fallback.manifest,
            policy: &fallback.policy,
            restoring: false
        })
        .is_ok());
    let automatic = fixture(true);
    assert!(verifier
        .validate_registration(RegistrationView {
            manifest: &automatic.manifest,
            policy: &automatic.policy,
            restoring: false
        })
        .is_err());
    assert!(verifier
        .validate_registration(RegistrationView {
            manifest: &automatic.manifest,
            policy: &automatic.policy,
            restoring: true
        })
        .is_ok());
    for mutation in 0..6 {
        let mut policy = fallback.policy.clone();
        match mutation {
            0 => {
                policy.policy.grants.remove(RELEASE_ENTRY_KEY);
            }
            1 => policy.policy.context.escrow_id = Uuid::now_v7(),
            4 | 5 => {
                let id = if mutation == 4 {
                    RELEASE_PREIMAGE
                } else {
                    RELEASE_ENTRY_KEY
                };
                policy.policy.grants.get_mut(id).unwrap().preparation =
                    escrow::PreparationPolicy::Single;
            }
            2 | 3 => {
                let id = if mutation == 2 {
                    RELEASE_PREIMAGE
                } else {
                    RELEASE_ENTRY_KEY
                };
                match &mut policy.policy.grants.get_mut(id).unwrap().operation {
                    Permission::ReleaseSecret { recipient, .. }
                    | Permission::ReleaseSigningKey { recipient, .. } => {
                        recipient.encryption_public_key = PublicKeyBytes::new(&public(19)).unwrap();
                    }
                    _ => unreachable!(),
                }
            }
            _ => unreachable!(),
        }
        policy = SignedEscrowPolicy::sign(policy.policy, &[14; 32]).unwrap();
        assert!(
            verifier
                .validate_registration(RegistrationView {
                    manifest: &fallback.manifest,
                    policy: &policy,
                    restoring: false
                })
                .is_err(),
            "mutation {mutation}"
        );
    }
}

#[test]
fn binding_rejects_missing_policies_changed_economics_and_wrong_roster() {
    let verifier = CoordinatorVerifier::default();
    let f = fixture(false);
    let data = Payload::encode(&ContractBinding {
        contract: f.contract.clone(),
    })
    .unwrap();
    let empty = BTreeMap::new();
    assert!(verifier
        .bind(
            BindView {
                manifest: &f.manifest,
                policy: &f.policy,
                participant_policies: &empty,
                participant_public_keys: &f.keys
            },
            &data
        )
        .is_err());
    let mut changed = f.contract.clone();
    changed.contract_parameters.funding_value = Amount::from_sat(99_000);
    assert!(verifier
        .bind(
            BindView {
                manifest: &f.manifest,
                policy: &f.policy,
                participant_policies: &f.policies,
                participant_public_keys: &f.keys
            },
            &Payload::encode(&ContractBinding { contract: changed }).unwrap()
        )
        .is_err());
    let mut keys = f.keys.clone();
    keys.insert(
        f.policy.policy.context.user_id.clone(),
        PublicKeyBytes::new(&public(20)).unwrap(),
    );
    assert!(verifier
        .bind(
            BindView {
                manifest: &f.manifest,
                policy: &f.policy,
                participant_policies: &f.policies,
                participant_public_keys: &keys
            },
            &data
        )
        .is_err());
}

#[tokio::test]
async fn signing_derives_messages_keys_subsets_tweaks_and_adaptors_from_bound_contract() {
    let verifier = CoordinatorVerifier::default();
    let f = fixture(false);
    let bound = f.bind(&verifier);
    let attempt = ActionAttempt {
        attempt_id: Uuid::now_v7(),
        signing_session_id: Some(SessionId::new_v7()),
    };
    let prior = BTreeMap::new();
    let scope = f.scope();
    let params = Payload::encode(&ActionParameters::SignContract {
        scope: scope.clone(),
    })
    .unwrap();
    let prepared = verifier
        .prepare(
            f.prepare_view(&bound, &attempt, SIGN_CONTRACT, &prior),
            &params,
        )
        .await
        .unwrap();
    verifier
        .verify_execution(
            f.execute_view(&bound, &attempt, SIGN_CONTRACT),
            &prepared,
            &Payload::default(),
        )
        .await
        .unwrap();
    for field in 0..6 {
        let mut bad = scope.clone();
        match field {
            0 => bad.batch[0].message_digest[0] ^= 1,
            1 => bad.batch[0].signers[0].public_key = PublicKeyBytes::new(&public(21)).unwrap(),
            2 => bad.batch[0].subset_id = Some(Uuid::now_v7()),
            3 => bad.batch[0].tweak = KeyTweak::TaprootKeyPath,
            4 => {
                let item = bad
                    .batch
                    .iter_mut()
                    .find(|item| matches!(item.adaptor, AdaptorContext::Single { .. }))
                    .unwrap();
                item.adaptor = AdaptorContext::None;
            }
            _ => {
                bad.batch.pop();
            }
        }
        let params = Payload::encode(&ActionParameters::SignContract { scope: bad }).unwrap();
        assert!(
            verifier
                .prepare(
                    f.prepare_view(&bound, &attempt, SIGN_CONTRACT, &prior),
                    &params
                )
                .await
                .is_err(),
            "mutation {field}"
        );
    }
    let wrong_attempt = ActionAttempt {
        attempt_id: attempt.attempt_id,
        signing_session_id: None,
    };
    assert!(verifier
        .prepare(
            f.prepare_view(&bound, &wrong_attempt, SIGN_CONTRACT, &prior),
            &params
        )
        .await
        .is_err());
}

#[tokio::test]
async fn payment_releases_only_independent_grants_and_restores_the_same_invoice() {
    let verifier = CoordinatorVerifier::default();
    let f = fixture(false);
    let bound = f.bind(&verifier);
    let attempt = ActionAttempt {
        attempt_id: Uuid::now_v7(),
        signing_session_id: None,
    };
    let empty = BTreeMap::new();
    let params = f.settlement_parameters(&attempt);
    assert!(verifier
        .prepare(
            f.prepare_view(&bound, &attempt, RELEASE_ENTRY_KEY, &empty),
            &params
        )
        .await
        .is_err());
    let first = verifier
        .prepare(
            f.prepare_view(&bound, &attempt, RELEASE_PREIMAGE, &empty),
            &params,
        )
        .await
        .unwrap();
    let prior = BTreeMap::from([(RELEASE_PREIMAGE.into(), first.clone())]);
    // A fresh verifier has no network client or preparation cache. The engine's
    // authenticated sibling state is sufficient for the distinct key permission.
    let restored = CoordinatorVerifier::default();
    let second = restored
        .prepare(
            f.prepare_view(&bound, &attempt, RELEASE_ENTRY_KEY, &prior),
            &params,
        )
        .await
        .unwrap();
    assert_eq!(first.output, second.output);
    assert!(matches!(first.action, Action::ReleaseSecret { .. }));
    assert!(matches!(second.action, Action::ReleaseSigningKey { .. }));
    let wrong = Payload::encode(&PaymentEvidence {
        payment_preimage: [8; 32],
    })
    .unwrap();
    assert!(restored
        .verify_execution(
            f.execute_view(&bound, &attempt, RELEASE_PREIMAGE),
            &first,
            &wrong
        )
        .await
        .is_err());
    let proof = Payload::encode(&PaymentEvidence {
        payment_preimage: [9; 32],
    })
    .unwrap();
    restored
        .verify_execution(
            f.execute_view(&bound, &attempt, RELEASE_PREIMAGE),
            &first,
            &proof,
        )
        .await
        .unwrap();
    restored
        .verify_execution(
            f.execute_view(&bound, &attempt, RELEASE_ENTRY_KEY),
            &second,
            &proof,
        )
        .await
        .unwrap();
    assert!(restored
        .verify_execution(
            f.execute_view(&bound, &attempt, RELEASE_ENTRY_KEY),
            &first,
            &proof
        )
        .await
        .is_err());
    let mut changed = first.clone();
    changed.output = Payload::default();
    assert!(restored
        .verify_execution(
            f.execute_view(&bound, &attempt, RELEASE_PREIMAGE),
            &changed,
            &proof
        )
        .await
        .is_err());
    let attempt2 = ActionAttempt {
        attempt_id: Uuid::now_v7(),
        signing_session_id: None,
    };
    let params2 = f.settlement_parameters(&attempt2);
    assert!(restored
        .prepare(
            f.prepare_view(&bound, &attempt2, RELEASE_PREIMAGE, &empty),
            &params2
        )
        .await
        .is_err());
}

#[tokio::test]
async fn preparation_rejects_forged_invoice_authorization_incomplete_signatures_and_outcome() {
    let verifier = CoordinatorVerifier::default();
    let f = fixture(false);
    let bound = f.bind(&verifier);
    let attempt = ActionAttempt {
        attempt_id: Uuid::now_v7(),
        signing_session_id: None,
    };
    let prior = BTreeMap::new();
    for field in 0..4 {
        let mut params: ActionParameters = f.settlement_parameters(&attempt).decode().unwrap();
        let ActionParameters::PrepareSettlement {
            contract_signatures,
            attestation,
            method,
            ..
        } = &mut params
        else {
            unreachable!()
        };
        match field {
            0 => *contract_signatures = "{}".into(),
            1 => *attestation = hex::encode([7; 32]),
            2 => {
                let PayoutMethod::Invoice { authorization, .. } = method else {
                    unreachable!()
                };
                authorization.signature[0] ^= 1;
            }
            _ => {
                let PayoutMethod::Invoice { authorization, .. } = method else {
                    unreachable!()
                };
                authorization.context.amount_msat += 1;
            }
        }
        assert!(verifier
            .prepare(
                f.prepare_view(&bound, &attempt, RELEASE_PREIMAGE, &prior),
                &Payload::encode(&params).unwrap()
            )
            .await
            .is_err());
    }
}

#[cfg(feature = "lnurl")]
#[tokio::test]
async fn enclave_tls_prepares_automatic_invoice_and_disabled_instance_recovers_paid_claim() {
    let f = fixture(true);
    let (client, server) =
        crate::lnurl_transport::tests::automatic_payout_tls_fixture(invoice()).await;
    let verifier = CoordinatorVerifier::with_lnurl(client);
    verifier
        .validate_registration(RegistrationView {
            manifest: &f.manifest,
            policy: &f.policy,
            restoring: false,
        })
        .unwrap();
    let bound = f.bind(&verifier);
    let attempt = ActionAttempt {
        attempt_id: Uuid::now_v7(),
        signing_session_id: None,
    };
    let mut params: ActionParameters = f.settlement_parameters(&attempt).decode().unwrap();
    let ActionParameters::PrepareSettlement { method, .. } = &mut params else {
        unreachable!()
    };
    *method = PayoutMethod::Automatic;
    let params = Payload::encode(&params).unwrap();
    let first = verifier
        .prepare(
            f.prepare_view(&bound, &attempt, RELEASE_PREIMAGE, &BTreeMap::new()),
            &params,
        )
        .await
        .unwrap();
    server.await.unwrap();
    let disabled = CoordinatorVerifier::default();
    let prior = BTreeMap::from([(RELEASE_PREIMAGE.into(), first.clone())]);
    let second = disabled
        .prepare(
            f.prepare_view(&bound, &attempt, RELEASE_ENTRY_KEY, &prior),
            &params,
        )
        .await
        .unwrap();
    assert_eq!(first.output, second.output);
    let proof = Payload::encode(&PaymentEvidence {
        payment_preimage: [9; 32],
    })
    .unwrap();
    disabled
        .verify_execution(
            f.execute_view(&bound, &attempt, RELEASE_PREIMAGE),
            &first,
            &proof,
        )
        .await
        .unwrap();
    disabled
        .verify_execution(
            f.execute_view(&bound, &attempt, RELEASE_ENTRY_KEY),
            &second,
            &proof,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn renewed_invoice_keeps_late_paid_candidate_and_freezes_both_releases() {
    let verifier = CoordinatorVerifier::default();
    let f = fixture(false);
    let bound = f.bind(&verifier);
    let first_attempt = ActionAttempt {
        attempt_id: Uuid::now_v7(),
        signing_session_id: None,
    };
    let renewed_attempt = ActionAttempt {
        attempt_id: Uuid::now_v7(),
        signing_session_id: None,
    };
    let empty = BTreeMap::new();
    let first_params = f.settlement_parameters(&first_attempt);
    let first_preimage = verifier
        .prepare(
            f.prepare_view(&bound, &first_attempt, RELEASE_PREIMAGE, &empty),
            &first_params,
        )
        .await
        .unwrap();
    let first_prior = BTreeMap::from([(RELEASE_PREIMAGE.into(), first_preimage.clone())]);
    let first_key = verifier
        .prepare(
            f.prepare_view(&bound, &first_attempt, RELEASE_ENTRY_KEY, &first_prior),
            &first_params,
        )
        .await
        .unwrap();
    let renewed_params =
        f.settlement_parameters_with_invoice(&renewed_attempt, invoice_with_preimage([22; 32]));
    // The generic engine authenticates the previous same-permission candidate.
    let renewed_preimage = verifier
        .prepare(
            f.prepare_view(&bound, &renewed_attempt, RELEASE_PREIMAGE, &first_prior),
            &renewed_params,
        )
        .await
        .unwrap();
    assert_eq!(first_preimage.action, renewed_preimage.action);
    assert_ne!(first_preimage.output, renewed_preimage.output);
    let renewed_prior = BTreeMap::from([
        (RELEASE_PREIMAGE.into(), renewed_preimage.clone()),
        (RELEASE_ENTRY_KEY.into(), first_key.clone()),
    ]);
    let renewed_key = verifier
        .prepare(
            f.prepare_view(&bound, &renewed_attempt, RELEASE_ENTRY_KEY, &renewed_prior),
            &renewed_params,
        )
        .await
        .unwrap();
    assert_eq!(first_key.action, renewed_key.action);
    assert_eq!(renewed_preimage.output, renewed_key.output);
    // A key candidate must use its sibling invoice, never a previous candidate's
    // valid receipt, even when both candidates authorize the same release action.
    assert!(verifier
        .prepare(
            f.prepare_view(&bound, &renewed_attempt, RELEASE_ENTRY_KEY, &first_prior),
            &renewed_params
        )
        .await
        .is_err());
    let late_proof = Payload::encode(&PaymentEvidence {
        payment_preimage: [9; 32],
    })
    .unwrap();
    verifier
        .verify_execution(
            f.execute_view(&bound, &first_attempt, RELEASE_PREIMAGE),
            &first_preimage,
            &late_proof,
        )
        .await
        .unwrap();
    verifier
        .verify_execution(
            f.execute_view(&bound, &first_attempt, RELEASE_ENTRY_KEY),
            &first_key,
            &late_proof,
        )
        .await
        .unwrap();
    // A late valid proof for the old invoice remains valid after renewal. Once
    // it wins, neither grant may release under the competing candidate.
    let renewed_proof = Payload::encode(&PaymentEvidence {
        payment_preimage: [22; 32],
    })
    .unwrap();
    for (permission, prepared) in [
        (RELEASE_PREIMAGE, &renewed_preimage),
        (RELEASE_ENTRY_KEY, &renewed_key),
    ] {
        assert!(verifier
            .verify_execution(
                f.execute_view(&bound, &renewed_attempt, permission),
                prepared,
                &renewed_proof
            )
            .await
            .is_err());
    }
    // Authenticated Executed-receipt recovery has no original payment evidence.
    // Rehydrate the same claim fence before the engine recovers the first grant,
    // so the other grant cannot execute under a different paid candidate.
    let restored = CoordinatorVerifier::default();
    let mut substituted = first_preimage.clone();
    substituted.output = renewed_preimage.output.clone();
    assert!(restored
        .restore_execution(
            f.execute_view(&bound, &first_attempt, RELEASE_PREIMAGE),
            &substituted,
        )
        .await
        .is_err());
    restored
        .restore_execution(
            f.execute_view(&bound, &first_attempt, RELEASE_PREIMAGE),
            &first_preimage,
        )
        .await
        .unwrap();
    assert!(restored
        .verify_execution(
            f.execute_view(&bound, &renewed_attempt, RELEASE_ENTRY_KEY),
            &renewed_key,
            &renewed_proof,
        )
        .await
        .is_err());
    restored
        .restore_execution(
            f.execute_view(&bound, &first_attempt, RELEASE_ENTRY_KEY),
            &first_key,
        )
        .await
        .unwrap();
}
