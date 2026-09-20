use anyhow::{Context, Result};
use coordinator_core::{
    keymeld::{
        payout::ContractAuthorization, prepare_payout_registration, prepare_registration,
        PayoutPolicy, PreparedRegistration,
    },
    RegistrationAssignment,
};

/// Verify the assigned enclave before preparing a participant-bound envelope.
pub async fn prepare_for_ticket(
    private_key_hex: &str,
    assignment: &RegistrationAssignment,
    choice: &coordinator_core::PayoutRegistrationRequest,
    competition_id: uuid::Uuid,
    ticket_hash: &str,
    payout_preimage_hex: &str,
) -> Result<PreparedRegistration> {
    let private_key: [u8; 32] = hex::decode(private_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Private key must be 32 bytes"))?;
    if let Some(json) = assignment.payout_policy.as_ref() {
        let policy: PayoutPolicy = serde_json::from_str(json)?;
        let terms = ContractAuthorization::from_policy(&policy)?;
        if terms.entry_id != choice.entry_id
            || terms.competition_id != competition_id
            || hex::encode(terms.ticket_hash) != ticket_hash
            || hex::encode(terms.payout_hash) != choice.payout_hash
            || policy.automatic_lightning_address != choice.lightning_address
            || policy.allow_invoice_fallback != choice.allow_invoice_fallback
            || policy.release_entry_key_after_payment != choice.release_entry_key_after_payment
        {
            anyhow::bail!("Ticket payout policy differs from the synth entry authorization");
        }
        let preimage: [u8; 32] = hex::decode(payout_preimage_hex)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Payout preimage must be 32 bytes"))?;
        prepare_payout_registration(&private_key, &preimage, assignment)
            .await
            .context("Failed to prepare payout escrow registration")
    } else {
        prepare_registration(&private_key, assignment)
            .await
            .context("Failed to prepare authorized Keymeld registration")
    }
}
