//! Authenticated, compressed checkpoint parts. The manifest is sealed at each CAS version;
//! unchanged parts are retained without rewriting their ciphertext.
use super::confidential_store::{failure, ProtocolState};
use super::KeymeldError;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use hmac::{Hmac, Mac};
use keymeld_core::{
    crypto::{EncryptedData, SessionSecret},
    SessionId,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
};
use zeroize::Zeroizing;

#[path = "checkpoint_array.rs"]
mod checkpoint_array;

const PART_BYTES: usize = 32 * 1024;
const MAX_STATE_BYTES: usize = 512 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub(super) enum Manifest {
    Part([u8; 32]),
    Object(BTreeMap<String, Manifest>),
    Array(Vec<Manifest>),
}

pub(super) struct Parts {
    pub manifest: Manifest,
    pub bodies: BTreeMap<[u8; 32], Vec<u8>>,
}

fn context(session: &SessionId, digest: &[u8; 32]) -> String {
    format!(
        "coordinator-protocol-part-v1/{session}/{}",
        hex::encode(digest)
    )
}

fn part_digest(key: &SessionSecret, session: &SessionId, text: &[u8]) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts a session key");
    mac.update(b"coordinator-protocol-content-v1/");
    mac.update(session.to_string().as_bytes());
    mac.update(text);
    mac.finalize().into_bytes().into()
}

impl Parts {
    pub fn encode(
        key: &SessionSecret,
        session: &SessionId,
        state: &ProtocolState,
    ) -> Result<Self, KeymeldError> {
        let json = Zeroizing::new(
            serde_json::to_string(state).map_err(|error| failure(error.to_string()))?,
        );
        crate::metrics::checkpoint_bytes("parts", "encode", json.len());
        if json.len() > MAX_STATE_BYTES {
            return Err(failure("Confidential checkpoint exceeds size limit"));
        }
        let raw: &RawValue =
            serde_json::from_str(&json).map_err(|error| failure(error.to_string()))?;
        let mut bodies = BTreeMap::new();
        let manifest = split(key, session, raw, &mut bodies)?;
        Ok(Self { manifest, bodies })
    }
}

fn split(
    key: &SessionSecret,
    session: &SessionId,
    raw: &RawValue,
    bodies: &mut BTreeMap<[u8; 32], Vec<u8>>,
) -> Result<Manifest, KeymeldError> {
    let text = raw.get();
    if text.len() > PART_BYTES {
        if text.starts_with('{') {
            let object: BTreeMap<String, &RawValue> =
                serde_json::from_str(text).map_err(|error| failure(error.to_string()))?;
            let parts = object
                .into_iter()
                .map(|(name, value)| Ok((name, split(key, session, value, bodies)?)))
                .collect::<Result<_, KeymeldError>>()?;
            return Ok(Manifest::Object(parts));
        }
        if text.starts_with('[') {
            // Binary vectors serialize as arrays of scalars. Keep them as one compressed
            // part. Classify without a Vec<&RawValue>: that temporary index costs two
            // machine words per byte, before compression or encryption even begins.
            if checkpoint_array::has_nested_values(text)
                .map_err(|error| failure(error.to_string()))?
            {
                let array: Vec<&RawValue> =
                    serde_json::from_str(text).map_err(|error| failure(error.to_string()))?;
                let parts = array
                    .into_iter()
                    .map(|value| split(key, session, value, bodies))
                    .collect::<Result<_, _>>()?;
                return Ok(Manifest::Array(parts));
            }
        }
    }
    // Bind the content address to this secret and session; equal public scalar values
    // cannot be guessed from hashes or correlated between sessions.
    let digest = part_digest(key, session, text.as_bytes());
    if let std::collections::btree_map::Entry::Vacant(entry) = bodies.entry(digest) {
        let mut compressor = GzEncoder::new(Vec::new(), Compression::fast());
        compressor
            .write_all(text.as_bytes())
            .map_err(|error| failure(error.to_string()))?;
        let compressed = Zeroizing::new(
            compressor
                .finish()
                .map_err(|error| failure(error.to_string()))?,
        );
        let encrypted = key
            .encrypt(&compressed, &context(session, &digest))
            .and_then(|value| value.to_bytes())
            .map_err(|_| failure("Cannot encrypt confidential checkpoint part"))?;
        entry.insert(encrypted);
    }
    Ok(Manifest::Part(digest))
}

