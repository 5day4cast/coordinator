use super::*;
use coordinator_escrow::authorization::ArkEscrowPolicy;
use coordinator_escrow::generic::{
    registration, ARK_ESCROW_RULE, ARK_REFUND_RULE, CONTRACT_RULE, RELEASE_ENTRY_KEY,
    RELEASE_PREIMAGE, SETTLEMENT_RULE, SIGN_ARK_ESCROW, SIGN_ARK_REFUND, SIGN_CONTRACT,
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
    fixture_with(automatic, None)
}
fn fixture_with(automatic: bool, ark_escrow: Option<ArkEscrowPolicy>) -> Fixture {
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
            ark_escrow,
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
            ark_funding: None,
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
            rule: rule(permission),
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
            rule: rule(permission),
            permission_id: permission,
            attempt,
            bound_state: bound,
        }
    }
    fn scope(&self) -> SigningScope {
        self.scope_for(&self.contract)
    }
    fn scope_for(&self, contract: &ContractCommitment) -> SigningScope {
        let batch = payout::signing_requirements(contract)
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
fn rule(permission: &str) -> &'static str {
    match permission {
        SIGN_CONTRACT => CONTRACT_RULE,
        SIGN_ARK_ESCROW => ARK_ESCROW_RULE,
        SIGN_ARK_REFUND => ARK_REFUND_RULE,
        _ => SETTLEMENT_RULE,
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
    invoice_for(100_000, preimage)
}
/// An invoice for `sats`, settled by `preimage`, issued for a Lightning Address: it commits
/// to the provider's metadata for that address, as LUD-06 requires.
#[cfg(feature = "lnurl")]
fn address_invoice_for(sats: u64, preimage: [u8; 32], metadata: &str) -> String {
    use dlctix::bitcoin::{
        hashes::sha256,
        secp256k1::{Secp256k1, SecretKey},
    };
    InvoiceBuilder::new(Currency::Regtest)
        .amount_milli_satoshis(sats * 1000)
        .description_hash(sha256::Hash::from_byte_array(payout::sha256(
            metadata.as_bytes(),
        )))
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
/// An invoice for `sats`, settled by `preimage`.
fn invoice_for(sats: u64, preimage: [u8; 32]) -> String {
    use dlctix::bitcoin::{
        hashes::sha256,
        secp256k1::{Secp256k1, SecretKey},
    };
    InvoiceBuilder::new(Currency::Regtest)
        .amount_milli_satoshis(sats * 1000)
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
        ark_funding: None,
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
        let params = Payload::encode(&ActionParameters::SignContract {
            scope: bad,
            ark_funding: None,
        })
        .unwrap();
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

mod ark_escrow {
    use super::*;
    use coordinator_ark_escrow::{
        EntryEscrow, EscrowPath, EscrowTerms, RefundSwap, RelativeTimelock, SwapTerms,
    };
    use coordinator_escrow::ark::{psbt_hex, ArkEscrowSpend, ArkFunding};
    use dlctix::bitcoin::{
        absolute::LockTime, taproot::LeafVersion, transaction::Version, Psbt, ScriptBuf,
        Transaction, TxIn, TxOut, Txid, XOnlyPublicKey,
    };

    const ESCROW_SATS: u64 = 20_000;
    /// The escrow's refund locktime, which a refund's transaction must wait for.
    const REFUND_AT: u32 = 1_790_000_000;
    /// A refunded escrow's value. The shared LNURL fixture answers one exact amount, so this
    /// leaves the player the 100,000 sats it expects to be asked for.
    const REFUNDED_SATS: u64 = 100_000 + MAX_REFUND_FEE_SATS;
    const MAX_FEE_SATS: u64 = 500;
    const MAX_REFUND_FEE_SATS: u64 = 100;

    fn xonly(secret: u8) -> XOnlyPublicKey {
        XOnlyPublicKey::from_slice(&public(secret)[1..]).unwrap()
    }
    /// The entry key 14's escrow, with the market maker 18 as its coordinator.
    fn escrow_with(player: u8, coordinator: u8) -> EntryEscrow {
        EntryEscrow::new(EscrowTerms {
            player: xonly(player),
            coordinator: xonly(coordinator),
            server: xonly(21),
            refund_locktime: LockTime::from_consensus(REFUND_AT),
            exit_delay: RelativeTimelock::Seconds(2048),
            unilateral_refund_delay: RelativeTimelock::Seconds(2048 + 512 * 100),
        })
        .unwrap()
    }
    fn policy_for(escrow: &EntryEscrow) -> ArkEscrowPolicy {
        ArkEscrowPolicy {
            escrow_tap_tree: hex::encode(escrow.vtxo_script().encode_tap_tree()),
            max_fee_sats: MAX_FEE_SATS,
            max_refund_fee_sats: MAX_REFUND_FEE_SATS,
        }
    }
    fn ark_fixture() -> (Fixture, EntryEscrow) {
        let escrow = escrow_with(14, 18);
        (fixture_with(false, Some(policy_for(&escrow))), escrow)
    }
    fn p2tr(secret: u8) -> ScriptBuf {
        ScriptBuf::new_p2tr_tweaked(
            dlctix::bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(xonly(secret)),
        )
    }
    fn spend_leaf(input: &mut dlctix::bitcoin::psbt::Input, escrow: &EntryEscrow) {
        input.tap_scripts.insert(
            escrow.control_block(EscrowPath::Funding),
            (
                escrow.script(EscrowPath::Funding).clone(),
                LeafVersion::TapScript,
            ),
        );
    }
    fn outpoint(byte: u8) -> OutPoint {
        OutPoint::new(Txid::from_byte_array([byte; 32]), 0)
    }
    /// A BIP322-style proof: the message input, this escrow, and another player's escrow.
    fn intent_proof(escrow: &EntryEscrow, outputs: Vec<TxOut>) -> Psbt {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: [1, 2, 3]
                .into_iter()
                .map(|byte| TxIn {
                    previous_output: outpoint(byte),
                    ..Default::default()
                })
                .collect(),
            output: outputs,
        })
        .unwrap();
        psbt.inputs[0].witness_utxo = Some(TxOut {
            value: Amount::ZERO,
            script_pubkey: escrow.script_pubkey(),
        });
        psbt.inputs[1].witness_utxo = Some(TxOut {
            value: Amount::from_sat(ESCROW_SATS),
            script_pubkey: escrow.script_pubkey(),
        });
        psbt.inputs[2].witness_utxo = Some(TxOut {
            value: Amount::from_sat(ESCROW_SATS),
            script_pubkey: p2tr(30),
        });
        spend_leaf(&mut psbt.inputs[0], escrow);
        spend_leaf(&mut psbt.inputs[1], escrow);
        psbt
    }
    fn funding_output(f: &Fixture) -> TxOut {
        f.contract.contract_parameters.funding_output().unwrap()
    }
    fn attempt() -> ActionAttempt {
        ActionAttempt {
            attempt_id: Uuid::now_v7(),
            signing_session_id: None,
        }
    }
    async fn prepare(
        verifier: &CoordinatorVerifier,
        f: &Fixture,
        bound: &Payload,
        spend: ArkEscrowSpend,
    ) -> Result<(PreparedAction, ActionAttempt), VerificationError> {
        let attempt = attempt();
        let prior = BTreeMap::new();
        let params = Payload::encode(&ActionParameters::SignArkEscrow { spend }).unwrap();
        let prepared = verifier
            .prepare(
                f.prepare_view(bound, &attempt, SIGN_ARK_ESCROW, &prior),
                &params,
            )
            .await?;
        Ok((prepared, attempt))
    }
    fn digests(action: &Action) -> Vec<[u8; 32]> {
        let Action::SignBip340 { scope } = action else {
            panic!("expected a BIP340 action")
        };
        scope.items.iter().map(|item| item.digest).collect()
    }

    /// The batch: a commitment paying the pool and a connector, the connector transaction, and a forfeit.
    struct Batch {
        commitment: Transaction,
        connector: Transaction,
        forfeit: Psbt,
    }
    fn new_batch(f: &Fixture, escrow: &EntryEscrow) -> Batch {
        let commitment = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: outpoint(9),
                ..Default::default()
            }],
            output: vec![
                funding_output(f),
                TxOut {
                    value: Amount::from_sat(330),
                    script_pubkey: p2tr(31),
                },
            ],
        };
        let connector = Transaction {
            version: Version::non_standard(3),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(commitment.compute_txid(), 1),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(330),
                script_pubkey: p2tr(32),
            }],
        };
        let mut forfeit = Psbt::from_unsigned_tx(Transaction {
            version: Version::non_standard(3),
            lock_time: LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: OutPoint::new(connector.compute_txid(), 0),
                    ..Default::default()
                },
                TxIn {
                    previous_output: outpoint(2),
                    ..Default::default()
                },
            ],
            output: vec![TxOut {
                value: Amount::from_sat(ESCROW_SATS + 330),
                script_pubkey: p2tr(33),
            }],
        })
        .unwrap();
        forfeit.inputs[0].witness_utxo = Some(connector.output[0].clone());
        forfeit.inputs[1].witness_utxo = Some(TxOut {
            value: Amount::from_sat(ESCROW_SATS),
            script_pubkey: escrow.script_pubkey(),
        });
        spend_leaf(&mut forfeit.inputs[1], escrow);
        Batch {
            commitment,
            connector,
            forfeit,
        }
    }
    fn funded(f: &Fixture, commitment: &Transaction) -> ContractCommitment {
        ContractCommitment {
            contract_parameters: f.contract.contract_parameters.clone(),
            funding_outpoint: OutPoint::new(commitment.compute_txid(), 0),
        }
    }
    fn forfeit_spend(batch: &Batch, signatures: &ContractSignatures) -> ArkEscrowSpend {
        ArkEscrowSpend::Forfeit {
            forfeit_psbt: psbt_hex(&batch.forfeit),
            funding: ArkFunding::new(&batch.commitment, 0),
            connector_txs: vec![dlctix::bitcoin::consensus::encode::serialize_hex(
                &batch.connector,
            )],
            contract_signatures: serde_json::to_string(signatures).unwrap(),
        }
    }

    #[test]
    fn the_escrow_must_belong_to_the_entry_key_and_the_market_maker() {
        let verifier = CoordinatorVerifier::default();
        for escrow in [escrow_with(15, 18), escrow_with(14, 19)] {
            let f = fixture_with(false, Some(policy_for(&escrow)));
            assert!(verifier
                .validate_registration(RegistrationView {
                    manifest: &f.manifest,
                    policy: &f.policy,
                    restoring: false,
                })
                .is_err());
        }
        let (f, _) = ark_fixture();
        verifier
            .validate_registration(RegistrationView {
                manifest: &f.manifest,
                policy: &f.policy,
                restoring: false,
            })
            .unwrap();
        assert!(f.policy.policy.grants.contains_key(SIGN_ARK_ESCROW));
        assert!(!fixture(false)
            .policy
            .policy
            .grants
            .contains_key(SIGN_ARK_ESCROW));
    }

    #[tokio::test]
    async fn an_intent_proof_signs_only_this_escrow_into_the_pool() {
        let verifier = CoordinatorVerifier::default();
        let (f, escrow) = ark_fixture();
        let bound = f.bind(&verifier);

        let fee = TxOut {
            value: Amount::from_sat(2 * MAX_FEE_SATS),
            script_pubkey: p2tr(40),
        };
        let proof = intent_proof(&escrow, vec![funding_output(&f), fee.clone()]);
        let spend = ArkEscrowSpend::IntentProof {
            proof_psbt: psbt_hex(&proof),
        };
        let (prepared, attempt) = prepare(&verifier, &f, &bound, spend).await.unwrap();
        // The message input and this escrow's input; never the other player's.
        let inputs: Vec<usize> = prepared.output.decode().unwrap();
        assert_eq!(inputs, vec![0, 1]);
        assert_eq!(digests(&prepared.action).len(), 2);
        verifier
            .verify_execution(
                f.execute_view(&bound, &attempt, SIGN_ARK_ESCROW),
                &prepared,
                &Payload::default(),
            )
            .await
            .unwrap();

        let over_fee = TxOut {
            value: Amount::from_sat(2 * MAX_FEE_SATS + 1),
            ..fee
        };
        let elsewhere = TxOut {
            script_pubkey: p2tr(41),
            ..funding_output(&f)
        };
        for outputs in [
            vec![funding_output(&f), over_fee],
            vec![elsewhere],
            vec![funding_output(&f), funding_output(&f)],
        ] {
            let spend = ArkEscrowSpend::IntentProof {
                proof_psbt: psbt_hex(&intent_proof(&escrow, outputs)),
            };
            assert!(prepare(&verifier, &f, &bound, spend).await.is_err());
        }

        // The escrow must be spent through its funding leaf.
        let mut refund_leaf = intent_proof(&escrow, vec![funding_output(&f)]);
        refund_leaf.inputs[1].tap_scripts.clear();
        refund_leaf.inputs[1].tap_scripts.insert(
            escrow.control_block(EscrowPath::Refund),
            (
                escrow.script(EscrowPath::Refund).clone(),
                LeafVersion::TapScript,
            ),
        );
        let spend = ArkEscrowSpend::IntentProof {
            proof_psbt: psbt_hex(&refund_leaf),
        };
        assert!(prepare(&verifier, &f, &bound, spend).await.is_err());
    }

    #[tokio::test]
    async fn a_forfeit_needs_the_signed_contract_and_a_connector_from_the_commitment() {
        let verifier = CoordinatorVerifier::default();
        let (f, escrow) = ark_fixture();
        let bound = f.bind(&verifier);
        let batch = new_batch(&f, &escrow);
        let signed = sign_contract(&funded(&f, &batch.commitment));

        let (prepared, attempt) = prepare(&verifier, &f, &bound, forfeit_spend(&batch, &signed))
            .await
            .unwrap();
        let inputs: Vec<usize> = prepared.output.decode().unwrap();
        assert_eq!(inputs, vec![1]);
        verifier
            .verify_execution(
                f.execute_view(&bound, &attempt, SIGN_ARK_ESCROW),
                &prepared,
                &Payload::default(),
            )
            .await
            .unwrap();

        // Signatures for another funding outpoint leave the players without a way out.
        let elsewhere = sign_contract(&f.contract);
        assert!(
            prepare(&verifier, &f, &bound, forfeit_spend(&batch, &elsewhere))
                .await
                .is_err()
        );
        // So does a set without the expiry transaction.
        let mut no_expiry = signed.clone();
        no_expiry.expiry_tx_signature = None;
        assert!(
            prepare(&verifier, &f, &bound, forfeit_spend(&batch, &no_expiry))
                .await
                .is_err()
        );

        // A connector that does not descend from the commitment makes the forfeit unconditional.
        let mut detached = new_batch(&f, &escrow);
        detached.connector.input[0].previous_output = outpoint(8);
        detached.forfeit.unsigned_tx.input[0].previous_output =
            OutPoint::new(detached.connector.compute_txid(), 0);
        assert!(
            prepare(&verifier, &f, &bound, forfeit_spend(&detached, &signed))
                .await
                .is_err()
        );

        // A commitment that does not pay the pool is refused.
        let mut unfunded = new_batch(&f, &escrow);
        unfunded.commitment.output[0].script_pubkey = p2tr(42);
        assert!(
            prepare(&verifier, &f, &bound, forfeit_spend(&unfunded, &signed))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn contract_signing_uses_the_batch_outpoint() {
        let verifier = CoordinatorVerifier::default();
        let (f, escrow) = ark_fixture();
        let bound = f.bind(&verifier);
        let batch = new_batch(&f, &escrow);
        let contract = funded(&f, &batch.commitment);
        let attempt = ActionAttempt {
            attempt_id: Uuid::now_v7(),
            signing_session_id: Some(SessionId::new_v7()),
        };
        let prior = BTreeMap::new();
        let sign = |ark_funding: Option<ArkFunding>, scope: SigningScope| {
            Payload::encode(&ActionParameters::SignContract { scope, ark_funding }).unwrap()
        };

        let params = sign(
            Some(ArkFunding::new(&batch.commitment, 0)),
            f.scope_for(&contract),
        );
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

        // Without the commitment, an Arkade-funded pool has no outpoint to sign against.
        let params = sign(None, f.scope());
        assert!(verifier
            .prepare(
                f.prepare_view(&bound, &attempt, SIGN_CONTRACT, &prior),
                &params
            )
            .await
            .is_err());
        // A commitment that does not pay the pool is refused.
        let mut unfunded = batch.commitment.clone();
        unfunded.output[0].script_pubkey = p2tr(42);
        let params = sign(Some(ArkFunding::new(&unfunded, 0)), f.scope_for(&contract));
        assert!(verifier
            .prepare(
                f.prepare_view(&bound, &attempt, SIGN_CONTRACT, &prior),
                &params
            )
            .await
            .is_err());
    }

    /// A refund of `escrow` into `swap`, spending at the escrow's locktime.
    fn refund_tx(escrow: &EntryEscrow, swap: &RefundSwap, value_sats: u64) -> Psbt {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            version: Version::TWO,
            lock_time: LockTime::from_consensus(REFUND_AT),
            input: vec![TxIn {
                previous_output: outpoint(11),
                sequence: dlctix::bitcoin::Sequence::ENABLE_LOCKTIME_NO_RBF,
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(value_sats),
                script_pubkey: swap.script_pubkey(),
            }],
        })
        .unwrap();
        psbt.inputs[0].witness_utxo = Some(TxOut {
            value: Amount::from_sat(value_sats),
            script_pubkey: escrow.script_pubkey(),
        });
        psbt.inputs[0].tap_scripts.insert(
            escrow.control_block(EscrowPath::Refund),
            (
                escrow.script(EscrowPath::Refund).clone(),
                LeafVersion::TapScript,
            ),
        );
        psbt
    }

    /// The swap a refund pays, committing to the invoice `preimage` settles.
    fn refund_swap(preimage: [u8; 32]) -> RefundSwap {
        let deadline = LockTime::from_consensus(now().unwrap() as u32 + 3_600);
        let exit_delay = RelativeTimelock::Seconds(2048);
        RefundSwap::new(SwapTerms {
            player: xonly(14),
            swapper: xonly(30),
            server: xonly(21),
            payment_hash: payout::sha256(&preimage),
            deadline,
            exit_delay,
            unilateral_reclaim_delay: RelativeTimelock::Seconds(2048 + 512 * 100),
        })
        .unwrap()
    }

    fn refund_of(escrow: &EntryEscrow, swap: &RefundSwap) -> ArkEscrowSpend {
        ArkEscrowSpend::Refund {
            refund_psbt: psbt_hex(&refund_tx(escrow, swap, REFUNDED_SATS)),
            swap_tap_tree: hex::encode(swap.vtxo_script().encode_tap_tree()),
        }
    }

    fn refund_parameters(spend: ArkEscrowSpend, invoice: String, fee_sats: u64) -> Payload {
        Payload::encode(&ActionParameters::RefundArkEscrow {
            spend,
            invoice,
            fee_sats,
        })
        .unwrap()
    }

    #[cfg(feature = "lnurl")]
    #[tokio::test]
    async fn a_refund_pays_the_players_own_address_through_a_swap_committed_to_its_invoice() {
        use crate::lnurl_transport::tests::{discovery_tls_fixture, FIXTURE_METADATA};

        let escrow = escrow_with(14, 18);
        let f = fixture_with(true, Some(policy_for(&escrow)));
        let preimage = [9u8; 32];
        let paid_sats = REFUNDED_SATS - MAX_REFUND_FEE_SATS;
        let invoice = address_invoice_for(paid_sats, preimage, FIXTURE_METADATA);
        let (client, server) = discovery_tls_fixture().await;
        let verifier = CoordinatorVerifier::with_lnurl(client);
        let swap = refund_swap(preimage);
        let attempt = attempt();
        let unbound = Payload::default();
        let prepared = verifier
            .prepare(
                f.prepare_view(&unbound, &attempt, SIGN_ARK_REFUND, &BTreeMap::new()),
                &refund_parameters(
                    refund_of(&escrow, &swap),
                    invoice.clone(),
                    MAX_REFUND_FEE_SATS,
                ),
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(digests(&prepared.action).len(), 1);

        // Executing and restoring recheck the refund without resolving the address again.
        verifier
            .verify_execution(
                f.execute_view(&unbound, &attempt, SIGN_ARK_REFUND),
                &prepared,
                &Payload::default(),
            )
            .await
            .unwrap();
        verifier
            .restore_execution(f.execute_view(&unbound, &attempt, SIGN_ARK_REFUND), &prepared)
            .await
            .unwrap();
    }

    #[cfg(feature = "lnurl")]
    #[tokio::test]
    async fn a_refund_is_refused_unless_the_invoice_and_swap_are_the_players() {
        use crate::lnurl_transport::tests::{discovery_tls_fixture, FIXTURE_METADATA};

        let escrow = escrow_with(14, 18);
        let f = fixture_with(true, Some(policy_for(&escrow)));
        let preimage = [9u8; 32];
        let paid_sats = REFUNDED_SATS - MAX_REFUND_FEE_SATS;
        let attempt = attempt();
        let unbound = Payload::default();
        let refund = |swap: &RefundSwap, invoice: String, fee: u64| {
            refund_parameters(refund_of(&escrow, swap), invoice, fee)
        };
        let prepare = |verifier: CoordinatorVerifier, params: Payload| {
            let f = &f;
            let attempt = &attempt;
            let unbound = &unbound;
            async move {
                verifier
                    .prepare(
                        f.prepare_view(unbound, attempt, SIGN_ARK_REFUND, &BTreeMap::new()),
                        &params,
                    )
                    .await
            }
        };

        // An invoice from another provider, or for another address, commits to other metadata.
        let (client, server) = discovery_tls_fixture().await;
        let params = refund(
            &refund_swap(preimage),
            address_invoice_for(paid_sats, preimage, "[[\"text/plain\",\"Someone else\"]]"),
            MAX_REFUND_FEE_SATS,
        );
        assert!(prepare(CoordinatorVerifier::with_lnurl(client), params)
            .await
            .is_err());
        server.await.unwrap();

        // A swap that commits to a different invoice cannot be claimed by paying this one.
        let (client, server) = discovery_tls_fixture().await;
        let params = refund(
            &refund_swap([1u8; 32]),
            address_invoice_for(paid_sats, preimage, FIXTURE_METADATA),
            MAX_REFUND_FEE_SATS,
        );
        assert!(prepare(CoordinatorVerifier::with_lnurl(client), params)
            .await
            .is_err());
        server.await.unwrap();

        // A fee above the player's cap is refused before the address is resolved at all.
        let (client, server) = discovery_tls_fixture().await;
        let params = refund(
            &refund_swap(preimage),
            address_invoice_for(paid_sats, preimage, FIXTURE_METADATA),
            MAX_REFUND_FEE_SATS + 1,
        );
        assert!(prepare(CoordinatorVerifier::with_lnurl(client), params)
            .await
            .is_err());
        server.abort();
    }

    #[tokio::test]
    async fn only_the_refund_permission_signs_a_refund() {
        let escrow = escrow_with(14, 18);
        let f = fixture_with(true, Some(policy_for(&escrow)));
        let verifier = CoordinatorVerifier::default();
        let attempt = attempt();
        let unbound = Payload::default();
        // The escrow spending permission may not sign a refund, and the refund permission may
        // not sign a batch.
        assert!(verifier
            .prepare(
                f.prepare_view(&unbound, &attempt, SIGN_ARK_ESCROW, &BTreeMap::new()),
                &refund_parameters(
                    refund_of(&escrow, &refund_swap([9u8; 32])),
                    invoice_for(1_000, [9u8; 32]),
                    0
                ),
            )
            .await
            .is_err());
        assert!(verifier
            .prepare(
                f.prepare_view(&unbound, &attempt, SIGN_ARK_REFUND, &BTreeMap::new()),
                &Payload::encode(&ActionParameters::SignArkEscrow {
                    spend: ArkEscrowSpend::IntentProof {
                        proof_psbt: String::new()
                    }
                })
                .unwrap(),
            )
            .await
            .is_err());
    }
}
