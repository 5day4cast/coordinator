//! Keymeld crypto utilities for WASM
//!
//! This module provides cryptographic functions for server-side keymeld registration.
//! The coordinator handles all keymeld communication - users just need to:
//! 1. Derive their auth pubkey from their ephemeral private key
//! 2. Encrypt their private key to the enclave's public key
//!
//! These values are sent with the entry submission and the coordinator
//! registers the user with keymeld on their behalf.

use keymeld_sdk::prelude::*;
use wasm_bindgen::prelude::*;

/// Derive the keymeld auth pubkey from a private key and session ID.
///
/// This is used for server-side keymeld registration where the coordinator
/// registers participants on their behalf. The user derives their auth_pubkey
/// client-side and sends it to the coordinator along with their encrypted private key.
///
/// @param private_key_hex - The user's ephemeral private key (hex encoded, 32 bytes)
/// @param session_id - The keymeld keygen session ID
/// @returns The derived auth public key (hex encoded)
#[wasm_bindgen]
pub fn derive_keymeld_auth_pubkey(
    private_key_hex: &str,
    session_id: &str,
) -> Result<String, JsValue> {
    let private_key_bytes: [u8; 32] = hex::decode(private_key_hex)
        .map_err(|e| JsValue::from_str(&format!("Invalid private key hex: {}", e)))?
        .try_into()
        .map_err(|_| JsValue::from_str("Private key must be 32 bytes"))?;

    let credentials = UserCredentials::from_private_key(&private_key_bytes)
        .map_err(|e| JsValue::from_str(&format!("Failed to create credentials: {}", e)))?;

    let auth_pubkey = credentials
        .derive_session_auth_pubkey(session_id)
        .map_err(|e| JsValue::from_str(&format!("Failed to derive auth pubkey: {}", e)))?;

    Ok(hex::encode(auth_pubkey))
}

/// Encrypt a private key for submission to a keymeld enclave.
///
/// This is used for server-side keymeld registration. The user encrypts their
/// ephemeral private key to the enclave's public key, allowing the coordinator
/// to forward it to keymeld without being able to read the key.
///
/// @param private_key_hex - The user's ephemeral private key (hex encoded, 32 bytes)
/// @param enclave_pubkey_hex - The keymeld enclave's public key (hex encoded)
/// @returns The encrypted private key (hex encoded)
#[wasm_bindgen]
pub fn encrypt_private_key_for_enclave(
    private_key_hex: &str,
    enclave_pubkey_hex: &str,
) -> Result<String, JsValue> {
    let private_key_bytes: [u8; 32] = hex::decode(private_key_hex)
        .map_err(|e| JsValue::from_str(&format!("Invalid private key hex: {}", e)))?
        .try_into()
        .map_err(|_| JsValue::from_str("Private key must be 32 bytes"))?;

    let credentials = UserCredentials::from_private_key(&private_key_bytes)
        .map_err(|e| JsValue::from_str(&format!("Failed to create credentials: {}", e)))?;

    let encrypted = credentials
        .encrypt_private_key_for_enclave(enclave_pubkey_hex)
        .map_err(|e| JsValue::from_str(&format!("Failed to encrypt for enclave: {}", e)))?;

    Ok(encrypted)
}
