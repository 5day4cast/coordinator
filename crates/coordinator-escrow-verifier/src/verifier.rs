//! Application semantics only. Custody keys, deposits, sealing and execution
//! remain in Keymeld; this verifier can only propose participant-authorized actions.
use coordinator_escrow::payout::dlctix::ContractSignatures;
use coordinator_escrow::{
    ark::{self, ArkEscrowSpend, ArkFunding},
    authorization::PayoutPolicy,
    generic::{self, ActionParameters, ContractBinding, PaymentEvidence, PreparedSettlement},
    payout::{self, ContractAuthorization, ContractCommitment},
    payout_protocol::{InvoiceAuthorizationContext, PayoutMethod},
};
use keymeld_core::{
    authorization::SignedSessionManifest,
    escrow::{
        self, protocol::Payload, Action, AdaptorContext, Bip340Item, Bip340Scope, Condition,
        KeyTweak, Permission, PublicKeyBytes, SignedEscrowPolicy, SigningScope,
    },
    protocol::{EnclaveError, ValidationError},
    SessionId, UserId,
};
use keymeld_enclave::escrow_verifier::{
    BindView, EscrowVerifier, ExecutionView, PreparationView, PreparedAction, RegistrationView,
    VerificationError, VerificationFuture, VerifierDescriptor,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_LEDGER_ENTRIES: usize = 4096;
type Claim = (SessionId, UserId, Uuid);

// Process-local duplicate-payment defense. Sealed receipts and this cache do not
// establish global antirollback or durable cross-restart uniqueness.
#[derive(Default)]
struct SettlementLedger {
    payment_hashes: BTreeMap<String, Claim>,
    released_entries: BTreeMap<(SessionId, UserId), Uuid>,
}

#[derive(Default)]
pub struct CoordinatorVerifier {
    #[cfg(feature = "lnurl")]
    lnurl: Option<crate::lnurl_transport::LnurlPayClient>,
    ledger: Mutex<SettlementLedger>,
}
impl CoordinatorVerifier {
    /// Enables network preparation only in this statically registered verifier.
    /// The default constructor permits invoice fallback and paid-claim recovery.
    #[cfg(feature = "lnurl")]
    pub fn with_lnurl(client: crate::lnurl_transport::LnurlPayClient) -> Self {
        Self {
            lnurl: Some(client),
            ledger: Mutex::default(),
        }
    }
    pub fn lnurl_enabled(&self) -> bool {
        #[cfg(feature = "lnurl")]
        {
            self.lnurl.is_some()
        }
        #[cfg(not(feature = "lnurl"))]
        {
            false
        }
    }
    fn reserve_payment(
        &self,
        claim: Claim,
        invoice_hash: &str,
        executing: bool,
    ) -> Result<(), VerificationError> {
        let mut ledger = self
            .ledger
            .lock()
            .map_err(|_| invalid("Settlement ledger lock poisoned"))?;
        if ledger
            .payment_hashes
            .get(invoice_hash)
            .is_some_and(|owner| owner != &claim)
        {
            return Err(invalid("Payment hash is committed to another claim"));
        }
        let entry = (claim.0.clone(), claim.1.clone());
        if ledger
            .released_entries
            .get(&entry)
            .is_some_and(|attempt| *attempt != claim.2)
        {
            return Err(invalid(
                "Entry has already been released under another claim",
            ));
        }
        if !ledger.payment_hashes.contains_key(invoice_hash)
            && ledger.payment_hashes.len() >= MAX_LEDGER_ENTRIES
        {
            return Err(invalid("Settlement ledger capacity reached"));
        }
        ledger
            .payment_hashes
            .insert(invoice_hash.into(), claim.clone());
        if executing {
            ledger.released_entries.insert(entry, claim.2);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundContract {
    contract: ContractCommitment,
    contract_digest: String,
    policy_digest: [u8; 32],
    manifest_digest: [u8; 32],
    participant_public_keys: BTreeMap<UserId, PublicKeyBytes>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PreparedState {
    ContractSigning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ark_funding: Option<ArkFunding>,
    },
    ArkEscrowSpend {
        spend: ArkEscrowSpend,
    },
    /// The refund of an escrow whose competition never kicked off, with the invoice the
    /// verifier resolved when preparing it. Later steps recheck the swap against that invoice
    /// rather than resolving the Lightning Address again.
    ArkRefund {
        spend: ArkEscrowSpend,
        invoice: String,
        /// What the invoice pays the player. Rechecked against the escrow and the player's fee
        /// cap, so a changed value cannot widen what the swap keeps.
        owed_sats: u64,
    },
    Settlement {
        request_digest: [u8; 32],
        settlement: PreparedSettlement,
    },
}

fn invalid(message: impl ToString) -> VerificationError {
    EnclaveError::Validation(ValidationError::Other(message.to_string()))
}
fn now() -> Result<u64, VerificationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .map_err(invalid)
}
fn hex32(value: &str) -> Result<[u8; 32], VerificationError> {
    hex::decode(value)
        .map_err(invalid)?
        .try_into()
        .map_err(|_| invalid("Expected exactly 32 bytes"))
}
fn policy(
    signed: &SignedEscrowPolicy,
) -> Result<(PayoutPolicy, ContractAuthorization), VerificationError> {
    let selection = signed
        .policy
        .verifier
        .as_ref()
        .ok_or_else(|| invalid("Missing Coordinator verifier selection"))?;
    if selection.id != generic::VERIFIER_ID || selection.version != generic::VERIFIER_VERSION {
        return Err(invalid("Unsupported Coordinator verifier"));
    }
    let application = &signed.policy.context.application;
    if application.namespace != generic::VERIFIER_ID
        || application.version != generic::VERIFIER_VERSION
        || application.commitment != escrow::sha256(selection.policy_data.as_bytes())
    {
        return Err(invalid(
            "Application commitment differs from the signed rule bytes",
        ));
    }
    let value: PayoutPolicy = selection.policy_data.decode().map_err(invalid)?;
    let terms = ContractAuthorization::from_policy(&value).map_err(invalid)?;
    Ok((value, terms))
}
fn check_permissions(
    signed: &SignedEscrowPolicy,
    terms: &ContractAuthorization,
    ark_escrow: bool,
) -> Result<(), VerificationError> {
    let mut expected: BTreeSet<_> = [
        generic::SIGN_CONTRACT,
        generic::RELEASE_PREIMAGE,
        generic::RELEASE_ENTRY_KEY,
    ]
    .into_iter()
    .collect();
    if ark_escrow {
        expected.insert(generic::SIGN_ARK_ESCROW);
        expected.insert(generic::SIGN_ARK_REFUND);
    }
    if signed
        .policy
        .grants
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != expected
    {
        return Err(invalid(
            "Coordinator policy requires exactly its explicit permissions",
        ));
    }
    for (id, grant) in &signed.policy.grants {
        let (expected_rule, expected_repetition, expected_preparation) = match id.as_str() {
            generic::SIGN_CONTRACT => (
                generic::CONTRACT_RULE,
                if ark_escrow {
                    escrow::Repetition::VerifierAuthorizedAttempts
                } else {
                    escrow::Repetition::RepeatIdenticalSigningScope
                },
                escrow::PreparationPolicy::Single,
            ),
            generic::SIGN_ARK_ESCROW => (
                generic::ARK_ESCROW_RULE,
                escrow::Repetition::VerifierAuthorizedAttempts,
                escrow::PreparationPolicy::Single,
            ),
            generic::SIGN_ARK_REFUND => (
                generic::ARK_REFUND_RULE,
                escrow::Repetition::VerifierAuthorizedAttempts,
                escrow::PreparationPolicy::Single,
            ),
            _ => (
                generic::SETTLEMENT_RULE,
                escrow::Repetition::Once,
                escrow::PreparationPolicy::RenewableIdenticalAction,
            ),
        };
        if grant.unbound != (id.as_str() == generic::SIGN_ARK_REFUND) {
            return Err(invalid(
                "Only the Arkade refund permission may act without a binding",
            ));
        }
        if grant.repetition != expected_repetition {
            return Err(invalid("Coordinator permission repetition differs"));
        }
        if grant.preparation != expected_preparation {
            return Err(invalid("Coordinator permission preparation policy differs"));
        }
        if grant.condition
            != (Condition::VerifierRule {
                rule: expected_rule.into(),
            })
        {
            return Err(invalid(
                "Coordinator permission has an unsupported condition",
            ));
        }
        match (id.as_str(), &grant.operation) {
            (generic::SIGN_CONTRACT, Permission::Sign) => {}
            (generic::SIGN_ARK_ESCROW, Permission::SignBip340) => {}
            (generic::SIGN_ARK_REFUND, Permission::SignBip340) => {}
            (generic::RELEASE_PREIMAGE, Permission::ReleaseSecret { name, recipient })
                if name == generic::PREIMAGE_SECRET
                    && recipient.encryption_public_key.as_bytes()
                        == terms.market_maker.pubkey.serialize() => {}
            (
                generic::RELEASE_ENTRY_KEY,
                Permission::ReleaseSigningKey {
                    public_key,
                    recipient,
                },
            ) if public_key == &signed.policy.participant_public_key
                && recipient.encryption_public_key.as_bytes()
                    == terms.market_maker.pubkey.serialize() => {}
            _ => return Err(invalid("Coordinator permission operation differs")),
        }
    }
    if signed.policy.secrets.len() != 1
        || signed
            .policy
            .secrets
            .get(generic::PREIMAGE_SECRET)
            .is_none_or(|secret| secret.length != 32 || secret.sha256 != terms.payout_hash)
    {
        return Err(invalid(
            "Entry preimage commitment differs from authorized contract",
        ));
    }
    Ok(())
}
fn validate_static(
    manifest: &SignedSessionManifest,
    signed: &SignedEscrowPolicy,
) -> Result<(PayoutPolicy, ContractAuthorization), VerificationError> {
    signed
        .verify(
            &signed.policy.context,
            signed.policy.participant_public_key.as_bytes(),
        )
        .map_err(invalid)?;
    manifest.verify().map_err(invalid)?;
    if signed.policy.context.keygen_session_id != manifest.manifest.keygen_session_id
        || signed.policy.context.manifest_digest.as_slice()
            != manifest.digest().map_err(invalid)?.as_slice()
        || signed.policy.context.user_id == manifest.manifest.coordinator_user_id
        || !manifest
            .manifest
            .participant_verifiers
            .contains_key(&signed.policy.context.user_id)
    {
        return Err(invalid(
            "Coordinator escrow policy has invalid participant or manifest scope",
        ));
    }
    let (policy, terms) = policy(signed)?;
    if signed.policy.context.escrow_id != terms.entry_id {
        return Err(invalid(
            "Escrow identifier differs from the authorized entry",
        ));
    }
    check_permissions(signed, &terms, policy.ark_escrow.is_some())?;
    if let Some(ark_policy) = &policy.ark_escrow {
        ark_escrow(signed, &terms, ark_policy)?;
    }
    if let Some(address) = &policy.automatic_lightning_address {
        #[cfg(feature = "lnurl")]
        if crate::lnurl_transport::validate_address(address).map_err(invalid)? != *address {
            return Err(invalid(
                "Lightning Address must be canonical before participant consent",
            ));
        }
        #[cfg(not(feature = "lnurl"))]
        if address.is_empty() || address.len() > 320 {
            return Err(invalid("Invalid Lightning Address"));
        }
    }
    Ok((policy, terms))
}
fn restore_binding(
    manifest: &SignedSessionManifest,
    signed: &SignedEscrowPolicy,
    payload: &Payload,
) -> Result<(BoundContract, PayoutPolicy, ContractAuthorization), VerificationError> {
    let (policy, terms) = validate_static(manifest, signed)?;
    let bound: BoundContract = payload.decode().map_err(invalid)?;
    if bound.policy_digest != signed.policy.digest().map_err(invalid)?
        || bound.manifest_digest.as_slice() != manifest.digest().map_err(invalid)?.as_slice()
        || bound.contract_digest != payout::contract_digest(&bound.contract).map_err(invalid)?
    {
        return Err(invalid(
            "Bound application state differs from policy, manifest or contract",
        ));
    }
    terms
        .verify_contract(
            &bound.contract,
            signed.policy.participant_public_key.as_bytes(),
        )
        .map_err(invalid)?;
    Ok((bound, policy, terms))
}

fn xonly(compressed: &[u8]) -> Result<ark::XOnlyPublicKey, VerificationError> {
    match compressed {
        [0x02 | 0x03, rest @ ..] => ark::XOnlyPublicKey::from_slice(rest).map_err(invalid),
        _ => Err(invalid("Expected a compressed public key")),
    }
}
/// The entry's escrow, checked against its entry key and the pool's market maker.
fn ark_escrow(
    signed: &SignedEscrowPolicy,
    terms: &ContractAuthorization,
    policy: &coordinator_escrow::authorization::ArkEscrowPolicy,
) -> Result<coordinator_ark_escrow::EntryEscrow, VerificationError> {
    ark::escrow(
        policy,
        xonly(signed.policy.participant_public_key.as_bytes())?,
        xonly(&terms.market_maker.pubkey.serialize())?,
    )
    .map_err(invalid)
}
/// The contract to sign or settle: the bound one, or for an Arkade-funded pool, the bound one at
/// the outpoint the batch's commitment transaction pays.
fn funded(
    bound: &BoundContract,
    policy: &PayoutPolicy,
    ark_funding: Option<&ArkFunding>,
) -> Result<ContractCommitment, VerificationError> {
    match (&policy.ark_escrow, ark_funding) {
        (None, None) => Ok(bound.contract.clone()),
        (Some(_), Some(funding)) => ark::funded_contract(&bound.contract, funding).map_err(invalid),
        (None, Some(_)) => Err(invalid("Only an Arkade-funded pool names its commitment")),
        (Some(_), None) => Err(invalid(
            "An Arkade-funded pool needs its commitment transaction",
        )),
    }
}
/// Recompute this participant's escrow spend, as the BIP340 action Keymeld may sign.
fn ark_escrow_action(
    signed: &SignedEscrowPolicy,
    policy: &PayoutPolicy,
    terms: &ContractAuthorization,
    bound: &BoundContract,
    spend: &ArkEscrowSpend,
) -> Result<(Action, Vec<usize>), VerificationError> {
    let ark_policy = policy
        .ark_escrow
        .as_ref()
        .ok_or_else(|| invalid("This ticket has no Arkade escrow"))?;
    let escrow = ark_escrow(signed, terms, ark_policy)?;
    let digests =
        ark::spend_digests(&escrow, ark_policy, &bound.contract, spend).map_err(invalid)?;
    let inputs = digests.iter().map(|(input, _)| *input).collect();
    let items = digests
        .into_iter()
        .enumerate()
        .map(|(index, (_, digest))| Bip340Item {
            item_id: Uuid::from_u128(index as u128 + 1),
            digest,
        })
        .collect();
    Ok((
        Action::SignBip340 {
            scope: Bip340Scope {
                public_key: signed.policy.participant_public_key.clone(),
                items,
            },
        },
        inputs,
    ))
}

/// Recheck a prepared refund, against the invoice the verifier resolved when preparing it.
///
/// The Lightning Address is not resolved again: a second invoice would have its own payment
/// hash, which the swap could not commit to. The deadline is not rechecked either, since it was
/// fixed when the swap was made.
fn check_refund(
    context: &ExecutionView<'_>,
    prepared: &PreparedAction,
    spend: &ArkEscrowSpend,
    invoice: &str,
    owed_sats: u64,
) -> Result<(), VerificationError> {
    if context.rule != generic::ARK_REFUND_RULE
        || context.permission_id != generic::SIGN_ARK_REFUND
    {
        return Err(invalid("Escrow refund permission differs"));
    }
    let (policy, terms) = validate_static(context.manifest, context.policy)?;
    let (action, swap, refund) = ark_refund_action(context.policy, &policy, &terms, spend)?;
    if prepared.action != action {
        return Err(invalid("Prepared refund differs from its transactions"));
    }
    let ark_policy = policy
        .ark_escrow
        .as_ref()
        .ok_or_else(|| invalid("This ticket has no Arkade escrow"))?;
    let fee_sats = refund
        .value_sats
        .checked_sub(owed_sats)
        .ok_or_else(|| invalid("Prepared refund pays more than the escrow holds"))?;
    if ark::refund_invoice_sats(&refund, fee_sats, ark_policy).map_err(invalid)? != owed_sats {
        return Err(invalid("Prepared refund pays the player a different amount"));
    }
    let parsed = payout::validate_prepared_invoice(invoice, owed_sats, terms.network)
        .map_err(|_| invalid("Prepared refund invoice is invalid"))?;
    let payment_hash = *(parsed.payment_hash().as_ref() as &[u8; 32]);
    ark::check_refund_invoice(&swap, payment_hash).map_err(invalid)
}

/// Recompute this participant's refund, as the BIP340 action Keymeld may sign.
///
/// A refund acts without a binding: its pool never formed, so there is no contract and possibly
/// no completed keygen session. Everything it needs comes from the participant's own signed
/// policy: the escrow's terms, and the cap on what a swap may keep.
fn ark_refund_action(
    signed: &SignedEscrowPolicy,
    policy: &PayoutPolicy,
    terms: &ContractAuthorization,
    spend: &ArkEscrowSpend,
) -> Result<(Action, coordinator_ark_escrow::RefundSwap, ark::RefundSpend), VerificationError> {
    let ark_policy = policy
        .ark_escrow
        .as_ref()
        .ok_or_else(|| invalid("This ticket has no Arkade escrow"))?;
    let escrow = ark_escrow(signed, terms, ark_policy)?;
    let (swap, refund) = ark::refund_from(&escrow, spend).map_err(invalid)?;
    let action = Action::SignBip340 {
        scope: Bip340Scope {
            public_key: signed.policy.participant_public_key.clone(),
            items: vec![Bip340Item {
                item_id: Uuid::from_u128(1),
                digest: refund.digest,
            }],
        },
    };
    Ok((action, swap, refund))
}

/// Recompute all messages this participant must sign. Client-selected routing
/// IDs are checked against the authenticated manifest; every cryptographic value
/// is derived from the bound DLC instead of accepted from the application host.
fn validate_signing_scope(
    manifest: &SignedSessionManifest,
    signed: &SignedEscrowPolicy,
    bound: &BoundContract,
    contract: &ContractCommitment,
    scope: &SigningScope,
) -> Result<(), VerificationError> {
    scope
        .validate(
            &signed.policy.context,
            &signed.policy.participant_public_key,
        )
        .map_err(invalid)?;
    if scope.session_tweak != KeyTweak::None {
        return Err(invalid("DLC contract signing cannot use a key tweak"));
    }
    let mut expected = payout::signing_requirements(contract)
        .map_err(invalid)?
        .into_iter()
        .filter(|(_, item)| {
            item.signers
                .iter()
                .any(|key| key.as_slice() == signed.policy.participant_public_key.as_bytes())
        })
        .map(|(message, item)| (escrow::sha256(&message), item))
        .collect::<BTreeMap<_, _>>();
    for item in &scope.batch {
        let requirement = expected.remove(&item.message_digest).ok_or_else(|| {
            invalid("Signing message is duplicated or outside the authorized DLC")
        })?;
        if item.tweak != KeyTweak::None
            || item
                .signers
                .iter()
                .map(|signer| signer.public_key.as_bytes())
                .collect::<Vec<_>>()
                != requirement
                    .signers
                    .iter()
                    .map(|key| key.as_slice())
                    .collect::<Vec<_>>()
        {
            return Err(invalid(
                "Signing keys or tweak differ from the authorized DLC",
            ));
        }
        let ids = match item.subset_id {
            None => bound
                .participant_public_keys
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            Some(id) => manifest
                .manifest
                .subset_definitions
                .iter()
                .find(|subset| subset.subset_id == id)
                .ok_or_else(|| invalid("Unknown signing subset"))?
                .participants
                .iter()
                .cloned()
                .collect(),
        };
        if ids
            != item
                .signers
                .iter()
                .map(|signer| signer.user_id.clone())
                .collect()
            || item.signers.iter().any(|signer| {
                bound.participant_public_keys.get(&signer.user_id) != Some(&signer.public_key)
            })
        {
            return Err(invalid(
                "Signing subset or participant identity differs from bound roster",
            ));
        }
        match (&item.adaptor, requirement.adaptor_point) {
            (AdaptorContext::None, None) => {}
            (AdaptorContext::Single { point, .. }, Some(expected))
                if point.as_bytes() == expected => {}
            _ => {
                return Err(invalid(
                    "Adaptor point differs from the authorized oracle outcome",
                ))
            }
        }
    }
    if !expected.is_empty() {
        return Err(invalid(
            "Signing plan omits required participant DLC messages",
        ));
    }
    Ok(())
}
fn release_action(signed: &SignedEscrowPolicy, id: &str) -> Result<Action, VerificationError> {
    match signed.policy.grants.get(id).map(|grant| &grant.operation) {
        Some(Permission::ReleaseSecret { name, recipient }) if id == generic::RELEASE_PREIMAGE => {
            Ok(Action::ReleaseSecret {
                name: name.clone(),
                recipient: recipient.clone(),
            })
        }
        Some(Permission::ReleaseSigningKey {
            public_key,
            recipient,
        }) if id == generic::RELEASE_ENTRY_KEY => Ok(Action::ReleaseSigningKey {
            public_key: public_key.clone(),
            recipient: recipient.clone(),
        }),
        _ => Err(invalid(
            "Settlement requires an explicit release permission",
        )),
    }
}
fn validate_settlement(
    bound: &BoundContract,
    terms: &ContractAuthorization,
    claim_id: Uuid,
    settlement: &PreparedSettlement,
) -> Result<(), VerificationError> {
    if settlement.claim_id != claim_id
        || settlement.contract_digest != bound.contract_digest
        || settlement.invoice_digest != payout::invoice_digest(&settlement.invoice)
    {
        return Err(invalid(
            "Prepared settlement belongs to a different claim or contract",
        ));
    }
    let invoice =
        payout::validate_prepared_invoice(&settlement.invoice, settlement.owed_sats, terms.network)
            .map_err(invalid)?;
    if settlement.payment_hash != hex::encode(invoice.payment_hash().as_ref() as &[u8]) {
        return Err(invalid("Prepared payment hash differs from invoice"));
    }
    Ok(())
}

impl CoordinatorVerifier {
    /// An invoice for `amount_msat` from a Lightning Address, resolved inside the enclave.
    ///
    /// The enclave asks for it itself, so the payment hash it then requires is one no caller
    /// chose.
    async fn request_invoice(
        &self,
        address: &str,
        amount_msat: u64,
    ) -> Result<String, VerificationError> {
        #[cfg(feature = "lnurl")]
        {
            self.lnurl
                .as_ref()
                .ok_or_else(|| invalid("Automatic Lightning Address resolution is disabled"))?
                .request_invoice(address, amount_msat)
                .await
                .map_err(invalid)
        }
        #[cfg(not(feature = "lnurl"))]
        {
            let _ = (address, amount_msat);
            Err(invalid(
                "LNURL support is not compiled into the Coordinator verifier",
            ))
        }
    }

    /// Authorize the refund of an escrow whose competition never kicked off.
    ///
    /// The player consented to the escrow's terms and to a cap on what a swap may keep. This
    /// resolves their own Lightning Address for the remainder, and signs only a refund paying a
    /// swap that commits to that invoice, so the service is paid by revealing its preimage.
    async fn prepare_refund(
        &self,
        context: PreparationView<'_>,
        spend: ArkEscrowSpend,
        fee_sats: u64,
    ) -> Result<PreparedAction, VerificationError> {
        if context.rule != generic::ARK_REFUND_RULE
            || context.permission_id != generic::SIGN_ARK_REFUND
        {
            return Err(invalid("Escrow refund permission differs"));
        }
        let (policy, terms) = validate_static(context.manifest, context.policy)?;
        let (action, swap, refund) = ark_refund_action(context.policy, &policy, &terms, &spend)?;
        let ark_policy = policy
            .ark_escrow
            .as_ref()
            .ok_or_else(|| invalid("This ticket has no Arkade escrow"))?;
        let owed_sats = ark::refund_invoice_sats(&refund, fee_sats, ark_policy).map_err(invalid)?;
        let amount_msat = owed_sats
            .checked_mul(1000)
            .ok_or_else(|| invalid("Refund amount overflow"))?;
        let address = policy
            .automatic_lightning_address
            .as_ref()
            .ok_or_else(|| invalid("This entry has no Lightning Address to refund"))?;
        let invoice = self.request_invoice(address, amount_msat).await?;
        let parsed = payout::validate_prepared_invoice(&invoice, owed_sats, terms.network)
            .map_err(invalid)?;
        let payment_hash = *(parsed.payment_hash().as_ref() as &[u8; 32]);
        ark::check_refund_invoice(&swap, payment_hash).map_err(invalid)?;
        let now = u32::try_from(now()?).map_err(|_| invalid("Clock is out of range"))?;
        ark::check_refund_deadline(&swap, now).map_err(invalid)?;
        context
            .attempt
            .validate(&action, &context.policy.policy.context)
            .map_err(invalid)?;
        Ok(PreparedAction {
            action,
            application_state: Payload::encode(&PreparedState::ArkRefund {
                spend,
                invoice,
                owed_sats,
            })
            .map_err(invalid)?,
            // The signed input, as for any other escrow spend.
            output: Payload::encode(&vec![0usize]).map_err(invalid)?,
        })
    }
}

impl EscrowVerifier for CoordinatorVerifier {
    fn descriptor(&self) -> VerifierDescriptor {
        VerifierDescriptor {
            id: generic::VERIFIER_ID.into(),
            version: generic::VERIFIER_VERSION,
        }
    }
    fn capabilities(&self) -> Result<Payload, VerificationError> {
        Payload::encode(
            &coordinator_escrow::payout_capabilities::PayoutCapabilities {
                payout: true,
                lnurl: self.lnurl_enabled(),
            },
        )
        .map_err(invalid)
    }
    fn validate_registration(
        &self,
        context: RegistrationView<'_>,
    ) -> Result<(), VerificationError> {
        let (policy, _) = validate_static(context.manifest, context.policy)?;
        if !context.restoring
            && policy.automatic_lightning_address.is_some()
            && !self.lnurl_enabled()
        {
            return Err(invalid(
                "Automatic Lightning Address resolution is disabled in this enclave",
            ));
        }
        Ok(())
    }
    fn bind(
        &self,
        context: BindView<'_>,
        binding_data: &Payload,
    ) -> Result<Payload, VerificationError> {
        let (_, terms) = validate_static(context.manifest, context.policy)?;
        let binding: ContractBinding = binding_data.decode().map_err(invalid)?;
        let manifest = &context.manifest.manifest;
        if context.participant_public_keys.keys().collect::<Vec<_>>()
            != manifest.participant_verifiers.keys().collect::<Vec<_>>()
        {
            return Err(invalid(
                "Binding requires the complete authenticated key roster",
            ));
        }
        let players = context
            .participant_public_keys
            .iter()
            .filter(|(id, _)| **id != manifest.coordinator_user_id)
            .collect::<Vec<_>>();
        if context.participant_policies.keys().collect::<Vec<_>>()
            != players.iter().map(|(id, _)| *id).collect::<Vec<_>>()
        {
            return Err(invalid(
                "Every DLC player must supply its authenticated escrow policy",
            ));
        }
        let params = &binding.contract.contract_parameters;
        params.validate().map_err(invalid)?;
        if params.players.len() != players.len()
            || context
                .participant_public_keys
                .get(&manifest.coordinator_user_id)
                .is_none_or(|key| key.as_bytes() != params.market_maker.pubkey.serialize())
            || params
                .players
                .iter()
                .zip(&players)
                .any(|(player, (_, key))| player.pubkey.serialize().as_slice() != key.as_bytes())
        {
            return Err(invalid(
                "Contract slots differ from the authenticated participant roster",
            ));
        }
        for (index, (user, key)) in players.iter().enumerate() {
            let accepted = context
                .participant_policies
                .get(*user)
                .ok_or_else(|| invalid("Missing player policy"))?;
            let (_, accepted_terms) = validate_static(context.manifest, accepted)?;
            if accepted.policy.context.user_id != **user
                || accepted.policy.participant_public_key != **key
                || accepted_terms.player_index != index
            {
                return Err(invalid(
                    "Player policy belongs to a different key or contract slot",
                ));
            }
            accepted_terms
                .verify_contract(&binding.contract, key.as_bytes())
                .map_err(invalid)?;
        }
        if context
            .participant_policies
            .get(&context.policy.policy.context.user_id)
            .is_none_or(|accepted| {
                accepted.policy.digest().ok() != context.policy.policy.digest().ok()
            })
        {
            return Err(invalid(
                "Binding policy differs from the enrolled participant policy",
            ));
        }
        terms
            .verify_contract(
                &binding.contract,
                context.policy.policy.participant_public_key.as_bytes(),
            )
            .map_err(invalid)?;
        Payload::encode(&BoundContract {
            contract_digest: payout::contract_digest(&binding.contract).map_err(invalid)?,
            contract: binding.contract,
            policy_digest: context.policy.policy.digest().map_err(invalid)?,
            manifest_digest: context
                .manifest
                .digest()
                .map_err(invalid)?
                .try_into()
                .map_err(|_| invalid("Invalid manifest digest"))?,
            participant_public_keys: context.participant_public_keys.clone(),
        })
        .map_err(invalid)
    }
    fn prepare<'a>(
        &'a self,
        context: PreparationView<'a>,
        action_parameters: &'a Payload,
    ) -> VerificationFuture<'a, PreparedAction> {
        Box::pin(async move {
            let parameters: ActionParameters = action_parameters.decode().map_err(invalid)?;
            // A refund acts without a binding, so it is prepared before one is restored.
            if let ActionParameters::RefundArkEscrow { spend, fee_sats } = parameters {
                return self.prepare_refund(context, spend, fee_sats).await;
            }
            let (bound, policy, terms) =
                restore_binding(context.manifest, context.policy, context.bound_state)?;
            if bound.participant_public_keys != *context.participant_public_keys {
                return Err(invalid("Restored binding key roster differs"));
            }
            match parameters {
                ActionParameters::RefundArkEscrow { .. } => {
                    Err(invalid("A refund is prepared without a binding"))
                }
                ActionParameters::SignContract { scope, ark_funding } => {
                    if context.rule != generic::CONTRACT_RULE
                        || context.permission_id != generic::SIGN_CONTRACT
                    {
                        return Err(invalid("Contract signing permission differs"));
                    }
                    let contract = funded(&bound, &policy, ark_funding.as_ref())?;
                    validate_signing_scope(
                        context.manifest,
                        context.policy,
                        &bound,
                        &contract,
                        &scope,
                    )?;
                    let action = Action::Sign { scope };
                    context
                        .attempt
                        .validate(&action, &context.policy.policy.context)
                        .map_err(invalid)?;
                    Ok(PreparedAction {
                        action,
                        application_state: Payload::encode(&PreparedState::ContractSigning {
                            ark_funding,
                        })
                        .map_err(invalid)?,
                        output: Payload::default(),
                    })
                }
                ActionParameters::SignArkEscrow { spend } => {
                    if context.rule != generic::ARK_ESCROW_RULE
                        || context.permission_id != generic::SIGN_ARK_ESCROW
                    {
                        return Err(invalid("Escrow spend permission differs"));
                    }
                    let (action, inputs) =
                        ark_escrow_action(context.policy, &policy, &terms, &bound, &spend)?;
                    context
                        .attempt
                        .validate(&action, &context.policy.policy.context)
                        .map_err(invalid)?;
                    Ok(PreparedAction {
                        action,
                        application_state: Payload::encode(&PreparedState::ArkEscrowSpend {
                            spend,
                        })
                        .map_err(invalid)?,
                        // The signed inputs, in the order of the signatures.
                        output: Payload::encode(&inputs).map_err(invalid)?,
                    })
                }
                ActionParameters::PrepareSettlement {
                    claim_id,
                    contract_signatures,
                    attestation,
                    method,
                    ark_funding,
                } => {
                    if context.rule != generic::SETTLEMENT_RULE
                        || context.attempt.attempt_id != claim_id
                    {
                        return Err(invalid("Settlement attempt or permission rule differs"));
                    }
                    let action = release_action(context.policy, context.permission_id)?;
                    context
                        .attempt
                        .validate(&action, &context.policy.policy.context)
                        .map_err(invalid)?;
                    let request_digest = escrow::sha256(action_parameters.as_bytes());
                    let settlement = if context.permission_id == generic::RELEASE_ENTRY_KEY {
                        // Reuse only a sibling preparation authenticated by the generic engine.
                        // Caller-supplied invoices or opaque application state cannot take this path.
                        let prior = context.prior_preparations.get(generic::RELEASE_PREIMAGE)
                            .ok_or_else(|| invalid("Key release requires the authenticated preimage preparation receipt"))?;
                        if prior.action
                            != release_action(context.policy, generic::RELEASE_PREIMAGE)?
                        {
                            return Err(invalid(
                                "Sibling preparation authorizes a different operation",
                            ));
                        }
                        let prior: PreparedState =
                            prior.application_state.decode().map_err(invalid)?;
                        match prior {
                            PreparedState::Settlement {
                                request_digest: expected,
                                settlement,
                            } if expected == request_digest => settlement,
                            _ => {
                                return Err(invalid(
                                    "Sibling preparation has different settlement inputs",
                                ))
                            }
                        }
                    } else {
                        let contract = funded(&bound, &policy, ark_funding.as_ref())?;
                        let signatures: ContractSignatures =
                            serde_json::from_str(&contract_signatures).map_err(invalid)?;
                        payout::verify_completed_contract(&contract, &signatures)
                            .map_err(invalid)?;
                        let outcome = payout::attested_outcome(
                            &contract.contract_parameters,
                            &hex32(&attestation)?,
                        )
                        .map_err(invalid)?;
                        let owed_sats = payout::owed_sats(
                            &contract.contract_parameters,
                            &outcome,
                            context.policy.policy.participant_public_key.as_bytes(),
                        )
                        .map_err(invalid)?;
                        let amount_msat = owed_sats
                            .checked_mul(1000)
                            .ok_or_else(|| invalid("Settlement amount overflow"))?;
                        let now = now()?;
                        let invoice = match method {
                            PayoutMethod::Automatic => {
                                let address =
                                    policy.automatic_lightning_address.as_ref().ok_or_else(
                                        || invalid("Automatic settlement was not authorized"),
                                    )?;
                                self.request_invoice(address, amount_msat).await?
                            }
                            PayoutMethod::Invoice {
                                invoice,
                                authorization,
                            } => {
                                if !policy.allow_invoice_fallback {
                                    return Err(invalid("Invoice fallback was not authorized"));
                                }
                                authorization
                                    .verify(
                                        context.policy.policy.participant_public_key.as_bytes(),
                                        &InvoiceAuthorizationContext {
                                            keygen_session_id: context
                                                .policy
                                                .policy
                                                .context
                                                .keygen_session_id
                                                .clone(),
                                            user_id: context.policy.policy.context.user_id.clone(),
                                            claim_id,
                                            competition_id: terms.competition_id,
                                            entry_id: terms.entry_id,
                                            contract_digest: bound.contract_digest.clone(),
                                            invoice_digest: payout::invoice_digest(&invoice),
                                            amount_msat,
                                            expires_at: authorization.context.expires_at,
                                        },
                                        now,
                                    )
                                    .map_err(invalid)?;
                                invoice
                            }
                        };
                        let parsed =
                            payout::validate_invoice(&invoice, owed_sats, terms.network, now)
                                .map_err(invalid)?;
                        PreparedSettlement {
                            claim_id,
                            contract_digest: bound.contract_digest.clone(),
                            invoice_digest: payout::invoice_digest(&invoice),
                            payment_hash: hex::encode(parsed.payment_hash().as_ref() as &[u8]),
                            invoice,
                            owed_sats,
                        }
                    };
                    validate_settlement(&bound, &terms, claim_id, &settlement)?;
                    self.reserve_payment(
                        (
                            context.policy.policy.context.keygen_session_id.clone(),
                            context.policy.policy.context.user_id.clone(),
                            claim_id,
                        ),
                        &settlement.payment_hash,
                        false,
                    )?;
                    Ok(PreparedAction {
                        action,
                        application_state: Payload::encode(&PreparedState::Settlement {
                            request_digest,
                            settlement: settlement.clone(),
                        })
                        .map_err(invalid)?,
                        output: Payload::encode(&settlement).map_err(invalid)?,
                    })
                }
            }
        })
    }
    fn verify_execution<'a>(
        &'a self,
        context: ExecutionView<'a>,
        prepared: &'a PreparedAction,
        evidence: &'a Payload,
    ) -> VerificationFuture<'a, ()> {
        Box::pin(async move {
            context
                .attempt
                .validate(&prepared.action, &context.policy.policy.context)
                .map_err(invalid)?;
            let state: PreparedState = prepared.application_state.decode().map_err(invalid)?;
            if let PreparedState::ArkRefund {
                spend,
                invoice,
                owed_sats,
            } = &state
            {
                if !evidence.as_bytes().is_empty() {
                    return Err(invalid("Invalid escrow refund execution"));
                }
                return check_refund(&context, prepared, spend, invoice, *owed_sats);
            }
            let (bound, policy, terms) =
                restore_binding(context.manifest, context.policy, context.bound_state)?;
            match state {
                PreparedState::ArkRefund { .. } => {
                    Err(invalid("A refund is executed without a binding"))?
                }
                PreparedState::ContractSigning { ark_funding } => {
                    if context.rule != generic::CONTRACT_RULE
                        || context.permission_id != generic::SIGN_CONTRACT
                        || !evidence.as_bytes().is_empty()
                    {
                        return Err(invalid("Invalid contract signing execution"));
                    }
                    let Action::Sign { scope } = &prepared.action else {
                        return Err(invalid("Prepared action is not contract signing"));
                    };
                    let contract = funded(&bound, &policy, ark_funding.as_ref())?;
                    validate_signing_scope(
                        context.manifest,
                        context.policy,
                        &bound,
                        &contract,
                        scope,
                    )?;
                }
                PreparedState::ArkEscrowSpend { spend } => {
                    if context.rule != generic::ARK_ESCROW_RULE
                        || context.permission_id != generic::SIGN_ARK_ESCROW
                        || !evidence.as_bytes().is_empty()
                    {
                        return Err(invalid("Invalid escrow spend execution"));
                    }
                    let (action, _) =
                        ark_escrow_action(context.policy, &policy, &terms, &bound, &spend)?;
                    if prepared.action != action {
                        return Err(invalid(
                            "Prepared escrow spend differs from its transactions",
                        ));
                    }
                }
                PreparedState::Settlement { settlement, .. } => {
                    if context.rule != generic::SETTLEMENT_RULE
                        || prepared.action != release_action(context.policy, context.permission_id)?
                    {
                        return Err(invalid("Prepared release differs from its permission"));
                    }
                    validate_settlement(&bound, &terms, context.attempt.attempt_id, &settlement)?;
                    if prepared.output != Payload::encode(&settlement).map_err(invalid)? {
                        return Err(invalid(
                            "Prepared output differs from authenticated settlement state",
                        ));
                    }
                    let evidence: PaymentEvidence = evidence.decode().map_err(invalid)?;
                    payout::verify_payment_preimage(
                        &settlement.invoice,
                        &evidence.payment_preimage,
                    )
                    .map_err(invalid)?;
                    self.reserve_payment(
                        (
                            context.policy.policy.context.keygen_session_id.clone(),
                            context.policy.policy.context.user_id.clone(),
                            context.attempt.attempt_id,
                        ),
                        &settlement.payment_hash,
                        true,
                    )?;
                }
            }
            context
                .attempt
                .validate(&prepared.action, &context.policy.policy.context)
                .map_err(invalid)
        })
    }
    fn restore_execution<'a>(
        &'a self,
        context: ExecutionView<'a>,
        prepared: &'a PreparedAction,
    ) -> VerificationFuture<'a, ()> {
        Box::pin(async move {
            // Keymeld calls this only after authenticating a sealed successful
            // execution. Restore the cross-permission claim fence without a new
            // invoice lookup or requiring disclosure of the original proof.
            context
                .attempt
                .validate(&prepared.action, &context.policy.policy.context)
                .map_err(invalid)?;
            let state: PreparedState = prepared.application_state.decode().map_err(invalid)?;
            if let PreparedState::ArkRefund {
                spend,
                invoice,
                owed_sats,
            } = &state
            {
                return check_refund(&context, prepared, spend, invoice, *owed_sats);
            }
            let (bound, policy, terms) =
                restore_binding(context.manifest, context.policy, context.bound_state)?;
            match state {
                PreparedState::ArkRefund { .. } => {
                    Err(invalid("A refund is restored without a binding"))?
                }
                PreparedState::ContractSigning { ark_funding } => {
                    if context.rule != generic::CONTRACT_RULE
                        || context.permission_id != generic::SIGN_CONTRACT
                    {
                        return Err(invalid("Invalid restored contract signing execution"));
                    }
                    let Action::Sign { scope } = &prepared.action else {
                        return Err(invalid("Restored action is not contract signing"));
                    };
                    let contract = funded(&bound, &policy, ark_funding.as_ref())?;
                    validate_signing_scope(
                        context.manifest,
                        context.policy,
                        &bound,
                        &contract,
                        scope,
                    )?;
                }
                PreparedState::ArkEscrowSpend { spend } => {
                    if context.rule != generic::ARK_ESCROW_RULE
                        || context.permission_id != generic::SIGN_ARK_ESCROW
                    {
                        return Err(invalid("Invalid restored escrow spend"));
                    }
                    let (action, _) =
                        ark_escrow_action(context.policy, &policy, &terms, &bound, &spend)?;
                    if prepared.action != action {
                        return Err(invalid(
                            "Restored escrow spend differs from its transactions",
                        ));
                    }
                }
                PreparedState::Settlement { settlement, .. } => {
                    if context.rule != generic::SETTLEMENT_RULE
                        || prepared.action != release_action(context.policy, context.permission_id)?
                    {
                        return Err(invalid("Restored release differs from its permission"));
                    }
                    validate_settlement(&bound, &terms, context.attempt.attempt_id, &settlement)?;
                    if prepared.output != Payload::encode(&settlement).map_err(invalid)? {
                        return Err(invalid("Restored output differs from settlement state"));
                    }
                    self.reserve_payment(
                        (
                            context.policy.policy.context.keygen_session_id.clone(),
                            context.policy.policy.context.user_id.clone(),
                            context.attempt.attempt_id,
                        ),
                        &settlement.payment_hash,
                        true,
                    )?;
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
