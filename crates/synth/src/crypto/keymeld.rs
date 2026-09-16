use anyhow::{Context, Result};
use coordinator_core::{
    keymeld::{prepare_registration, PreparedRegistration},
    RegistrationAssignment,
};

/// Verify the assigned enclave before preparing a participant-bound envelope.
pub async fn prepare_for_ticket(
    private_key_hex: &str,
    assignment: &RegistrationAssignment,
) -> Result<PreparedRegistration> {
    let private_key: [u8; 32] = hex::decode(private_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Private key must be 32 bytes"))?;
    prepare_registration(&private_key, assignment)
        .await
        .context("Failed to prepare authorized Keymeld registration")
}
