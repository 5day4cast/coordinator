//! Password-based encryption for nsec keys
//!
//! Uses scrypt for key derivation and XChaCha20-Poly1305 for encryption.
//! The encrypted format is: base64(salt[32] || nonce[24] || ciphertext || tag[16])

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use nostr_sdk::{EventBuilder, Keys, Kind};
use rand::RngCore;
use scrypt::{scrypt, Params};
use thiserror::Error;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 24; // XChaCha20 uses 192-bit nonce
const KEY_LEN: usize = 32;

// scrypt params: N=2^17, r=8, p=1 (128 MB memory, ~1-2 sec on modern devices)
// Provides strong protection against GPU-based brute force attacks
const SCRYPT_LOG_N: u8 = 17; // 2^17 = 131072
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

#[derive(Error, Debug)]
pub enum PasswordCryptoError {
    #[error("Encryption failed: {0}")]
    EncryptionFailed(String),
    #[error("Decryption failed: {0}")]
    DecryptionFailed(String),
    #[error("Invalid encrypted data format")]
    InvalidFormat,
    #[error("Key derivation failed: {0}")]
    KeyDerivationFailed(String),
    #[error("Invalid nsec format: {0}")]
    InvalidNsec(String),
    #[error("Signing failed: {0}")]
    SigningFailed(String),
}

#[cfg(target_arch = "wasm32")]
impl From<PasswordCryptoError> for JsValue {
    fn from(error: PasswordCryptoError) -> Self {
        JsValue::from_str(&error.to_string())
    }
}

/// Derive a 32-byte key from password using scrypt
fn derive_key(password: &[u8], salt: &[u8]) -> Result<[u8; KEY_LEN], PasswordCryptoError> {
    let params = Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, KEY_LEN)
        .map_err(|e| PasswordCryptoError::KeyDerivationFailed(e.to_string()))?;

    let mut key = [0u8; KEY_LEN];
    scrypt(password, salt, &params, &mut key)
        .map_err(|e| PasswordCryptoError::KeyDerivationFailed(e.to_string()))?;

    Ok(key)
}

/// Encrypt nsec with password using scrypt + XChaCha20-Poly1305
///
/// Returns base64-encoded: salt[32] || nonce[24] || ciphertext || tag[16]
pub fn encrypt_nsec_with_password(
    nsec: &str,
    password: &str,
) -> Result<String, PasswordCryptoError> {
    // Generate random salt and nonce
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut salt);
    rand::rng().fill_bytes(&mut nonce_bytes);

    // Derive key from password
    let key = derive_key(password.as_bytes(), &salt)?;

    // Create cipher and encrypt
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| PasswordCryptoError::EncryptionFailed(e.to_string()))?;

    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, nsec.as_bytes())
        .map_err(|e| PasswordCryptoError::EncryptionFailed(e.to_string()))?;

    // Combine: salt || nonce || ciphertext (which includes the tag)
    let mut result = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    result.extend_from_slice(&salt);
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(&result))
}

/// Decrypt nsec with password
///
/// Input: base64-encoded blob from encrypt_nsec_with_password
pub fn decrypt_nsec_with_password(
    encrypted_blob: &str,
    password: &str,
) -> Result<String, PasswordCryptoError> {
    // Decode base64
    let data = BASE64
        .decode(encrypted_blob)
        .map_err(|_| PasswordCryptoError::InvalidFormat)?;

    // Minimum length: salt + nonce + tag (no ciphertext for empty nsec, but nsec is never empty)
    if data.len() < SALT_LEN + NONCE_LEN + 16 {
        return Err(PasswordCryptoError::InvalidFormat);
    }

    // Extract components
    let salt = &data[..SALT_LEN];
    let nonce_bytes = &data[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ciphertext = &data[SALT_LEN + NONCE_LEN..];

    // Derive key from password
    let key = derive_key(password.as_bytes(), salt)?;

    // Create cipher and decrypt
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| PasswordCryptoError::DecryptionFailed(e.to_string()))?;

    let nonce = XNonce::from_slice(nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_| {
        PasswordCryptoError::DecryptionFailed("Invalid password or corrupted data".to_string())
    })?;

    String::from_utf8(plaintext).map_err(|_| {
        PasswordCryptoError::DecryptionFailed("Decrypted data is not valid UTF-8".to_string())
    })
}

