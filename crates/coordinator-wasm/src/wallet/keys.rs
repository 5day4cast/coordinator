//! Wallet key material.
//!
//! The wallet is a 32-byte seed. Every entry key is a BIP-340 style tagged
//! hash of (seed, network, entry id), so keys are bound to one entry, never
//! reused, and recoverable from the seed and the entry id the server stores.
//! The payout preimage is Keymeld's `derive_payout_preimage(entry key)`, so
//! the enclave holding the key can release the same value (see
//! `docs/PAYOUT_ESCROW.md`). Nothing here is `Clone`, `Debug` or `Serialize`;
//! buffers are erased on drop. `SecretKey` and `Scalar` are `Copy`, so
//! library-internal stack copies are outside our control.

use super::WalletError;
use coordinator_core::derive_payout_preimage;
use dlctix::{
    bitcoin::{
        secp256k1::{ecdsa::Signature, All, Message, PublicKey, Secp256k1, SecretKey},
        Network,
    },
    secp::{Point, Scalar},
};
use rand::RngCore;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

const SEED_LEN: usize = 32;
/// Versioned plaintext of the encrypted seed backup.
const BACKUP_PREFIX: &str = "coordinator-wallet-v1:";
const ENTRY_KEY_TAG: &[u8] = b"coordinator/entry-key/v1";

pub struct WalletSeed(Zeroizing<[u8; SEED_LEN]>);

impl WalletSeed {
    pub fn generate() -> Self {
        let mut seed = Zeroizing::new([0u8; SEED_LEN]);
        rand::rng().fill_bytes(&mut seed[..]);
        Self(seed)
    }

    pub fn from_backup(backup: &str) -> Result<Self, WalletError> {
        let hex_seed = backup
            .strip_prefix(BACKUP_PREFIX)
            .ok_or(WalletError::InvalidBackup)?;
        let mut seed = Zeroizing::new([0u8; SEED_LEN]);
        hex::decode_to_slice(hex_seed, &mut seed[..]).map_err(|_| WalletError::InvalidBackup)?;
        Ok(Self(seed))
    }

    /// Plaintext for the encrypted backup. Only ever passed to encryption.
    pub fn to_backup(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("{BACKUP_PREFIX}{}", hex::encode(&self.0[..])))
    }

    pub fn entry_key(
        &self,
        secp: &Secp256k1<All>,
        network: Network,
        entry_id: Uuid,
    ) -> Result<EntryKey, WalletError> {
        let bytes = self.derive(ENTRY_KEY_TAG, network, entry_id);
        // Fails only if the hash is zero or >= the curve order (p ~ 2^-128).
        let secret =
            SecretKey::from_slice(&bytes[..]).map_err(|_| WalletError::DerivationFailed)?;
        Ok(EntryKey {
            pubkey: secret.public_key(secp),
            secret,
            payout_preimage: Zeroizing::new(derive_payout_preimage(&bytes)),
        })
    }

    fn derive(&self, tag: &[u8], network: Network, entry_id: Uuid) -> Zeroizing<[u8; 32]> {
        let tag_hash = Sha256::digest(tag);
        let mut hasher = Sha256::new();
        hasher.update(tag_hash);
        hasher.update(tag_hash);
        hasher.update(&self.0[..]);
        hasher.update(network.magic().to_bytes());
        hasher.update(entry_id.as_bytes());
        Zeroizing::new(hasher.finalize().into())
    }
}

/// The ephemeral key and payout preimage a player uses for one entry.
pub struct EntryKey {
    secret: SecretKey,
    pub pubkey: PublicKey,
    payout_preimage: Zeroizing<[u8; 32]>,
}

impl EntryKey {
    /// The pubkey as dlctix and the coordinator encode it.
    pub fn point(&self) -> Point {
        Point::from_slice(&self.pubkey.serialize()).expect("a valid PublicKey is a valid point")
    }

    pub fn scalar(&self) -> Scalar {
        Scalar::from_slice(&self.secret_bytes()[..])
            .expect("a valid SecretKey is a valid nonzero scalar")
    }

    pub fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.secret_bytes())
    }

    pub fn secret_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex::encode(&self.secret_bytes()[..]))
    }

    pub fn payout_preimage_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex::encode(&self.payout_preimage[..]))
    }

    pub fn payout_hash(&self) -> [u8; 32] {
        Sha256::digest(&self.payout_preimage[..]).into()
    }

    pub fn sign_ecdsa(&self, secp: &Secp256k1<All>, message: &Message) -> Signature {
        secp.sign_ecdsa(message, &self.secret)
    }
}

impl Drop for EntryKey {
    fn drop(&mut self) {
        self.secret.non_secure_erase();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seed: &WalletSeed, network: Network, id: Uuid) -> EntryKey {
        seed.entry_key(&Secp256k1::new(), network, id).unwrap()
    }

    #[test]
    fn entry_keys_are_stable_per_entry() {
        let seed = WalletSeed::generate();
        let id = Uuid::now_v7();
        let first = entry(&seed, Network::Signet, id);
        let again = entry(&seed, Network::Signet, id);

        assert_eq!(first.pubkey, again.pubkey);
        assert_eq!(first.payout_hash(), again.payout_hash());
    }

    #[test]
    fn entry_keys_differ_by_entry_network_and_seed() {
        let seed = WalletSeed::generate();
        let id = Uuid::now_v7();
        let base = entry(&seed, Network::Signet, id).pubkey;

        assert_ne!(base, entry(&seed, Network::Signet, Uuid::now_v7()).pubkey);
        assert_ne!(base, entry(&seed, Network::Bitcoin, id).pubkey);
        assert_ne!(
            base,
            entry(&WalletSeed::generate(), Network::Signet, id).pubkey
        );
    }

    #[test]
    fn payout_preimage_is_the_enclave_derivation_of_the_entry_key() {
        let key = entry(&WalletSeed::generate(), Network::Signet, Uuid::now_v7());
        assert_eq!(
            *key.payout_preimage,
            derive_payout_preimage(&key.secret_bytes())
        );
        assert_ne!(&key.payout_preimage[..], &key.secret_bytes()[..]);
        assert_eq!(
            key.payout_hash(),
            <[u8; 32]>::from(Sha256::digest(&key.payout_preimage[..]))
        );
    }

    #[test]
    fn point_matches_pubkey_encoding() {
        let key = entry(&WalletSeed::generate(), Network::Signet, Uuid::now_v7());
        assert_eq!(key.point().to_string(), key.pubkey.to_string());
        assert_eq!(key.scalar().base_point_mul(), key.point());
    }

    #[test]
    fn backup_round_trips() {
        let seed = WalletSeed::generate();
        let restored = WalletSeed::from_backup(&seed.to_backup()).unwrap();
        assert_eq!(*restored.0, *seed.0);
    }

    #[test]
    fn rejects_malformed_backup() {
        for backup in [
            "",
            "deadbeef",
            "coordinator-wallet-v1:zz",
            "coordinator-wallet-v1:00",
        ] {
            assert!(matches!(
                WalletSeed::from_backup(backup),
                Err(WalletError::InvalidBackup)
            ));
        }
    }
}
