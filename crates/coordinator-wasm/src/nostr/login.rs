//! Username/password login keys.
//!
//! The password never leaves the browser:
//!
//! ```text
//! stretched = scrypt(password, salt = tagged_hash("login-salt", lowercase(username)))
//! auth_key  = tagged_hash("login-auth",  stretched)  -> sent to the server, which argon2-hashes it
//! vault_key = tagged_hash("login-vault", stretched)  -> never leaves WASM; seals the nsec
//! sealed    = base64(nonce[24] || XChaCha20-Poly1305(vault_key, nonce, nsec, aad = SEAL_AAD))
//! ```
//!
//! The server can verify a login but holds nothing that decrypts the nsec,
//! so it cannot reach the wallet seed, which is encrypted to the Nostr key.

use ::nostr::{Keys, SecretKey};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use scrypt::{scrypt, Params};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const SECRET_KEY_LEN: usize = 32;
const SEAL_AAD: &[u8] = b"coordinator/nostr-key/v1";

// N=2^17, r=8, p=1: 128 MiB and ~1-2 s per attempt in the browser.
const SCRYPT_LOG_N: u8 = 17;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

#[derive(Error, Debug)]
pub enum LoginError {
    #[error("Key stretching failed")]
    KeyStretching,
    #[error("Invalid sealed key")]
    InvalidSealedKey,
    #[error("Wrong username or password")]
    WrongPassword,
}

/// Keys derived from a username and password. Neither `Clone` nor `Debug`.
pub struct LoginKeys {
    auth_key: Zeroizing<[u8; 32]>,
    vault_key: Zeroizing<[u8; 32]>,
}

impl LoginKeys {
    pub fn derive(username: &str, password: &str) -> Result<Self, LoginError> {
        Ok(Self::from_stretched(&stretch(username, password)?))
    }

    /// The keys from [`stretch`]'s output. Cheap: log-in stretches the
    /// password in a worker, off the page's thread, and finishes here.
    pub fn from_stretched(stretched: &[u8; 32]) -> Self {
        Self {
            auth_key: tagged_hash(b"coordinator/login-auth/v1", &stretched[..]),
            vault_key: tagged_hash(b"coordinator/login-vault/v1", &stretched[..]),
        }
    }

    /// Hex credential the server verifies in place of the password.
    pub fn auth_key_hex(&self) -> String {
        hex::encode(&self.auth_key[..])
    }

    pub fn seal(&self, secret_key: &SecretKey) -> Result<String, LoginError> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let plaintext = Zeroizing::new(secret_key.to_secret_bytes());
        let ciphertext = self
            .cipher()
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext[..],
                    aad: SEAL_AAD,
                },
            )
            .map_err(|_| LoginError::InvalidSealedKey)?;

        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(BASE64.encode(sealed))
    }

    pub fn open(&self, sealed: &str) -> Result<Keys, LoginError> {
        let data = BASE64
            .decode(sealed)
            .map_err(|_| LoginError::InvalidSealedKey)?;
        if data.len() != NONCE_LEN + SECRET_KEY_LEN + TAG_LEN {
            return Err(LoginError::InvalidSealedKey);
        }
        let (nonce, ciphertext) = data.split_at(NONCE_LEN);
        let plaintext = Zeroizing::new(
            self.cipher()
                .decrypt(
                    XNonce::from_slice(nonce),
                    Payload {
                        msg: ciphertext,
                        aad: SEAL_AAD,
                    },
                )
                .map_err(|_| LoginError::WrongPassword)?,
        );
        let secret_key =
            SecretKey::from_slice(&plaintext).map_err(|_| LoginError::InvalidSealedKey)?;
        Ok(Keys::new(secret_key))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&*self.vault_key).into())
    }
}

/// scrypt(password) salted with the username: the slow step, ~1-2 s. Its
/// output unlocks the account as the password does, so treat it as one.
pub fn stretch(username: &str, password: &str) -> Result<Zeroizing<[u8; 32]>, LoginError> {
    stretch_with(username, password, SCRYPT_LOG_N)
}

