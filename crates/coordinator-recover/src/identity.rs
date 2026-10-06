//! The player's Nostr key: it finds the records (the blind tag) and decrypts them.

use nostr::nips::nip44;
use nostr::{Keys, PublicKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{Error, Result};

const BLIND_DOMAIN: &[u8] = b"coordinator-recovery/v1";

/// The player's Nostr key pair. Neither `Clone` nor `Debug`, so it is never printed.
pub struct Identity {
    keys: Keys,
}

impl Identity {
    /// From an `nsec1…` or hex secret key.
    pub fn from_nsec(nsec: &str) -> Result<Self> {
        let keys = Keys::parse(nsec.trim()).map_err(|_| Error::Nsec)?;
        Ok(Self { keys })
    }

    pub fn pubkey(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// The tag the coordinator puts on this player's records, so relays can serve them without
    /// learning whose they are: `sha256("coordinator-recovery/v1" || coordinator || user)`.
    pub fn blind_tag(&self, coordinator: &PublicKey) -> String {
        blind_tag(coordinator, &self.pubkey())
    }

    /// Decrypt NIP-44 `payload` sent to this player by `sender`.
    pub fn decrypt(
        &self,
        sender: &PublicKey,
        payload: &str,
        what: &'static str,
    ) -> Result<Zeroizing<String>> {
        nip44::decrypt(self.keys.secret_key(), sender, payload)
            .map(Zeroizing::new)
            .map_err(|_| Error::Decrypt(what))
    }

    /// Decrypt NIP-44 `payload` this player encrypted to themselves, as the wallet backup is.
    pub fn decrypt_own(&self, payload: &str, what: &'static str) -> Result<Zeroizing<String>> {
        self.decrypt(&self.pubkey(), payload, what)
    }
}

/// See [`Identity::blind_tag`].
pub fn blind_tag(coordinator: &PublicKey, user: &PublicKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(BLIND_DOMAIN);
    hasher.update(coordinator.to_bytes());
    hasher.update(user.to_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blind_tag_hashes_domain_coordinator_and_user() {
        let coordinator = Keys::parse(&"02".repeat(32)).unwrap().public_key();
        let user = Identity::from_nsec(&"01".repeat(32)).unwrap();
        let mut preimage = BLIND_DOMAIN.to_vec();
        preimage.extend_from_slice(&coordinator.to_bytes());
        preimage.extend_from_slice(&user.pubkey().to_bytes());
        assert_eq!(
            user.blind_tag(&coordinator),
            hex::encode(Sha256::digest(&preimage))
        );
        // Another coordinator gives the same player another tag.
        let other = Keys::parse(&"03".repeat(32)).unwrap().public_key();
        assert_ne!(user.blind_tag(&coordinator), user.blind_tag(&other));
    }

    /// The shared spec's vector: coordinator secret key 1, player secret key 2.
    #[test]
    fn blind_tag_matches_the_spec_vector() {
        let coordinator = Keys::parse(&format!("{:064x}", 1)).unwrap().public_key();
        let player = Identity::from_nsec(&format!("{:064x}", 2)).unwrap();
        assert_eq!(
            coordinator.to_hex(),
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        );
        assert_eq!(
            player.pubkey().to_hex(),
            "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5"
        );
        assert_eq!(
            player.blind_tag(&coordinator),
            "f5b81b1079306318eceb3a9a65e9e0144bd0757b1f3304e1b5d5d549328b1bc9"
        );
    }

    #[test]
    fn rejects_a_bad_nsec() {
        assert!(Identity::from_nsec("nsec1notakey").is_err());
        assert!(Identity::from_nsec("").is_err());
    }
}
