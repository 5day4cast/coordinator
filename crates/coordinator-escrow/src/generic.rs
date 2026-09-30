//! Typed application data carried only inside Keymeld's confidential payloads.
use crate::{
    ark::{ArkEscrowSpend, ArkFunding},
    authorization::PayoutPolicy,
    oracle_statement::SignedStatement,
    payout::ContractCommitment,
    payout_protocol::PayoutMethod,
};
use keymeld_core::{
    escrow::{
        self, protocol::Payload, ActionGrant, ApplicationContext, Condition, EscrowContext,
        EscrowPolicy, EscrowRegistration, Permission, PublicKeyBytes, Recipient, SecretCommitment,
        SignedEscrowPolicy, SigningScope, VerifierPolicy,
    },
    KeyMeldError,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const VERIFIER_ID: &str = "coordinator.dlc";
pub const VERIFIER_VERSION: u16 = 1;
pub const SIGN_CONTRACT: &str = "sign_contract";
pub const RELEASE_PREIMAGE: &str = "release_preimage";
pub const RELEASE_ENTRY_KEY: &str = "release_entry_key";
pub const PREIMAGE_SECRET: &str = "entry_preimage";
pub const CONTRACT_RULE: &str = "contract_signing_allowed";
pub const SETTLEMENT_RULE: &str = "settlement_completed";
/// Signs spends of the entry's Arkade escrow VTXO, when the ticket has one.
pub const SIGN_ARK_ESCROW: &str = "sign_ark_escrow";
pub const ARK_ESCROW_RULE: &str = "ark_escrow_spend_allowed";
/// Signs the refund of an escrow whose competition never kicked off, offchain or, once the
/// escrow's VTXO has expired, as the intent of a batch; and the delete proof that frees the
/// escrow from a batch intent left queued. Unbound, because a pool that never filled has no
/// contract and may have no completed keygen session.
pub const SIGN_ARK_REFUND: &str = "sign_ark_refund";
pub const ARK_REFUND_RULE: &str = "ark_refund_allowed";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractBinding {
    pub contract: ContractCommitment,
    /// For a pool of a queued competition: the oracle's signed statement of the pool's event.
    /// The verifier derives every member's contract terms from it. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement: Option<SignedStatement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionParameters {
    /// IDs and routing subsets are client-selected. The verifier reconstructs
    /// the exact contract messages, signing keys, tweaks and adaptor points.
    SignContract {
        scope: SigningScope,
        /// For an Arkade-funded pool: the commitment transaction that fixes the funding outpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ark_funding: Option<ArkFunding>,
    },
    /// [`ActionParameters::SignContract`] naming each message by its digest alone. The verifier
    /// expands it to the full scope, deriving what it would otherwise check: the signers from
    /// the bound roster and the manifest's subsets, and the adaptor points from the contract.
    /// A verifier older than this variant refuses it; see [`refuses_compact_scope`].
    SignContractCompact {
        items: Vec<ContractItem>,
        /// For an Arkade-funded pool: the commitment transaction that fixes the funding outpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ark_funding: Option<ArkFunding>,
    },
    PrepareSettlement {
        claim_id: Uuid,
        contract_signatures: String,
        /// `None` settles on the expiry outcome; see [`crate::payout::settled_outcome`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<String>,
        method: PayoutMethod,
        /// For an Arkade-funded pool: the commitment transaction that fixes the funding outpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ark_funding: Option<ArkFunding>,
    },
    /// Sign the entry's Arkade escrow spend. See [`crate::ark`].
    SignArkEscrow { spend: ArkEscrowSpend },
    /// Sign the refund of an escrow whose competition never kicked off; `spend` is an
    /// [`ArkEscrowSpend::Refund`] or an [`ArkEscrowSpend::RefundIntent`]. See [`crate::ark`].
    ///
    /// The invoice is supplied rather than requested by the verifier, because the swap the
    /// refund pays must already commit to its payment hash. The verifier checks it was issued
    /// for the player's own Lightning Address before signing anything.
    RefundArkEscrow {
        spend: ArkEscrowSpend,
        /// The invoice the swap service pays, from the player's Lightning Address.
        invoice: String,
        /// What the swap service keeps, capped by the player's consented policy.
        fee_sats: u64,
    },
    /// Sign a proof deleting a queued batch intent that holds the entry's escrow; `spend` is an
    /// [`ArkEscrowSpend::DeleteIntent`]. See [`crate::ark`].
    ///
    /// Granted by the refund permission and unbound like a refund, since it is what lets a refund
    /// through when a kickoff that never finished left its intent queued. It moves nothing.
    DeleteArkIntent { spend: ArkEscrowSpend },
}

/// One message of a [`ActionParameters::SignContractCompact`] scope: the client-selected
/// routing ids of a [`SigningItem`](escrow::SigningItem), and the digest naming its message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractItem {
    pub item_id: Uuid,
    #[serde(with = "hex_digest")]
    pub message_digest: [u8; 32],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subset_id: Option<Uuid>,
    /// Set exactly when the message is signed with an adaptor point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptor_id: Option<Uuid>,
}
impl ContractItem {
    /// The compact form of a contract signing scope. It keeps everything the verifier cannot
    /// derive, so the verifier expands it back to exactly `scope`.
    pub fn compact(scope: &SigningScope) -> Vec<Self> {
        scope
            .batch
            .iter()
            .map(|item| Self {
                item_id: item.item_id,
                message_digest: item.message_digest,
                subset_id: item.subset_id,
                adaptor_id: match &item.adaptor {
                    escrow::AdaptorContext::None => None,
                    escrow::AdaptorContext::Single { adaptor_id, .. } => Some(*adaptor_id),
                },
            })
            .collect()
    }
}
/// Whether a refused contract signing preparation came from a verifier that predates
/// [`ActionParameters::SignContractCompact`]. Such a verifier accepts the full form instead.
pub fn refuses_compact_scope(error: &str) -> bool {
    error.contains("unknown variant `sign_contract_compact`")
}
mod hex_digest {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(value))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        let value = String::deserialize(deserializer)?;
        let mut digest = [0; 32];
        hex::decode_to_slice(value, &mut digest).map_err(serde::de::Error::custom)?;
        Ok(digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSettlement {
    pub claim_id: Uuid,
    pub contract_digest: String,
    pub invoice: String,
    pub invoice_digest: String,
    pub payment_hash: String,
    pub owed_sats: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentEvidence {
    pub payment_preimage: [u8; 32],
}
impl Drop for PaymentEvidence {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.payment_preimage.zeroize();
    }
}
impl std::fmt::Debug for PaymentEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PaymentEvidence([REDACTED])")
    }
}