impl Manifest {
    pub fn digests(&self, output: &mut BTreeSet<[u8; 32]>) {
        match self {
            Self::Part(digest) => {
                output.insert(*digest);
            }
            Self::Object(values) => {
                for value in values.values() {
                    value.digests(output);
                }
            }
            Self::Array(values) => {
                for value in values {
                    value.digests(output);
                }
            }
        }
    }

    pub fn decode(
        &self,
        key: &SessionSecret,
        session: &SessionId,
        bodies: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<ProtocolState, KeymeldError> {
        let mut json = Zeroizing::new(Vec::new());
        self.append(key, session, bodies, &mut json)?;
        crate::metrics::checkpoint_bytes("parts", "decode", json.len());
        serde_json::from_slice(&json).map_err(|_| failure("Invalid confidential checkpoint schema"))
    }

    fn append(
        &self,
        key: &SessionSecret,
        session: &SessionId,
        bodies: &BTreeMap<Vec<u8>, Vec<u8>>,
        output: &mut Vec<u8>,
    ) -> Result<(), KeymeldError> {
        match self {
            Self::Part(digest) => {
                let body = bodies
                    .get(digest.as_slice())
                    .ok_or_else(|| failure("Confidential checkpoint part is missing"))?;
                let encrypted = EncryptedData::from_bytes(body)
                    .map_err(|_| failure("Invalid checkpoint part encoding"))?;
                let compressed =
                    Zeroizing::new(key.decrypt(&encrypted, &context(session, digest)).map_err(
                        |_| failure("Confidential checkpoint part authentication failed"),
                    )?);
                let remaining = MAX_STATE_BYTES.saturating_sub(output.len());
                let mut plaintext = Zeroizing::new(Vec::new());
                GzDecoder::new(compressed.as_slice())
                    .take(remaining as u64 + 1)
                    .read_to_end(&mut plaintext)
                    .map_err(|_| failure("Invalid checkpoint part compression"))?;
                if plaintext.len() > remaining {
                    return Err(failure("Confidential checkpoint exceeds size limit"));
                }
                if part_digest(key, session, &plaintext) != *digest {
                    return Err(failure("Confidential checkpoint part digest differs"));
                }
                output.extend_from_slice(&plaintext);
            }
            Self::Object(values) => {
                output.push(b'{');
                for (index, (name, value)) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    serde_json::to_writer(&mut *output, name)
                        .map_err(|error| failure(error.to_string()))?;
                    output.push(b':');
                    value.append(key, session, bodies, output)?;
                }
                output.push(b'}');
            }
            Self::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    value.append(key, session, bodies, output)?;
                }
                output.push(b']');
            }
        }
        if output.len() > MAX_STATE_BYTES {
            return Err(failure("Confidential checkpoint exceeds size limit"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_scalar_array_retains_its_authenticated_part_and_exact_bytes() {
        let key = SessionSecret::from_bytes([31; 32]);
        let session = SessionId::new_v7();
        let json = serde_json::to_string(&vec![255_u8; PART_BYTES]).unwrap();
        let raw: &RawValue = serde_json::from_str(&json).unwrap();
        let mut bodies = BTreeMap::new();
        let manifest = split(&key, &session, raw, &mut bodies).unwrap();
        let expected = part_digest(&key, &session, json.as_bytes());
        assert!(matches!(&manifest, Manifest::Part(digest) if *digest == expected));
        assert_eq!(bodies.len(), 1);
        let bodies = bodies.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();
        let mut restored = Vec::new();
        manifest
            .append(&key, &session, &bodies, &mut restored)
            .unwrap();
        assert_eq!(restored, json.as_bytes());
    }

    #[test]
    fn large_mixed_array_keeps_scalar_prefix_and_nested_values() {
        let key = SessionSecret::from_bytes([32; 32]);
        let session = SessionId::new_v7();
        let value = serde_json::json!([0, {"payload": vec![255_u8; PART_BYTES]}, "[", null]);
        let json = serde_json::to_string(&value).unwrap();
        let raw: &RawValue = serde_json::from_str(&json).unwrap();
        let mut bodies = BTreeMap::new();
        let manifest = split(&key, &session, raw, &mut bodies).unwrap();
        assert!(matches!(&manifest, Manifest::Array(values) if values.len() == 4));
        let bodies = bodies.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();
        let mut restored = Vec::new();
        manifest
            .append(&key, &session, &bodies, &mut restored)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&restored).unwrap(),
            value
        );
    }
}
