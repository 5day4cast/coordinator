use anyhow::Result;
use blake2::{Blake2b512, Digest as Blake2Digest};
use dlctix::{
    bitcoin::bip32::{ChainCode, ChildNumber, DerivationPath, Xpriv},
    secp::Scalar,
};
use nostr::{Keys, SecretKey};
use rand::RngCore;
use sha2::Sha256;
use std::str::FromStr;

/// A synthetic user with Nostr keys and a Bitcoin wallet key
#[derive(Clone)]
pub struct SynthUser {
    pub name: String,
    pub nostr_keys: Keys,
    pub master_xpriv: Xpriv,
}

impl SynthUser {
    /// Create a new synthetic user with random keys
    pub fn new_random(name: &str) -> Result<Self> {
        let nostr_keys = Keys::generate();
        let master_xpriv = generate_master_xpriv()?;

        Ok(Self {
            name: name.to_string(),
            nostr_keys,
            master_xpriv,
        })
    }

    /// Restore a synthetic user from a stored Nostr secret key
    pub fn from_secret_key(name: &str, secret_key_hex: &str) -> Result<Self> {
        let secret_key = SecretKey::from_hex(secret_key_hex)?;
        let nostr_keys = Keys::new(secret_key);

        // Derive the master xpriv deterministically from the nostr key
        let mut entropy = [0u8; 32];
        let mut hasher = Sha256::new();
        hasher.update(nostr_keys.secret_key().as_secret_bytes());
        hasher.update(b"synth-wallet-derivation");
        let hash = hasher.finalize();
        entropy.copy_from_slice(&hash);

        let secret_key = dlctix::bitcoin::secp256k1::SecretKey::from_slice(&entropy)
            .map_err(|e| anyhow::anyhow!("Invalid derived key: {}", e))?;

        let mut blake_hasher = Blake2b512::new();
        blake_hasher.update(&secret_key[..]);
        let blake_hash = blake_hasher.finalize();
        let mut chain_code = [0u8; 32];
        chain_code.copy_from_slice(&blake_hash[0..32]);

        let xpriv = Xpriv {
            network: dlctix::bitcoin::NetworkKind::Test,
            depth: 0,
            parent_fingerprint: Default::default(),
            chain_code: ChainCode::from(&chain_code),
            child_number: ChildNumber::from_normal_idx(0)
                .map_err(|e| anyhow::anyhow!("Invalid child number: {}", e))?,
            private_key: secret_key,
        };

        Ok(Self {
            name: name.to_string(),
            nostr_keys,
            master_xpriv: xpriv,
        })
    }

    /// Get the user's Nostr public key as hex
    pub fn nostr_pubkey_hex(&self) -> String {
        self.nostr_keys.public_key().to_hex()
    }

    /// Get the user's Nostr secret key as hex
    pub fn nostr_secret_key_hex(&self) -> String {
        self.nostr_keys.secret_key().to_secret_hex()
    }

    /// Derive the ephemeral keypair for one entry.
    ///
    /// Keyed by the entry, as the browser wallet's keys are: the coordinator refuses a payout hash
    /// it has seen before, so a player entering competition after competition needs a new key for
    /// each entry, not one per seat.
    pub fn derive_ephemeral_key(&self, entry_id: &uuid::Uuid) -> Result<EphemeralKey> {
        let digest = Sha256::digest(entry_id.as_bytes());
        let index = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) & 0x7fff_ffff;
        let path = format!("m/86'/0'/{}'/0/0", index);
        let path = DerivationPath::from_str(&path)
            .map_err(|e| anyhow::anyhow!("Invalid derivation path: {}", e))?;

        let secp = dlctix::bitcoin::secp256k1::Secp256k1::new();
        let child_xpriv = self
            .master_xpriv
            .derive_priv(&secp, &path)
            .map_err(|e| anyhow::anyhow!("Key derivation error: {}", e))?;

        let secret_bytes = child_xpriv.private_key.secret_bytes();
        let secret_scalar = Scalar::from_hex(&hex::encode(secret_bytes))
            .map_err(|e| anyhow::anyhow!("Failed to convert to scalar: {}", e))?;
        let pubkey = secret_scalar.base_point_mul();

        Ok(EphemeralKey {
            private_key_hex: hex::encode(secret_bytes),
            public_key: pubkey.to_string(),
            secret_bytes,
        })
    }
}

/// An ephemeral keypair derived for a specific DLC entry
pub struct EphemeralKey {
    pub private_key_hex: String,
    pub public_key: String,
    pub secret_bytes: [u8; 32],
}

/// Generate a random master extended private key
fn generate_master_xpriv() -> Result<Xpriv> {
    let mut entropy = [0u8; 32];
    rand::rng().fill_bytes(&mut entropy);

    let secret_key = dlctix::bitcoin::secp256k1::SecretKey::from_slice(&entropy)
        .map_err(|e| anyhow::anyhow!("Invalid random key: {}", e))?;

    let mut hasher = Blake2b512::new();
    hasher.update(&secret_key[..]);
    let hash = hasher.finalize();
    let mut chain_code = [0u8; 32];
    chain_code.copy_from_slice(&hash[0..32]);

    Ok(Xpriv {
        network: dlctix::bitcoin::NetworkKind::Test,
        depth: 0,
        parent_fingerprint: Default::default(),
        chain_code: ChainCode::from(&chain_code),
        child_number: ChildNumber::from_normal_idx(0)
            .map_err(|e| anyhow::anyhow!("Invalid child number: {}", e))?,
        private_key: secret_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The coordinator refuses a payout hash it has seen, so a player's next entry needs a new key
    /// even in the same seat of the next competition.
    #[test]
    fn each_entry_gets_its_own_key() {
        let user = SynthUser::new_random("alice").unwrap();
        let first = uuid::Uuid::now_v7();
        let second = uuid::Uuid::now_v7();

        let key = user.derive_ephemeral_key(&first).unwrap();
        assert_eq!(
            key.secret_bytes,
            user.derive_ephemeral_key(&first).unwrap().secret_bytes,
            "an entry's key is the same whenever it is derived"
        );
        assert_ne!(
            key.secret_bytes,
            user.derive_ephemeral_key(&second).unwrap().secret_bytes
        );
    }
}
