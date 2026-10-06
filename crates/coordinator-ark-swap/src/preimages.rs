//! Invoice preimages, encrypted at rest.
//!
//! A swap's preimage settles a payer's HTLC and a refund's preimage claims a VTXO, so a copy of
//! the database must not be enough to take either. Each is sealed with AES-256-GCM under a key
//! derived from the wallet key, which already has to be kept and backed up: losing it loses the
//! wallet's coins, so it adds no secret to keep. The sealed form is a random 96-bit nonce, the
//! ciphertext, then the tag. The associated data names the row and its payment hash, so a sealed
//! preimage moved to another row does not open.

use aes_gcm::aead::{Aead, Key, KeyInit, Nonce, Payload};
use aes_gcm::Aes256Gcm;
use anyhow::Context;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const KEY_TAG: &[u8] = b"ark-swapd/preimage-key/v1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// The key preimages are sealed under. It is never printed.
#[derive(Clone)]
pub struct PreimageKey(Key<Aes256Gcm>);

impl std::fmt::Debug for PreimageKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PreimageKey(..)")
    }
}

/// Which table a preimage belongs to, so a swap's sealed preimage never opens as a refund's.
#[derive(Clone, Copy)]
pub enum Row {
    Swap,
    Refund,
}

impl PreimageKey {
    /// Derive the key from the wallet's secret key, with a BIP-340 style tagged hash so it is
    /// unrelated to anything else made from that key.
    pub fn from_wallet_secret(secret: &[u8; 32]) -> Self {
        let tag = Sha256::digest(KEY_TAG);
        let key: [u8; 32] = Sha256::new()
            .chain_update(tag)
            .chain_update(tag)
            .chain_update(secret)
            .finalize()
            .into();
        Self(Key::<Aes256Gcm>::from(key))
    }

