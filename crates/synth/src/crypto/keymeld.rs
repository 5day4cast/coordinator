use anyhow::Result;
use keymeld_sdk::prelude::UserCredentials;

/// Derive the keymeld auth pubkey from an ephemeral private key and session ID.
/// Used for server-side keymeld registration.
pub fn derive_auth_pubkey(private_key_hex: &str, session_id: &str) -> Result<String> {
    let private_key_bytes: [u8; 32] = hex::decode(private_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Private key must be 32 bytes"))?;

    let credentials = UserCredentials::from_private_key(&private_key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to create credentials: {}", e))?;

    let auth_pubkey = credentials
        .derive_session_auth_pubkey(session_id)
        .map_err(|e| anyhow::anyhow!("Failed to derive auth pubkey: {}", e))?;

    Ok(hex::encode(auth_pubkey))
}

/// Encrypt an ephemeral private key for a keymeld enclave.
/// The coordinator forwards this to keymeld for server-side registration.
pub fn encrypt_for_enclave(private_key_hex: &str, enclave_pubkey_hex: &str) -> Result<String> {
    let private_key_bytes: [u8; 32] = hex::decode(private_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Private key must be 32 bytes"))?;

    let credentials = UserCredentials::from_private_key(&private_key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to create credentials: {}", e))?;

    let encrypted = credentials
        .encrypt_private_key_for_enclave(enclave_pubkey_hex)
        .map_err(|e| anyhow::anyhow!("Failed to encrypt for enclave: {}", e))?;

    Ok(encrypted)
}
