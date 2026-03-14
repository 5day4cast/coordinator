use sha2::{Digest, Sha256};

/// Generate a payout preimage from an ephemeral private key.
/// The preimage is sha256(secret_bytes), and the hash is sha256(preimage).
pub fn generate_payout_pair(secret_bytes: &[u8; 32]) -> (String, String) {
    let mut hasher = Sha256::new();
    hasher.update(secret_bytes);
    let preimage: [u8; 32] = hasher.finalize().into();

    let mut hasher = Sha256::new();
    hasher.update(preimage);
    let hash: [u8; 32] = hasher.finalize().into();

    (hex::encode(preimage), hex::encode(hash))
}
