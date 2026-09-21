//! Typed application data carried only inside Keymeld's confidential payloads.
use crate::{
    ark::{ArkEscrowSpend, ArkFunding},
    authorization::PayoutPolicy,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractBinding {
    pub contract: ContractCommitment,
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
    PrepareSettlement {
        claim_id: Uuid,
        contract_signatures: String,
        attestation: String,
        method: PayoutMethod,
        /// For an Arkade-funded pool: the commitment transaction that fixes the funding outpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ark_funding: Option<ArkFunding>,
    },
    /// Sign the entry's Arkade escrow spend. See [`crate::ark`].
    SignArkEscrow { spend: ArkEscrowSpend },
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
    let terms = crate::payout::ContractAuthorization::from_policy(&policy)
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
    let terms = crate::payout::ContractAuthorization::from_policy(&policy)
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
                condition: Condition::VerifierRule {
                    rule: ARK_ESCROW_RULE.into(),
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
                sha256: terms.payout_hash,
                length: 32,
            },
        )]),
        grants,
    };
    policy.validate()?;
    Ok(policy)
}
