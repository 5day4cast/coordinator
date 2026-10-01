//! Authenticated requests to the independently trusted, durable payout witness.
use anyhow::{ensure, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;
use zeroize::Zeroizing;

const REQUEST_DOMAIN: &[u8] = b"coordinator/payout-witness/request/v1\0";
const RESPONSE_DOMAIN: &[u8] = b"coordinator/payout-witness/response/v1\0";
pub const MAX_REQUEST_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub claim_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub claim: Claim,
    pub payment_hash: [u8; 32],
    pub executing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub ledger_id: Uuid,
    pub nonce: [u8; 32],
    pub reservation: Reservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Occupancy {
    pub payment_hashes: u64,
    pub released_entries: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum WitnessError {
    #[error("payout witness rejected conflicting claim ownership")]
    Conflict,
    #[error("payout witness storage capacity exhausted")]
    Capacity,
    #[error("payout witness unavailable; reconcile or retry the same claim")]
    Unavailable,
    #[error("payout witness is not configured")]
    NotConfigured,
    #[error("payout witness ledger identity differs")]
    WrongLedger,
    #[error("payout witness response authentication failed")]
    Authentication,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub request: Request,
    pub result: Result<Occupancy, WitnessError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authenticated<T> {
    pub payload: T,
    pub mac: String,
}

/// Provision this secret separately to the trusted witness and measured enclave.
/// It must never be available to the untrusted HTTPS relay.
pub struct AuthenticationKey(Zeroizing<[u8; 32]>);

impl AuthenticationKey {
    pub fn from_hex(value: &str) -> Result<Self> {
        let bytes = Zeroizing::new(hex::decode(value.trim())?);
        ensure!(bytes.len() == 32, "Witness key must contain 32 bytes");
        let mut key = Zeroizing::new([0; 32]);
        key.copy_from_slice(&bytes);
        Ok(Self(key))
    }

    fn mac(&self, domain: &[u8], payload: &impl Serialize) -> Result<Hmac<Sha256>> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.as_ref())?;
        mac.update(domain);
        mac.update(&serde_json::to_vec(payload)?);
        Ok(mac)
    }

    pub fn sign_request(&self, payload: Request) -> Result<Authenticated<Request>> {
        let mac = hex::encode(self.mac(REQUEST_DOMAIN, &payload)?.finalize().into_bytes());
        Ok(Authenticated { payload, mac })
    }

    pub fn verify_request(&self, request: &Authenticated<Request>) -> Result<()> {
        self.mac(REQUEST_DOMAIN, &request.payload)?
            .verify_slice(&hex::decode(&request.mac)?)?;
        Ok(())
    }

    pub fn sign_receipt(&self, payload: Receipt) -> Result<Authenticated<Receipt>> {
        let mac = hex::encode(self.mac(RESPONSE_DOMAIN, &payload)?.finalize().into_bytes());
        Ok(Authenticated { payload, mac })
    }

    pub fn verify_receipt(
        &self,
        receipt: &Authenticated<Receipt>,
        request: &Request,
    ) -> Result<()> {
        self.mac(RESPONSE_DOMAIN, &receipt.payload)?
            .verify_slice(&hex::decode(&receipt.mac)?)?;
        ensure!(
            receipt.payload.request == *request,
            "Witness receipt belongs to another request"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_authenticate_the_exact_request_and_fresh_challenge() {
        let key = AuthenticationKey::from_hex(&hex::encode([7; 32])).unwrap();
        let request = Request {
            ledger_id: Uuid::now_v7(),
            nonce: [1; 32],
            reservation: Reservation {
                claim: Claim {
                    session_id: Uuid::now_v7(),
                    user_id: Uuid::now_v7(),
                    claim_id: Uuid::now_v7(),
                },
                payment_hash: [2; 32],
                executing: false,
            },
        };
        let signed = key.sign_request(request.clone()).unwrap();
        key.verify_request(&signed).unwrap();
        let mut changed = signed.clone();
        changed.payload.reservation.executing = true;
        assert!(key.verify_request(&changed).is_err());
        let receipt = key
            .sign_receipt(Receipt {
                request: request.clone(),
                result: Ok(Occupancy {
                    payment_hashes: 1,
                    released_entries: 0,
                }),
            })
            .unwrap();
        key.verify_receipt(&receipt, &request).unwrap();
        let mut next = request.clone();
        next.nonce = [3; 32];
        assert!(key.verify_receipt(&receipt, &next).is_err());
        let other = AuthenticationKey::from_hex(&hex::encode([8; 32])).unwrap();
        assert!(other.verify_request(&signed).is_err());
        assert!(other.verify_receipt(&receipt, &request).is_err());
    }
}