/// Sign a challenge for forgot password flow
///
/// Creates a Nostr event with the challenge as content and returns the signed event JSON.
/// Uses kind 27235 (NIP-98 HTTP Auth) as it's a generic signed message.
pub fn sign_forgot_password_challenge(
    nsec: &str,
    challenge: &str,
) -> Result<String, PasswordCryptoError> {
    // Parse nsec to Keys
    let keys = Keys::parse(nsec).map_err(|e| PasswordCryptoError::InvalidNsec(e.to_string()))?;

    // Create unsigned event with challenge as content
    // Using kind 27235 (NIP-98) as a generic "prove you own this key" mechanism
    let event = EventBuilder::new(Kind::HttpAuth, challenge)
        .sign_with_keys(&keys)
        .map_err(|e| PasswordCryptoError::SigningFailed(e.to_string()))?;

    // Return event as JSON
    serde_json::to_string(&event).map_err(|e| PasswordCryptoError::SigningFailed(e.to_string()))
}

// WASM bindings
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = "encryptNsecWithPassword")]
pub fn encrypt_nsec_with_password_wasm(nsec: &str, password: &str) -> Result<String, JsValue> {
    encrypt_nsec_with_password(nsec, password).map_err(|e| e.into())
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = "decryptNsecWithPassword")]
pub fn decrypt_nsec_with_password_wasm(
    encrypted_blob: &str,
    password: &str,
) -> Result<String, JsValue> {
    decrypt_nsec_with_password(encrypted_blob, password).map_err(|e| e.into())
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = "signForgotPasswordChallenge")]
pub fn sign_forgot_password_challenge_wasm(nsec: &str, challenge: &str) -> Result<String, JsValue> {
    sign_forgot_password_challenge(nsec, challenge).map_err(|e| e.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let nsec = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";
        let password = "test_password_123";

        let encrypted = encrypt_nsec_with_password(nsec, password).unwrap();
        let decrypted = decrypt_nsec_with_password(&encrypted, password).unwrap();

        assert_eq!(nsec, decrypted);
    }

    #[test]
    fn test_wrong_password_fails() {
        let nsec = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";
        let password = "correct_password";
        let wrong_password = "wrong_password";

        let encrypted = encrypt_nsec_with_password(nsec, password).unwrap();
        let result = decrypt_nsec_with_password(&encrypted, wrong_password);

        assert!(result.is_err());
    }

    #[test]
    fn test_different_encryptions_produce_different_output() {
        let nsec = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";
        let password = "test_password";

        let encrypted1 = encrypt_nsec_with_password(nsec, password).unwrap();
        let encrypted2 = encrypt_nsec_with_password(nsec, password).unwrap();

        // Different salt/nonce should produce different ciphertext
        assert_ne!(encrypted1, encrypted2);

        // But both should decrypt to the same value
        let decrypted1 = decrypt_nsec_with_password(&encrypted1, password).unwrap();
        let decrypted2 = decrypt_nsec_with_password(&encrypted2, password).unwrap();
        assert_eq!(decrypted1, decrypted2);
    }

    #[test]
    fn test_invalid_base64_fails() {
        let result = decrypt_nsec_with_password("not_valid_base64!!!", "password");
        assert!(result.is_err());
    }

    #[test]
    fn test_too_short_data_fails() {
        let short_data = BASE64.encode([0u8; 10]);
        let result = decrypt_nsec_with_password(&short_data, "password");
        assert!(result.is_err());
    }

    #[test]
    fn test_sign_challenge() {
        let nsec = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";
        let challenge = "abc123def456";

        let signed_event = sign_forgot_password_challenge(nsec, challenge).unwrap();

        // Should be valid JSON
        let event: serde_json::Value = serde_json::from_str(&signed_event).unwrap();

        // Should have expected structure
        assert!(event.get("id").is_some());
        assert!(event.get("pubkey").is_some());
        assert!(event.get("sig").is_some());
        assert_eq!(event.get("content").unwrap().as_str().unwrap(), challenge);
    }
}
