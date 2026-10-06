//! The wallet seed and the keys it derives.
//!
//! This repeats the browser wallet's derivation (`coordinator-wasm/src/wallet/keys.rs`) byte for
//! byte: an entry key and payout preimage are BIP-340 style tagged hashes of the seed, the network
//! and the entry id. The frozen vector in the tests is the one the wallet's own tests pin, so the
//! two cannot drift apart unnoticed.

use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::{All, SecretKey};
use bitcoin::{Network, XOnlyPublicKey};
use dlctix::secp::{Point, Scalar};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{Error, Result};

const SEED_LEN: usize = 32;
const BACKUP_PREFIX: &str = "coordinator-wallet-v1:";
const ENTRY_KEY_TAG: &[u8] = b"coordinator/entry-key/v1";
const PAYOUT_PREIMAGE_TAG: &[u8] = b"coordinator/payout-preimage/v1";

/// The browser wallet's 32-byte seed. Not `Clone`, `Debug` or `Serialize`; erased on drop.
pub struct WalletSeed(Zeroizing<[u8; SEED_LEN]>);

impl WalletSeed {
    /// The seed from the wallet backup's plaintext, `coordinator-wallet-v1:<hex>`.
    pub fn from_backup(backup: &str) -> Result<Self> {
        let hex_seed = backup
            .strip_prefix(BACKUP_PREFIX)
            .ok_or(Error::Decrypt("wallet backup"))?;
        let mut seed = Zeroizing::new([0u8; SEED_LEN]);
        hex::decode_to_slice(hex_seed, &mut seed[..])
            .map_err(|_| Error::Decrypt("wallet backup"))?;
        Ok(Self(seed))
    }

    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: [u8; SEED_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn entry_key(&self, network: Network, entry_id: Uuid) -> Result<EntryKey> {
        let secret = self.derive(ENTRY_KEY_TAG, network, entry_id);
        // Fails only if the hash is zero or >= the curve order (p ~ 2^-128).
        let secret_key = SecretKey::from_slice(&secret[..])
            .map_err(|_| Error::Invalid(format!("entry key for {entry_id}")))?;
        let secp = Secp256k1::new();
        let point = Point::from_slice(&secret_key.public_key(&secp).serialize())
            .map_err(|_| Error::Invalid(format!("entry key for {entry_id}")))?;
        Ok(EntryKey {
            secret,
            point,
            payout_preimage: self.derive(PAYOUT_PREIMAGE_TAG, network, entry_id),
            secp,
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

/// One entry's key and payout preimage. Neither `Clone` nor `Debug`; erased on drop.
pub struct EntryKey {
    secret: Zeroizing<[u8; 32]>,
    point: Point,
    payout_preimage: Zeroizing<[u8; 32]>,
    secp: Secp256k1<All>,
}

impl EntryKey {
    /// The entry pubkey as dlctix and the coordinator encode it (compressed).
    pub fn point(&self) -> Point {
        self.point
    }

    pub fn xonly(&self) -> XOnlyPublicKey {
        XOnlyPublicKey::from_slice(&self.point.serialize_xonly())
            .expect("a valid point has a valid x-only key")
    }

    /// Whether `recorded`, hex of the compressed or x-only key, is this key.
    pub fn matches(&self, recorded: &str) -> bool {
        let recorded = recorded.trim().to_ascii_lowercase();
        recorded == hex::encode(self.point.serialize())
            || recorded == hex::encode(self.point.serialize_xonly())
    }

    pub fn scalar(&self) -> Scalar {
        Scalar::from_slice(&self.secret[..]).expect("a valid secret key is a nonzero scalar")
    }

    pub fn keypair(&self) -> Keypair {
        Keypair::from_seckey_slice(&self.secp, &self.secret[..])
            .expect("a valid secret key makes a keypair")
    }

    pub fn payout_hash(&self) -> [u8; 32] {
        Sha256::digest(&self.payout_preimage[..]).into()
    }

    #[cfg(test)]
    pub(crate) fn payout_preimage(&self) -> [u8; 32] {
        *self.payout_preimage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The browser wallet's frozen vector (`frozen_nip44_v2_backup_restores_original_entry_keys`):
    /// seed bytes 0..31, signet, entry 00112233-4455-6677-8899-aabbccddeeff.
    #[test]
    fn derives_the_browser_wallets_frozen_vector() {
        let seed: [u8; 32] = std::array::from_fn(|i| i as u8);
        let backup = format!("coordinator-wallet-v1:{}", hex::encode(seed));
        let seed = WalletSeed::from_backup(&backup).unwrap();
        let entry_id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let key = seed.entry_key(Network::Signet, entry_id).unwrap();

        assert_eq!(
            hex::encode(key.point().serialize()),
            "02db32ae6adf4d575228bc8de8a99d3f856855bbe2d8d9ff84e9a1c81d9ea73a03"
        );
        assert_eq!(
            hex::encode(&key.secret[..]),
            "f53d104768bda153f481882115a011185c917998b2073bb45d6694eb0f9f1aed"
        );
        assert_eq!(
            hex::encode(key.payout_preimage()),
            "f0856215fa2ee82c74c788deba04dfb38d068058617409973a5f007ee1327299"
        );
        assert_eq!(
            hex::encode(key.payout_hash()),
            "33ed8f79efa5a15b4c513b3ed5a22122a7d7eb2663687e3f555561fe13641977"
        );
        assert!(key.matches("02db32ae6adf4d575228bc8de8a99d3f856855bbe2d8d9ff84e9a1c81d9ea73a03"));
        assert!(key.matches("db32ae6adf4d575228bc8de8a99d3f856855bbe2d8d9ff84e9a1c81d9ea73a03"));
        assert!(!key.matches("03db32ae6adf4d575228bc8de8a99d3f856855bbe2d8d9ff84e9a1c81d9ea73a03"));
        assert_eq!(key.scalar().base_point_mul(), key.point());
        assert_eq!(key.keypair().x_only_public_key().0, key.xonly());
    }

    #[test]
    fn rejects_malformed_backup() {
        for backup in [
            "",
            "deadbeef",
            "coordinator-wallet-v1:zz",
            "coordinator-wallet-v1:00",
        ] {
            assert!(WalletSeed::from_backup(backup).is_err());
        }
    }
}
