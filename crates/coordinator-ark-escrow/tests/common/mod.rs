#![allow(dead_code)]

//! Helpers for tests ported from `@arkade-os/sdk`.

use std::path::PathBuf;

use bitcoin::XOnlyPublicKey;
use coordinator_ark_escrow::RelativeTimelock;
use serde::de::DeserializeOwned;
use serde::Deserialize;

pub fn fixture<T: DeserializeOwned>(name: &str) -> T {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("read {}: {error}", path.display());
    });
    serde_json::from_str(&json).unwrap_or_else(|error| panic!("parse {name}: {error}"))
}

/// An x-only key from 32-byte hex, or from 33-byte compressed hex.
pub fn xonly(hex_key: &str) -> XOnlyPublicKey {
    let bytes = hex::decode(hex_key).unwrap();
    let bytes = match bytes.len() {
        32 => &bytes[..],
        33 => &bytes[1..],
        length => panic!("unexpected key length {length}"),
    };
    XOnlyPublicKey::from_slice(bytes).unwrap()
}

/// A relative timelock in the SDK's JSON form: `{ "type": "blocks" | "seconds", "value": n }`.
#[derive(Debug, Deserialize)]
pub struct JsonTimelock {
    #[serde(rename = "type")]
    kind: String,
    value: u32,
}

impl JsonTimelock {
    pub fn to_timelock(&self) -> RelativeTimelock {
        match self.kind.as_str() {
            "blocks" => RelativeTimelock::Blocks(u16::try_from(self.value).unwrap()),
            "seconds" => RelativeTimelock::Seconds(self.value),
            other => panic!("unknown timelock type {other}"),
        }
    }
}
