//! Coordinator application payloads carried inside the confidential generic escrow protocol.
use crate::{
    authorization::{sign_authorization, verify_authorization},
    KeyMeldError, SessionId, UserId,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvoiceAuthorizationContext {
    pub keygen_session_id: SessionId,
    pub user_id: UserId,
    pub claim_id: Uuid,
    pub competition_id: Uuid,
    pub entry_id: Uuid,
    pub contract_digest: String,
    pub invoice_digest: String,
    pub amount_msat: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedInvoiceAuthorization {
    pub context: InvoiceAuthorizationContext,
    pub signature: Vec<u8>,
}

impl SignedInvoiceAuthorization {
    pub fn sign(
        secret: &[u8; 32],
        context: InvoiceAuthorizationContext,
    ) -> Result<Self, KeyMeldError> {
        let signature = sign_authorization(secret, "payout-invoice-v1", &context)?;
        Ok(Self { context, signature })
    }
    pub fn verify(
        &self,
        public_key: &[u8],
        expected: &InvoiceAuthorizationContext,
        now: u64,
    ) -> Result<(), KeyMeldError> {
        if &self.context != expected || self.context.expires_at < now {
            return Err(KeyMeldError::ValidationError(
                "Invoice authorization context differs or has expired".into(),
            ));
        }
        verify_authorization(
            public_key,
            "payout-invoice-v1",
            &self.context,
            &self.signature,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PayoutMethod {
    Automatic,
    Invoice {
        invoice: String,
        authorization: SignedInvoiceAuthorization,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparePayoutRequest {
    pub claim_id: Uuid,
    pub binding_receipt: String,
    /// Serialized dlctix::ContractSignatures, never a caller's signing receipt.
    pub contract_signatures: String,
    pub attestation: String,
    pub method: PayoutMethod,
    /// The batch that funded an Arkade pool, whose contract is bound without a funding outpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ark_funding: Option<crate::ark::ArkFunding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleasePayoutRequest {
    pub claim_id: Uuid,
    pub state_receipt: String,
    pub payment_preimage: String,
}

/// Application view over an enclave-signed generic binding, for one participant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutContractBoundResponse {
    pub keygen_session_id: SessionId,
    pub enclave_id: crate::EnclaveId,
    pub enclave_key_epoch: u64,
    pub user_id: UserId,
    pub contract_digest: String,
    pub policy_set_digest: String,
    pub binding_receipt: String,
    pub binding: crate::generic::ContractBinding,
    pub response: keymeld_core::escrow::protocol::EscrowResponse,
}
fn invalid(message: impl Into<String>) -> KeyMeldError {
    KeyMeldError::ValidationError(message.into())
}
pub fn accepted_policy_set_digest(
    policies: &std::collections::BTreeMap<UserId, [u8; 32]>,
) -> Result<String, KeyMeldError> {
    crate::authorization::authorization_digest("coordinator-escrow-policy-set-v1", policies)
        .map(hex::encode)
}
impl PayoutContractBoundResponse {
    pub fn from_response(
        binding: crate::generic::ContractBinding,
        response: keymeld_core::escrow::protocol::EscrowResponse,
    ) -> Result<Self, KeyMeldError> {
        use keymeld_core::escrow::{
            self,
            protocol::{BindingOutput, Operation, Payload},
        };
        let output: BindingOutput = response.output.decode()?;
        if response.context.request.operation != Operation::Bind
            || response.context.request.escrow.application.namespace != crate::generic::VERIFIER_ID
            || response.context.request.escrow.application.version
                != crate::generic::VERIFIER_VERSION
            || output
                .participant_policy_digests
                .get(&response.context.request.escrow.user_id)
                != Some(&response.context.request.policy_digest)
            || output.binding_data_digest != escrow::sha256(Payload::encode(&binding)?.as_bytes())
        {
            return Err(invalid(
                "Generic binding response commits to different application data",
            ));
        }
        Ok(Self {
            keygen_session_id: response.context.request.escrow.keygen_session_id.clone(),
            enclave_id: response.context.enclave_id,
            enclave_key_epoch: response.context.enclave_key_epoch,
            user_id: response.context.request.escrow.user_id.clone(),
            contract_digest: crate::payout::contract_digest(&binding.contract)
                .map_err(|e| invalid(e.to_string()))?,
            policy_set_digest: accepted_policy_set_digest(&output.participant_policy_digests)?,
            binding_receipt: hex::encode(response.sealed_state.as_bytes()),
            binding,
            response,
        })
    }
    pub fn verify(&self, public_key: &[u8]) -> Result<(), KeyMeldError> {
        self.response.verify(&self.response.context, public_key)?;
        let expected = Self::from_response(self.binding.clone(), self.response.clone())?;
        if self.keygen_session_id != expected.keygen_session_id
            || self.enclave_id != expected.enclave_id
            || self.enclave_key_epoch != expected.enclave_key_epoch
            || self.user_id != expected.user_id
            || self.contract_digest != expected.contract_digest
            || self.policy_set_digest != expected.policy_set_digest
            || self.binding_receipt != expected.binding_receipt
        {
            return Err(invalid(
                "Application binding fields differ from the signed generic response",
            ));
        }
        Ok(())
    }
}

/// Both explicit release grants share the exact prepared invoice. These generic
/// signed responses also carry the sealed states needed for restart recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedPayoutReceipts {
    pub preimage_preparation: keymeld_core::escrow::protocol::EscrowResponse,
    pub key_preparation: keymeld_core::escrow::protocol::EscrowResponse,
}
impl PreparedPayoutReceipts {
    pub fn encode(&self) -> Result<String, KeyMeldError> {
        serde_json::to_string(self).map_err(|e| KeyMeldError::SerializationError(e.to_string()))
    }
    pub fn decode(value: &str) -> Result<Self, KeyMeldError> {
        if value.len() > keymeld_core::escrow::MAX_WIRE_BYTES * 2 {
            return Err(invalid("Prepared settlement receipt exceeds size limit"));
        }
        serde_json::from_str(value).map_err(|e| KeyMeldError::SerializationError(e.to_string()))
    }
    pub fn verify(&self, public_key: &[u8]) -> Result<(), KeyMeldError> {
        self.preimage_preparation
            .verify(&self.preimage_preparation.context, public_key)?;
        self.key_preparation
            .verify(&self.key_preparation.context, public_key)?;
        self.settlement()?;
        Ok(())
    }
    pub fn settlement(&self) -> Result<crate::generic::PreparedSettlement, KeyMeldError> {
        use keymeld_core::escrow::protocol::Operation;
        let first = &self.preimage_preparation;
        let second = &self.key_preparation;
        let a = &first.context.request;
        let b = &second.context.request;
        if a.operation != Operation::Prepare
            || b.operation != Operation::Prepare
            || a.escrow.application.namespace != crate::generic::VERIFIER_ID
            || a.escrow.application.version != crate::generic::VERIFIER_VERSION
            || a.action_id.as_deref() != Some(crate::generic::RELEASE_PREIMAGE)
            || b.action_id.as_deref() != Some(crate::generic::RELEASE_ENTRY_KEY)
            || a.escrow != b.escrow
            || a.policy_digest != b.policy_digest
            || a.attempt != b.attempt
            || first.context.enclave_id != second.context.enclave_id
            || first.context.enclave_key_epoch != second.context.enclave_key_epoch
            || first.output != second.output
        {
            return Err(invalid(
                "Release preparations belong to different policies, attempts or invoices",
            ));
        }
        let output: crate::generic::PreparedSettlement = first.output.decode()?;
        let attempt = a
            .attempt
            .as_ref()
            .ok_or_else(|| invalid("Missing settlement attempt"))?;
        if attempt.attempt_id != output.claim_id
            || attempt.signing_session_id.is_some()
            || output.invoice_digest != crate::payout::invoice_digest(&output.invoice)
        {
            return Err(invalid(
                "Prepared settlement identity or invoice digest differs",
            ));
        }
        Ok(output)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutPreparedResponse {
    pub keygen_session_id: SessionId,
    pub enclave_id: crate::EnclaveId,
    pub user_id: UserId,
    pub claim_id: Uuid,
    pub invoice: String,
    pub invoice_digest: String,
    pub payment_hash: String,
    pub owed_sats: u64,
    pub state_receipt: String,
    pub preimage_preparation: keymeld_core::escrow::protocol::EscrowResponse,
    pub key_preparation: keymeld_core::escrow::protocol::EscrowResponse,
}
impl PayoutPreparedResponse {
    pub fn from_responses(
        preimage_preparation: keymeld_core::escrow::protocol::EscrowResponse,
        key_preparation: keymeld_core::escrow::protocol::EscrowResponse,
    ) -> Result<Self, KeyMeldError> {
        let receipts = PreparedPayoutReceipts {
            preimage_preparation,
            key_preparation,
        };
        let output = receipts.settlement()?;
        Ok(Self {
            keygen_session_id: receipts
                .preimage_preparation
                .context
                .request
                .escrow
                .keygen_session_id
                .clone(),
            enclave_id: receipts.preimage_preparation.context.enclave_id,
            user_id: receipts
                .preimage_preparation
                .context
                .request
                .escrow
                .user_id
                .clone(),
            claim_id: output.claim_id,
            invoice: output.invoice,
            invoice_digest: output.invoice_digest,
            payment_hash: output.payment_hash,
            owed_sats: output.owed_sats,
            state_receipt: receipts.encode()?,
            preimage_preparation: receipts.preimage_preparation,
            key_preparation: receipts.key_preparation,
        })
    }
    pub fn verify(&self, public_key: &[u8]) -> Result<(), KeyMeldError> {
        let receipts = PreparedPayoutReceipts {
            preimage_preparation: self.preimage_preparation.clone(),
            key_preparation: self.key_preparation.clone(),
        };
        receipts.verify(public_key)?;
        let expected =
            Self::from_responses(receipts.preimage_preparation, receipts.key_preparation)?;
        if self.keygen_session_id != expected.keygen_session_id
            || self.enclave_id != expected.enclave_id
            || self.user_id != expected.user_id
            || self.claim_id != expected.claim_id
            || self.invoice != expected.invoice
            || self.invoice_digest != expected.invoice_digest
            || self.payment_hash != expected.payment_hash
            || self.owed_sats != expected.owed_sats
            || self.state_receipt != expected.state_receipt
        {
            return Err(invalid(
                "Application settlement fields differ from signed generic preparations",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayoutReleasedResponse {
    pub keygen_session_id: SessionId,
    pub user_id: UserId,
    pub claim_id: Uuid,
    pub encrypted_secrets: String,
}

#[derive(Serialize, Deserialize)]
pub struct PayoutSecrets {
    pub entry_private_key: String,
    pub payout_preimage: String,
}
impl Drop for PayoutSecrets {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.entry_private_key.zeroize();
        self.payout_preimage.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn public(secret: &[u8; 32]) -> Vec<u8> {
        secp256k1::PublicKey::from_secret_key(
            &secp256k1::Secp256k1::new(),
            &secp256k1::SecretKey::from_byte_array(*secret).unwrap(),
        )
        .serialize()
        .to_vec()
    }
    #[test]
    fn invoice_authorization_binds_claim_identity_economics_and_invoice() {
        let context = InvoiceAuthorizationContext {
            keygen_session_id: SessionId::new_v7(),
            user_id: UserId::new_v7(),
            claim_id: Uuid::now_v7(),
            competition_id: Uuid::now_v7(),
            entry_id: Uuid::now_v7(),
            contract_digest: "contract".into(),
            invoice_digest: "invoice".into(),
            amount_msat: 123000,
            expires_at: 500,
        };
        let signed = SignedInvoiceAuthorization::sign(&[1; 32], context.clone()).unwrap();
        signed.verify(&public(&[1; 32]), &context, 499).unwrap();
        assert!(signed.verify(&public(&[1; 32]), &context, 501).is_err());
        for field in 0..9 {
            let mut changed = signed.clone();
            match field {
                0 => changed.context.keygen_session_id = SessionId::new_v7(),
                1 => changed.context.user_id = UserId::new_v7(),
                2 => changed.context.claim_id = Uuid::now_v7(),
                3 => changed.context.competition_id = Uuid::now_v7(),
                4 => changed.context.entry_id = Uuid::now_v7(),
                5 => changed.context.contract_digest.push('x'),
                6 => changed.context.invoice_digest.push('x'),
                7 => changed.context.amount_msat += 1,
                _ => changed.context.expires_at += 1,
            }
            assert!(changed
                .verify(&public(&[1; 32]), &changed.context, 499)
                .is_err());
        }
    }
    fn response(
        context: keymeld_core::escrow::protocol::RequestContext,
        output: keymeld_core::escrow::protocol::Payload,
    ) -> keymeld_core::escrow::protocol::EscrowResponse {
        use keymeld_core::escrow::{
            self,
            protocol::{EscrowResponse, Payload, ReceiptContext},
        };
        EscrowResponse::sign(
            ReceiptContext {
                schema_version: escrow::SCHEMA_VERSION,
                enclave_id: crate::EnclaveId::new(1),
                enclave_key_epoch: 3,
                request: context,
                request_digest: [2; 32],
            },
            output,
            Payload::new(vec![7; 32]).unwrap(),
            &[2; 32],
        )
        .unwrap()
    }
    fn context(
        operation: keymeld_core::escrow::protocol::Operation,
    ) -> keymeld_core::escrow::protocol::RequestContext {
        use keymeld_core::escrow::{
            self, protocol::RequestContext, ApplicationContext, EscrowContext,
        };
        RequestContext {
            schema_version: escrow::SCHEMA_VERSION,
            operation,
            escrow: EscrowContext {
                keygen_session_id: SessionId::new_v7(),
                user_id: UserId::new_v7(),
                escrow_id: Uuid::now_v7(),
                manifest_digest: [3; 32],
                application: ApplicationContext::commit(
                    crate::generic::VERIFIER_ID.into(),
                    1,
                    b"policy",
                )
                .unwrap(),
            },
            policy_digest: [4; 32],
            request_id: Uuid::now_v7(),
            action_id: None,
            attempt: None,
        }
    }
    fn prepared_fixture() -> PayoutPreparedResponse {
        use keymeld_core::escrow::{
            protocol::{Operation, Payload},
            ActionAttempt,
        };
        let mut first = context(Operation::Prepare);
        let claim_id = Uuid::now_v7();
        first.action_id = Some(crate::generic::RELEASE_PREIMAGE.into());
        first.attempt = Some(ActionAttempt {
            attempt_id: claim_id,
            signing_session_id: None,
        });
        let mut second = first.clone();
        second.action_id = Some(crate::generic::RELEASE_ENTRY_KEY.into());
        second.request_id = Uuid::now_v7();
        let output = Payload::encode(&crate::generic::PreparedSettlement {
            claim_id,
            contract_digest: "contract".into(),
            invoice: "invoice".into(),
            invoice_digest: crate::payout::invoice_digest("invoice"),
            payment_hash: "hash".into(),
            owed_sats: 100,
        })
        .unwrap();
        PayoutPreparedResponse::from_responses(
            response(first, output.clone()),
            response(second, output),
        )
        .unwrap()
    }
    #[test]
    fn generic_response_signatures_reject_application_field_and_receipt_substitution() {
        let prepared = prepared_fixture();
        prepared.verify(&public(&[2; 32])).unwrap();
        assert!(prepared.verify(&public(&[3; 32])).is_err());
        for field in 0..10 {
            let mut changed = prepared.clone();
            match field {
                0 => changed.keygen_session_id = SessionId::new_v7(),
                1 => changed.enclave_id = crate::EnclaveId::new(2),
                2 => changed.user_id = UserId::new_v7(),
                3 => changed.claim_id = Uuid::now_v7(),
                4 => changed.invoice.push('x'),
                5 => changed.invoice_digest.push('x'),
                6 => changed.payment_hash.push('x'),
                7 => changed.owed_sats += 1,
                8 => changed.state_receipt.push('x'),
                _ => changed.key_preparation.enclave_signature[0] ^= 1,
            }
            assert!(
                changed.verify(&public(&[2; 32])).is_err(),
                "mutation {field}"
            );
        }
    }
    #[test]
    fn independent_release_receipts_must_share_policy_attempt_and_invoice() {
        let prepared = prepared_fixture();
        let first = prepared.preimage_preparation;
        for field in 0..5 {
            let mut second = prepared.key_preparation.clone();
            match field {
                0 => second.context.request.attempt.as_mut().unwrap().attempt_id = Uuid::now_v7(),
                1 => second.context.request.escrow.user_id = UserId::new_v7(),
                2 => second.context.request.policy_digest[0] ^= 1,
                3 => {
                    second.context.request.action_id = Some(crate::generic::RELEASE_PREIMAGE.into())
                }
                _ => second.output = keymeld_core::escrow::protocol::Payload::default(),
            }
            assert!(PayoutPreparedResponse::from_responses(first.clone(), second).is_err());
        }
    }
    #[test]
    fn generic_binding_authenticates_contract_policy_roster_participant_and_epoch() {
        use keymeld_core::escrow::{
            self,
            protocol::{BindingOutput, Operation, Payload},
        };
        let (contract, _) = crate::payout::tests::fixture();
        let binding = crate::generic::ContractBinding { contract };
        let context = context(Operation::Bind);
        let output = Payload::encode(&BindingOutput {
            binding_data_digest: escrow::sha256(Payload::encode(&binding).unwrap().as_bytes()),
            participant_policy_digests: std::collections::BTreeMap::from([(
                context.escrow.user_id.clone(),
                context.policy_digest,
            )]),
        })
        .unwrap();
        let bound =
            PayoutContractBoundResponse::from_response(binding, response(context, output)).unwrap();
        bound.verify(&public(&[2; 32])).unwrap();
        for field in 0..5 {
            let mut changed = bound.clone();
            match field {
                0 => changed.user_id = UserId::new_v7(),
                1 => changed.policy_set_digest.push('x'),
                2 => changed.enclave_key_epoch += 1,
                3 => changed.binding_receipt.push('x'),
                _ => {
                    changed.binding.contract.contract_parameters.funding_value =
                        crate::payout::dlctix::bitcoin::Amount::from_sat(1)
                }
            }
            assert!(changed.verify(&public(&[2; 32])).is_err());
        }
    }
}