fn stretch_with(
    username: &str,
    password: &str,
    log_n: u8,
) -> Result<Zeroizing<[u8; 32]>, LoginError> {
    let salt = tagged_hash(
        b"coordinator/login-salt/v1",
        username.to_lowercase().as_bytes(),
    );
    let params =
        Params::new(log_n, SCRYPT_R, SCRYPT_P, 32).map_err(|_| LoginError::KeyStretching)?;
    let mut stretched = Zeroizing::new([0u8; 32]);
    scrypt(password.as_bytes(), &salt[..], &params, &mut stretched[..])
        .map_err(|_| LoginError::KeyStretching)?;
    Ok(stretched)
}

fn tagged_hash(tag: &[u8], data: &[u8]) -> Zeroizing<[u8; 32]> {
    let tag_hash = Sha256::digest(tag);
    let mut hasher = Sha256::new();
    hasher.update(tag_hash);
    hasher.update(tag_hash);
    hasher.update(data);
    Zeroizing::new(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cheap scrypt cost so tests stay fast; production cost is covered by `stretch`.
    const TEST_LOG_N: u8 = 4;

    fn keys(username: &str, password: &str) -> LoginKeys {
        LoginKeys::from_stretched(&stretch_with(username, password, TEST_LOG_N).unwrap())
    }

    #[test]
    fn frozen_v1_login_restores_original_identity() {
        // Independent v1 vector: scrypt logN=4, r=8, p=1; nonce bytes 0..23;
        // raw Nostr secret scalar 1. Keep the ciphertext fixed across SDK upgrades.
        const SEALED: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXOq6VCBZ1XrPgjqhWb1+Z5JU5pTVYThyjHiP20osNioNqNotpMCMeLwUCKp1SEMLy";
        let login = keys("Alice", "pw");
        assert_eq!(
            login.auth_key_hex(),
            "6be6c5aef8ae3cf29c773e1878fa11594d40ed207086b5e25b3efe45b053464d"
        );

        let opened = login.open(SEALED).unwrap();
        let mut expected_secret = [0u8; 32];
        expected_secret[31] = 1;
        assert_eq!(opened.secret_key().to_secret_bytes(), expected_secret);
        assert_eq!(
            opened.public_key().to_hex(),
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        );
    }

    #[test]
    fn sealed_key_opens_with_same_login() {
        let nostr = Keys::generate();
        let sealed = keys("alice", "pw").seal(nostr.secret_key()).unwrap();
        let opened = keys("alice", "pw").open(&sealed).unwrap();
        assert_eq!(opened.public_key(), nostr.public_key());
    }

    #[test]
    fn wrong_password_or_username_is_rejected() {
        let sealed = keys("alice", "pw")
            .seal(Keys::generate().secret_key())
            .unwrap();
        assert!(matches!(
            keys("alice", "nope").open(&sealed),
            Err(LoginError::WrongPassword)
        ));
        assert!(matches!(
            keys("bob", "pw").open(&sealed),
            Err(LoginError::WrongPassword)
        ));
    }

    #[test]
    fn auth_key_reveals_nothing_about_vault_key() {
        let login = keys("alice", "pw");
        assert_ne!(*login.auth_key, *login.vault_key);
        assert_eq!(login.auth_key_hex().len(), 64);
    }

    #[test]
    fn username_is_case_insensitive_for_key_derivation() {
        assert_eq!(
            keys("Alice", "pw").auth_key_hex(),
            keys("alice", "pw").auth_key_hex()
        );
    }

    #[test]
    fn sealing_is_randomized() {
        let login = keys("alice", "pw");
        let secret = Keys::generate();
        assert_ne!(
            login.seal(secret.secret_key()).unwrap(),
            login.seal(secret.secret_key()).unwrap()
        );
    }

    #[test]
    fn malformed_sealed_key_is_rejected() {
        let login = keys("alice", "pw");
        for sealed in ["not base64!!", "", &BASE64.encode([0u8; 10])] {
            assert!(matches!(
                login.open(sealed),
                Err(LoginError::InvalidSealedKey)
            ));
        }
    }
}