    /// Seal `preimage` for the row `id` whose invoice pays to `payment_hash` (hex).
    pub fn seal(
        &self,
        row: Row,
        id: Uuid,
        payment_hash: &str,
        preimage: &[u8; 32],
    ) -> anyhow::Result<Vec<u8>> {
        let nonce: [u8; NONCE_LEN] = rand08::random();
        let aad = associated_data(row, id, payment_hash);
        let ciphertext = Aes256Gcm::new(&self.0)
            .encrypt(
                &Nonce::<Aes256Gcm>::from(nonce),
                Payload {
                    msg: preimage,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot seal the preimage of {id}"))?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    /// Open a preimage sealed for the row `id` whose invoice pays to `payment_hash` (hex).
    pub fn open(
        &self,
        row: Row,
        id: Uuid,
        payment_hash: &str,
        sealed: &[u8],
    ) -> anyhow::Result<[u8; 32]> {
        anyhow::ensure!(
            sealed.len() == NONCE_LEN + 32 + TAG_LEN,
            "the sealed preimage of {id} is {} bytes",
            sealed.len()
        );
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce length");
        let aad = associated_data(row, id, payment_hash);
        let preimage = Aes256Gcm::new(&self.0)
            .decrypt(
                &Nonce::<Aes256Gcm>::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| {
                anyhow::anyhow!("the sealed preimage of {id} does not open with this wallet key")
            })?;
        preimage
            .try_into()
            .ok()
            .with_context(|| format!("the sealed preimage of {id} is not 32 bytes"))
    }
}

impl PreimageKey {
    /// A row's preimage, hex: the sealed copy if it has one, and the plaintext only for a row
    /// not yet sealed. Either way it must pay to the row's payment hash (hex).
    pub fn stored(
        &self,
        row: Row,
        id: Uuid,
        payment_hash: &str,
        sealed: Option<Vec<u8>>,
        plaintext: Option<String>,
    ) -> anyhow::Result<Option<String>> {
        let preimage = match (sealed, plaintext.filter(|plaintext| !plaintext.is_empty())) {
            (Some(sealed), _) => self.open(row, id, payment_hash, &sealed)?,
            (None, Some(plaintext)) => from_hex(id, &plaintext)?,
            (None, None) => return Ok(None),
        };
        checked(id, payment_hash, &preimage).map(Some)
    }
}

/// A plaintext preimage, hex. The error does not repeat it.
pub fn from_hex(id: Uuid, preimage: &str) -> anyhow::Result<[u8; 32]> {
    hex::decode(preimage)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .with_context(|| format!("the stored preimage of {id} is not 32 bytes of hex"))
}

fn associated_data(row: Row, id: Uuid, payment_hash: &str) -> Vec<u8> {
    let table = match row {
        Row::Swap => "swap",
        Row::Refund => "refund",
    };
    format!("ark-swapd/preimage/v1/{table}/{id}/{payment_hash}").into_bytes()
}

/// `preimage` as hex, refused unless it pays to `payment_hash` (hex). A preimage that does not
/// match its row is never used: the row is corrupt or was opened with the wrong key.
pub fn checked(id: Uuid, payment_hash: &str, preimage: &[u8; 32]) -> anyhow::Result<String> {
    let hash = hex::encode(Sha256::digest(preimage));
    anyhow::ensure!(
        hash.eq_ignore_ascii_case(payment_hash),
        "the stored preimage of {id} does not pay to its payment hash {payment_hash}"
    );
    Ok(hex::encode(preimage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(preimage: &[u8; 32]) -> String {
        hex::encode(Sha256::digest(preimage))
    }

    #[test]
    fn a_sealed_preimage_opens_only_for_its_own_row_and_key() {
        let key = PreimageKey::from_wallet_secret(&[1u8; 32]);
        let preimage = [7u8; 32];
        let hash = hash_of(&preimage);
        let id = Uuid::now_v7();
        let sealed = key.seal(Row::Swap, id, &hash, &preimage).unwrap();
        assert_eq!(sealed.len(), NONCE_LEN + 32 + TAG_LEN);
        assert!(!sealed.windows(32).any(|window| window == preimage));
        assert_eq!(key.open(Row::Swap, id, &hash, &sealed).unwrap(), preimage);

        // A fresh nonce each time.
        assert_ne!(sealed, key.seal(Row::Swap, id, &hash, &preimage).unwrap());

        // Another wallet's key, another row, another table or another hash: it does not open.
        let other_key = PreimageKey::from_wallet_secret(&[2u8; 32]);
        assert!(other_key.open(Row::Swap, id, &hash, &sealed).is_err());
        assert!(key.open(Row::Swap, Uuid::now_v7(), &hash, &sealed).is_err());
        assert!(key.open(Row::Refund, id, &hash, &sealed).is_err());
        assert!(key
            .open(Row::Swap, id, &hash_of(&[8u8; 32]), &sealed)
            .is_err());

        // Nor once tampered with or cut short.
        let mut tampered = sealed.clone();
        tampered[NONCE_LEN] ^= 1;
        assert!(key.open(Row::Swap, id, &hash, &tampered).is_err());
        assert!(key
            .open(Row::Swap, id, &hash, &sealed[..sealed.len() - 1])
            .is_err());
    }

    #[test]
    fn the_key_is_derived_from_the_wallet_key_and_never_printed() {
        let secret = [1u8; 32];
        let key = PreimageKey::from_wallet_secret(&secret);
        let again = PreimageKey::from_wallet_secret(&secret);
        assert_eq!(key.0, again.0, "the same key after a restart");
        assert_ne!(key.0.as_slice(), &secret[..]);
        assert_eq!(format!("{key:?}"), "PreimageKey(..)");
    }

    #[test]
    fn a_preimage_that_does_not_pay_to_its_hash_is_refused() {
        let id = Uuid::now_v7();
        let preimage = [7u8; 32];
        assert_eq!(
            checked(id, &hash_of(&preimage), &preimage).unwrap(),
            hex::encode(preimage)
        );
        assert!(checked(id, &hash_of(&[8u8; 32]), &preimage).is_err());
    }
}
