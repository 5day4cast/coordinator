//! Username-login credentials.
//!
//! Browsers never send the password. They send an `AuthKey` derived from it
//! with scrypt (see `coordinator-wasm` `nostr::login`); a separate key derived
//! from the same password seals the user's nsec and never leaves the browser.
//! The server stores only an argon2 hash of the auth key.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use serde::Deserialize;
use std::{fmt, sync::LazyLock};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

const AUTH_KEY_HEX_LEN: usize = 64;

#[derive(Debug, Error)]
pub enum PasswordError {
    #[error("Hash failed: {0}")]
    HashError(String),
    #[error("Stored hash is malformed: {0}")]
    MalformedHash(String),
    #[error("Auth key must be {AUTH_KEY_HEX_LEN} lowercase hex characters")]
    InvalidAuthKey,
}

/// A client-derived login credential: 32 bytes as lowercase hex.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(try_from = "String")]
pub struct AuthKey(String);

impl TryFrom<String> for AuthKey {
    type Error = PasswordError;

    fn try_from(mut value: String) -> Result<Self, Self::Error> {
        let valid = value.len() == AUTH_KEY_HEX_LEN
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(value))
        } else {
            value.zeroize();
            Err(PasswordError::InvalidAuthKey)
        }
    }
}

impl fmt::Debug for AuthKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthKey([REDACTED])")
    }
}

pub fn hash_auth_key(key: &AuthKey) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(key.0.as_bytes(), &salt)
        .map_err(|e| PasswordError::HashError(e.to_string()))?;
    Ok(hash.to_string())
}

/// Constant-work check: an unknown user (`stored_hash == None`) is verified
/// against a real dummy hash, so response time does not reveal whether the
/// username exists.
pub fn verify_auth_key(key: &AuthKey, stored_hash: Option<&str>) -> Result<bool, PasswordError> {
    static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
        let dummy = AuthKey("0".repeat(AUTH_KEY_HEX_LEN));
        hash_auth_key(&dummy).expect("argon2 hashes a fixed input")
    });

    let exists = stored_hash.is_some();
    let hash = stored_hash.unwrap_or(DUMMY_HASH.as_str());
    let parsed =
        PasswordHash::new(hash).map_err(|e| PasswordError::MalformedHash(e.to_string()))?;
    let matches = Argon2::default()
        .verify_password(key.0.as_bytes(), &parsed)
        .is_ok();
    Ok(exists && matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: char) -> AuthKey {
        AuthKey::try_from(byte.to_string().repeat(AUTH_KEY_HEX_LEN)).unwrap()
    }

    #[test]
    fn verifies_matching_auth_key_only() {
        let hash = hash_auth_key(&key('a')).unwrap();

        assert!(verify_auth_key(&key('a'), Some(&hash)).unwrap());
        assert!(!verify_auth_key(&key('b'), Some(&hash)).unwrap());
    }

    #[test]
    fn unknown_user_never_verifies() {
        let dummy = AuthKey("0".repeat(AUTH_KEY_HEX_LEN));
        assert!(!verify_auth_key(&dummy, None).unwrap());
    }

    #[test]
    fn hashes_are_salted() {
        assert_ne!(
            hash_auth_key(&key('c')).unwrap(),
            hash_auth_key(&key('c')).unwrap()
        );
    }

    #[test]
    fn rejects_malformed_auth_keys() {
        for bad in [
            "",
            "short",
            &"A".repeat(64),
            &"g".repeat(64),
            &"a".repeat(65),
        ] {
            assert!(AuthKey::try_from(bad.to_string()).is_err());
        }
    }

    #[test]
    fn debug_output_is_redacted() {
        assert_eq!(format!("{:?}", key('d')), "AuthKey([REDACTED])");
    }

    #[test]
    fn deserializes_only_valid_keys() {
        let json = format!("\"{}\"", "e".repeat(64));
        assert!(serde_json::from_str::<AuthKey>(&json).is_ok());
        assert!(serde_json::from_str::<AuthKey>("\"hunter2\"").is_err());
    }
}