/// Creates new participant consent in the generic v2 signature domain. Existing
/// draft payout signatures must not be reinterpreted as this authorization.
pub fn registration(
    context: EscrowContext,
    participant_secret: &[u8; 32],
    policy: PayoutPolicy,
    payout_preimage: &[u8; 32],
    recipient: Recipient,
) -> Result<EscrowRegistration, KeyMeldError> {
    let terms = crate::queued::EntryConsent::from_policy(&policy)
        .map_err(|error| KeyMeldError::ValidationError(error.to_string()))?;
    terms
        .verify_preimage(payout_preimage)
        .map_err(|error| KeyMeldError::ValidationError(error.to_string()))?;
    let key = secp256k1::PublicKey::from_secret_key(
        &secp256k1::Secp256k1::new(),
        &secp256k1::SecretKey::from_byte_array(*participant_secret)
            .map_err(KeyMeldError::InvalidKey)?,
    );
    let participant_public_key = PublicKeyBytes::new(&key.serialize())?;
    let policy = participant_policy(context, participant_public_key, policy, recipient)?;
    Ok(EscrowRegistration {
        policy: SignedEscrowPolicy::sign(policy, participant_secret)?,
        secrets: BTreeMap::from([(PREIMAGE_SECRET.into(), payout_preimage.to_vec())]),
    })
}

