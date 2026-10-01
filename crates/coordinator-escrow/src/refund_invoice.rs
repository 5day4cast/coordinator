//! Domain-separated authorization of an invoice resolved inside the verifier.
//! The policy digest binds the participant, escrow, session, and authorized address.
use keymeld_core::{
    escrow::{self, protocol::Payload, SignedEscrowPolicy},
    KeyMeldError,
};

pub fn digest(policy: &SignedEscrowPolicy, invoice: &str) -> Result<[u8; 32], KeyMeldError> {
    Ok(escrow::sha256(
        Payload::encode(&(
            "coordinator/refund-invoice/v1",
            policy.policy.digest()?,
            invoice,
        ))?
        .as_bytes(),
    ))
}

pub fn verify(
    policy: &SignedEscrowPolicy,
    invoice: &str,
    signature: &str,
) -> Result<(), KeyMeldError> {
    let invalid = |error: String| KeyMeldError::ValidationError(error);
    let signature: [u8; 64] = hex::decode(signature)
        .map_err(|e| invalid(e.to_string()))?
        .try_into()
        .map_err(|_| invalid("Invalid refund invoice signature length".into()))?;
    let key = secp256k1::PublicKey::from_slice(policy.policy.participant_public_key.as_bytes())
        .map_err(|e| invalid(e.to_string()))?;
    secp256k1::Secp256k1::verification_only()
        .verify_schnorr(
            &secp256k1::schnorr::Signature::from_byte_array(signature),
            &digest(policy, invoice)?,
            &key.x_only_public_key().0,
        )
        .map_err(|_| invalid("Refund invoice lacks recipient authorization".into()))
}
