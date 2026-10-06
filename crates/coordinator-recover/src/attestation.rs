//! Finding the oracle's attestation without trusting where it came from.
//!
//! An attestation is the scalar whose point is one of the event's locking points, so any value
//! that opens a locking point of the signed contract is the attestation, whoever served it. That
//! lets this read it from the competition record, the oracle's API (`/oracle/events/{id}`) or the
//! oracle's Nostr events without knowing their exact format: it collects every 32-byte value
//! they hold (and the `s` half of every 64-byte signature) and keeps the one that opens a point.

use dlctix::secp::{MaybePoint, MaybeScalar};
use serde_json::{json, Value};

use crate::spec::KIND_APP_DATA;

/// NIP-88 attestation events.
pub const KIND_NIP88_ATTESTATION: u16 = 89;

/// The relay filters that may hold the attestation for oracle event `event_id`: the oracle's
/// kind 30078 event (`d` = `oracle:<event_id>`), and NIP-88 attestations naming the event.
pub fn relay_filters(event_id: &str) -> Vec<Value> {
    vec![
        json!({"kinds": [KIND_APP_DATA], "#d": [format!("oracle:{event_id}")]}),
        json!({"kinds": [KIND_NIP88_ATTESTATION], "#d": [event_id]}),
    ]
}

/// The first of `candidates` that opens one of `locking_points`.
pub fn select(candidates: &[MaybeScalar], locking_points: &[MaybePoint]) -> Option<MaybeScalar> {
    candidates
        .iter()
        .find(|candidate| locking_points.contains(&candidate.base_point_mul()))
        .copied()
}

/// Every value in a JSON document that could be an attestation.
pub fn candidates_in_json(value: &Value) -> Vec<MaybeScalar> {
    let mut found = Vec::new();
    collect(value, &mut found, 0);
    found
}

/// The same for an event's content, which may be JSON or a bare value.
pub fn candidates_in_text(text: &str) -> Vec<MaybeScalar> {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => candidates_in_json(&value),
        Err(_) => candidates_in_str(text.trim()),
    }
}

fn collect(value: &Value, found: &mut Vec<MaybeScalar>, depth: usize) {
    // Deep enough for any announcement or attestation; bounds work on hostile input.
    if depth > 16 || found.len() > 256 {
        return;
    }
    match value {
        Value::String(text) => found.extend(candidates_in_str(text)),
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect(item, found, depth + 1)),
        Value::Object(fields) => fields
            .values()
            .for_each(|item| collect(item, found, depth + 1)),
        _ => {}
    }
}

fn candidates_in_str(text: &str) -> Vec<MaybeScalar> {
    let bytes = match text.len() {
        64 | 128 => match hex::decode(text) {
            Ok(bytes) => bytes,
            Err(_) => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    // A 64-byte value is a Schnorr signature `R || s`; the attestation is `s`.
    let scalar = &bytes[bytes.len() - 32..];
    MaybeScalar::try_from(scalar).ok().into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dlctix::secp::Scalar;

    fn scalar(byte: u8) -> Scalar {
        Scalar::from_slice(&[byte; 32]).unwrap()
    }

    #[test]
    fn finds_the_attestation_wherever_it_is_and_only_if_it_opens_a_point() {
        let attestation = MaybeScalar::from(scalar(7));
        let other = MaybeScalar::from(scalar(9));
        let points = vec![
            scalar(3).base_point_mul().into(),
            attestation.base_point_mul(),
        ];
        let hex_attestation = hex::encode(attestation.serialize());

        // The oracle's API.
        let api = json!({"id": "x", "attestation": hex_attestation, "nonce_point": "02ab"});
        assert_eq!(
            select(&candidates_in_json(&api), &points),
            Some(attestation)
        );

        // A signature-shaped value, and bare content.
        let signature = format!("{}{}", "11".repeat(32), hex_attestation);
        assert_eq!(
            select(
                &candidates_in_text(&json!({"sig": signature}).to_string()),
                &points
            ),
            Some(attestation)
        );
        assert_eq!(
            select(&candidates_in_text(&hex_attestation), &points),
            Some(attestation)
        );

        // A value that opens no locking point is never used.
        let wrong = json!({"attestation": hex::encode(other.serialize())});
        assert_eq!(select(&candidates_in_json(&wrong), &points), None);
        assert_eq!(
            select(&candidates_in_json(&json!({"attestation": null})), &points),
            None
        );
    }
}