/// Reconstruct the exact policy that a wallet must sign. The Coordinator uses
/// this to compare fresh consent against the accepted ticket before enrollment.
pub fn participant_policy(
    mut context: EscrowContext,
    participant_public_key: PublicKeyBytes,
    policy: PayoutPolicy,
    recipient: Recipient,
) -> Result<EscrowPolicy, KeyMeldError> {
    let terms = crate::queued::EntryConsent::from_policy(&policy)
        .map_err(|error| KeyMeldError::ValidationError(error.to_string()))?;
    let ark_escrow = policy.ark_escrow.is_some();
    let policy_data = Payload::encode(&policy)?;
    context.application =
        ApplicationContext::commit(VERIFIER_ID.into(), VERIFIER_VERSION, policy_data.as_bytes())?;
    let grants = BTreeMap::from([
        (
            SIGN_CONTRACT.into(),
            ActionGrant {
                preparation: escrow::PreparationPolicy::Single,
                // An Arkade pool signs again, for a new funding outpoint, when its batch is retried.
                repetition: if ark_escrow {
                    escrow::Repetition::VerifierAuthorizedAttempts
                } else {
                    escrow::Repetition::RepeatIdenticalSigningScope
                },
                unbound: false,
                condition: Condition::VerifierRule {
                    rule: CONTRACT_RULE.into(),
                },
                operation: Permission::Sign,
            },
        ),
        (
            RELEASE_PREIMAGE.into(),
            ActionGrant {
                preparation: escrow::PreparationPolicy::RenewableIdenticalAction,
                repetition: escrow::Repetition::Once,
                unbound: false,
                condition: Condition::VerifierRule {
                    rule: SETTLEMENT_RULE.into(),
                },
                operation: Permission::ReleaseSecret {
                    name: PREIMAGE_SECRET.into(),
                    recipient: recipient.clone(),
                },
            },
        ),
        (
            RELEASE_ENTRY_KEY.into(),
            ActionGrant {
                preparation: escrow::PreparationPolicy::RenewableIdenticalAction,
                repetition: escrow::Repetition::Once,
                unbound: false,
                condition: Condition::VerifierRule {
                    rule: SETTLEMENT_RULE.into(),
                },
                operation: Permission::ReleaseSigningKey {
                    public_key: participant_public_key.clone(),
                    recipient,
                },
            },
        ),
    ]);
    let mut grants = grants;
    if ark_escrow {
        grants.insert(
            SIGN_ARK_ESCROW.into(),
            ActionGrant {
                preparation: escrow::PreparationPolicy::Single,
                // Every batch attempt signs new transactions, and the verifier checks each one.
                repetition: escrow::Repetition::VerifierAuthorizedAttempts,
                unbound: false,
                condition: Condition::VerifierRule {
                    rule: ARK_ESCROW_RULE.into(),
                },
                operation: Permission::SignBip340,
            },
        );
        grants.insert(
            SIGN_ARK_REFUND.into(),
            ActionGrant {
                preparation: escrow::PreparationPolicy::Single,
                // A refund is retried with a fresh invoice and swap, which the verifier checks.
                repetition: escrow::Repetition::VerifierAuthorizedAttempts,
                // The pool never formed, so there is no binding to act under.
                unbound: true,
                condition: Condition::VerifierRule {
                    rule: ARK_REFUND_RULE.into(),
                },
                operation: Permission::SignBip340,
            },
        );
    }
    let policy = EscrowPolicy {
        schema_version: escrow::SCHEMA_VERSION,
        context,
        participant_public_key,
        verifier: Some(VerifierPolicy {
            id: VERIFIER_ID.into(),
            version: VERIFIER_VERSION,
            policy_data,
        }),
        secrets: BTreeMap::from([(
            PREIMAGE_SECRET.into(),
            SecretCommitment {
                sha256: terms.payout_hash(),
                length: 32,
            },
        )]),
        grants,
    };
    policy.validate()?;
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keymeld_core::{
        escrow::{
            protocol::PrepareEscrowRequest, ActionAttempt, AdaptorContext, KeyTweak, ScopeSigner,
            SigningItem, SCHEMA_VERSION,
        },
        SessionId, UserId,
    };

    /// One player's contract signing scope in a pool of `players`, shaped like a queued pool's:
    /// the outcome transactions, the player's own win split, and a refund and an expiry split
    /// for every player, each signed by every player and the market maker.
    fn pool_scope(players: usize) -> SigningScope {
        let signers = {
            let mut signers = (0..=players)
                .map(|index| ScopeSigner {
                    user_id: UserId::from(Uuid::from_u128(index as u128 + 1)),
                    public_key: key(index as u8 + 1),
                })
                .collect::<Vec<_>>();
            signers.sort_by(|a, b| a.public_key.cmp(&b.public_key));
            signers
        };
        let outcomes = players + 2;
        SigningScope {
            session_tweak: KeyTweak::None,
            batch: (0..outcomes + 1 + 2 * players)
                .map(|index| SigningItem {
                    item_id: Uuid::now_v7(),
                    message_digest: [index as u8; 32],
                    subset_id: (index >= outcomes).then(Uuid::now_v7),
                    signers: signers.clone(),
                    tweak: KeyTweak::None,
                    adaptor: if index + 1 < outcomes {
                        AdaptorContext::Single {
                            adaptor_id: Uuid::now_v7(),
                            point: key(200),
                        }
                    } else {
                        AdaptorContext::None
                    },
                })
                .collect(),
        }
    }
    fn key(secret: u8) -> PublicKeyBytes {
        let secret = secp256k1::SecretKey::from_byte_array([secret; 32]).unwrap();
        PublicKeyBytes::new(&secret.public_key(&secp256k1::Secp256k1::new()).serialize()).unwrap()
    }
    fn prepare_request(parameters: &ActionParameters) -> Vec<u8> {
        serde_json::to_vec(&PrepareEscrowRequest {
            schema_version: SCHEMA_VERSION,
            binding_receipt: Payload::default(),
            action_id: SIGN_CONTRACT.into(),
            attempt: ActionAttempt {
                attempt_id: Uuid::now_v7(),
                signing_session_id: Some(SessionId::new_v7()),
            },
            action: None,
            action_parameters: Payload::encode(parameters).unwrap(),
            prior_preparation_receipts: vec![],
        })
        .unwrap()
    }

    #[test]
    fn a_compact_scope_keeps_what_the_verifier_cannot_derive_in_a_fraction_of_the_bytes() {
        for players in [22, 25] {
            let scope = pool_scope(players);
            assert_eq!(scope.batch.len(), 3 * players + 3);
            let items = ContractItem::compact(&scope);
            for (item, compact) in scope.batch.iter().zip(&items) {
                assert_eq!(compact.item_id, item.item_id);
                assert_eq!(compact.message_digest, item.message_digest);
                assert_eq!(compact.subset_id, item.subset_id);
                assert_eq!(
                    compact.adaptor_id.is_some(),
                    matches!(item.adaptor, AdaptorContext::Single { .. })
                );
            }
            let compact = ActionParameters::SignContractCompact {
                items,
                ark_funding: None,
            };
            let decoded: ActionParameters = Payload::encode(&compact).unwrap().decode().unwrap();
            assert!(
                matches!(decoded, ActionParameters::SignContractCompact { items, .. } if items == ContractItem::compact(&scope))
            );
            let full = ActionParameters::SignContract {
                scope,
                ark_funding: None,
            };
            let (full_parameters, compact_parameters) = (
                Payload::encode(&full).unwrap().as_bytes().len(),
                Payload::encode(&compact).unwrap().as_bytes().len(),
            );
            let (full_request, compact_request) = (
                prepare_request(&full).len(),
                prepare_request(&compact).len(),
            );
            println!(
                "{players} players: parameters {full_parameters} -> {compact_parameters} bytes, \
                 prepare request {full_request} -> {compact_request} bytes"
            );
            assert!(compact_request * 10 < full_request);
        }
    }

    /// A verifier that predates the compact scope, as it decodes the parameters.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    #[allow(dead_code)]
    enum OlderParameters {
        SignContract {
            scope: SigningScope,
            #[serde(default)]
            ark_funding: Option<ArkFunding>,
        },
        SignArkEscrow {
            spend: ArkEscrowSpend,
        },
    }

    #[test]
    fn only_an_older_verifiers_refusal_of_the_compact_scope_is_recognised() {
        let scope = pool_scope(3);
        let compact = Payload::encode(&ActionParameters::SignContractCompact {
            items: ContractItem::compact(&scope),
            ark_funding: None,
        })
        .unwrap();
        let refusal = compact.decode::<OlderParameters>().unwrap_err();
        // How the refusal reaches the coordinator, through the enclave's error.
        let relayed = format!("Enclave rejected the confidential operation: {refusal}");
        assert!(refuses_compact_scope(&relayed), "{relayed}");

        let full = Payload::encode(&ActionParameters::SignContract {
            scope,
            ark_funding: None,
        })
        .unwrap();
        assert!(full.decode::<OlderParameters>().is_ok());
        assert!(!refuses_compact_scope(
            "Signing keys or tweak differ from the authorized DLC"
        ));
    }
}
