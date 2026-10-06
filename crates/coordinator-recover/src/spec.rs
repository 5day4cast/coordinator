//! The recovery records the coordinator publishes, and the recovery file (spec v1).
//!
//! Every record is a kind 30078 event by the coordinator's recovery key:
//!
//! | `d`                              | Content                                         |
//! |----------------------------------|-------------------------------------------------|
//! | `<blind>:wallet`                 | NIP-44 to the player: [`WalletRecord`]          |
//! | `<blind>:entry:<entry_id>`       | NIP-44 to the player: [`EntryRecord`]           |
//! | `competition:<id>`               | plaintext [`CompetitionRecord`], maybe gzipped  |
//! | `competition:<id>:part:<n>`      | one part of a competition too large for one event |
//!
//! Player records carry `["b", <blind>]`, competitions `["c", <id>]`. The recovery file
//! ([`Kit`]) holds the same ciphertexts and competition objects. Parsing is lenient about fields
//! it does not know, so a later version can add some; everything that moves money is checked
//! against the contract and the derived keys before it is used.

use std::collections::BTreeMap;
use std::io::Read;

use base64::Engine;
use bitcoin::OutPoint;
use dlctix::{ContractParameters, ContractSignatures};
use nostr::{Event, PublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{Error, Result};

/// NIP-78 application data, parameterized replaceable.
pub const KIND_APP_DATA: u16 = 30078;
/// The largest competition this accepts once decompressed.
const MAX_COMPETITION_BYTES: u64 = 32 * 1024 * 1024;
/// The most parts a competition may be split into.
const MAX_PARTS: usize = 64;

#[derive(Debug, Clone, Deserialize)]
pub struct WalletRecord {
    pub network: String,
    /// The browser wallet's backup, NIP-44 encrypted by the player to themselves.
    pub wallet_blob: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryRecord {
    pub network: String,
    pub coordinator_pubkey: String,
    pub user_pubkey: String,
    pub competition_id: Uuid,
    pub entry_id: Uuid,
    /// Hex, compressed or x-only.
    pub entry_pubkey: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub escrow: Option<EscrowRecord>,
    #[serde(default)]
    pub ticket: Option<TicketRecord>,
    #[serde(default)]
    pub contract: Option<ContractRecord>,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscrowRecord {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub arkd_url: String,
    pub server_pubkey: String,
    /// The escrow VTXO; null until it is funded.
    #[serde(default)]
    pub outpoint: Option<String>,
    #[serde(default)]
    pub amount_sat: Option<u64>,
    /// `T`, unix seconds.
    pub refund_locktime: u64,
    #[serde(default)]
    pub exit_delay_secs: u64,
    #[serde(default)]
    pub unilateral_refund_delay_secs: u64,
    /// Hex leaf scripts, in leaf order.
    pub tap_tree: Vec<String>,
    #[serde(default)]
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketRecord {
    pub hash: String,
    #[serde(default)]
    pub preimage: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractRecord {
    pub funding_outpoint: String,
    pub player_index: usize,
    #[serde(default)]
    pub contract_parameters_sha256: String,
    #[serde(default)]
    pub pruned_signatures: Option<ContractSignatures>,
    pub relative_locktime_block_delta: u16,
}

/// A signed contract as the coordinator's API serialises dlctix's `SignedContract`.
#[derive(Debug, Clone, Deserialize)]
pub struct SignedContractRecord {
    pub signatures: ContractSignatures,
    pub dlc: DlcRecord,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DlcRecord {
    pub params: ContractParameters,
    pub funding_outpoint: OutPoint,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OracleRecord {
    #[serde(default)]
    pub pubkey: Option<String>,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub expiry: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompetitionRecord {
    #[serde(default)]
    pub network: String,
    pub competition_id: Uuid,
    #[serde(default)]
    pub contract_parameters: Option<ContractParameters>,
    #[serde(default)]
    pub signed_contract: Option<SignedContractRecord>,
    #[serde(default)]
    pub funding_outpoint: Option<String>,
    #[serde(default)]
    pub funding_tx: Option<String>,
    #[serde(default)]
    pub oracle: Option<OracleRecord>,
    #[serde(default)]
    pub attestation: Option<String>,
}

impl CompetitionRecord {
    /// The contract terms and funding outpoint, from the signed contract if there is one.
    pub fn contract(&self) -> Option<(&ContractParameters, OutPoint)> {
        if let Some(signed) = &self.signed_contract {
            return Some((&signed.dlc.params, signed.dlc.funding_outpoint));
        }
        let params = self.contract_parameters.as_ref()?;
        let outpoint = self.funding_outpoint.as_deref()?.parse().ok()?;
        Some((params, outpoint))
    }

    /// The oracle's event id: the record's, or the competition id, which the coordinator uses.
    pub fn oracle_event_id(&self) -> String {
        self.oracle
            .as_ref()
            .and_then(|oracle| oracle.event_id.clone())
            .unwrap_or_else(|| self.competition_id.to_string())
    }
}

/// The downloadable recovery file.
#[derive(Debug, Clone, Deserialize)]
pub struct Kit {
    #[serde(rename = "type")]
    pub kind: String,
    pub v: u32,
    #[serde(default)]
    pub network: String,
    pub coordinator_pubkey: String,
    pub user_pubkey: String,
    #[serde(default)]
    pub relays: Vec<String>,
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default)]
    pub entries: Vec<String>,
    #[serde(default)]
    pub competitions: Vec<Value>,
}

impl Kit {
    pub fn parse(json: &str) -> Result<Self> {
        let kit: Kit = serde_json::from_str(json)
            .map_err(|e| Error::Invalid(format!("recovery file: {e}")))?;
        if kit.kind != "coordinator-recovery-kit" || kit.v != 1 {
            return Err(Error::Invalid(format!(
                "recovery file: unsupported {} v{}",
                kit.kind, kit.v
            )));
        }
        Ok(kit)
    }

    pub fn coordinator(&self) -> Result<PublicKey> {
        parse_pubkey(
            &self.coordinator_pubkey,
            "coordinator pubkey in the recovery file",
        )
    }
}

pub fn parse_pubkey(hex: &str, what: &str) -> Result<PublicKey> {
    PublicKey::parse(hex.trim()).map_err(|_| Error::Invalid(what.to_owned()))
}

/// The `d` tags of a player's records.
pub fn wallet_d(blind: &str) -> String {
    format!("{blind}:wallet")
}

pub fn entry_d_prefix(blind: &str) -> String {
    format!("{blind}:entry:")
}

/// The relay filter for a player's wallet and entry records.
pub fn player_filter(coordinator: &PublicKey, blind: &str) -> Value {
    json!({
        "kinds": [KIND_APP_DATA],
        "authors": [coordinator.to_hex()],
        "#b": [blind],
    })
}

/// The relay filter for competition contracts, whole or in parts.
pub fn competition_filter(coordinator: &PublicKey, competitions: &[Uuid]) -> Value {
    json!({
        "kinds": [KIND_APP_DATA],
        "authors": [coordinator.to_hex()],
        "#c": competitions.iter().map(Uuid::to_string).collect::<Vec<_>>(),
    })
}

/// The first value of an event's tag named `name`.
pub fn tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [key, value, ..] if key == name => Some(value.as_str()),
        _ => None,
    })
}

/// Keep, of `events`, those signed by `author` with kind 30078 and a valid signature, and of
/// each `d` only the newest, as a relay keeping replaceable events would.
pub fn newest_by_d(events: &[Event], author: &PublicKey) -> BTreeMap<String, Event> {
    let mut newest: BTreeMap<String, Event> = BTreeMap::new();
    for event in events {
        if event.pubkey != *author
            || event.kind.as_u16() != KIND_APP_DATA
            || event.verify().is_err()
        {
            continue;
        }
        let Some(d) = tag(event, "d") else {
            continue;
        };
        match newest.get(d) {
            Some(kept) if kept.created_at >= event.created_at => {}
            _ => {
                newest.insert(d.to_owned(), event.clone());
            }
        }
    }
    newest
}

/// A competition from its event content or recovery file object: plain, gzipped, or (with
/// [`assemble_parts`]) in parts.
pub fn competition_from_value(value: Value) -> Result<CompetitionRecord> {
    let value = unwrap_encoding(value)?;
    serde_json::from_value(value).map_err(|e| Error::Record(format!("competition: {e}")))
}

fn unwrap_encoding(value: Value) -> Result<Value> {
    match value.get("encoding").and_then(Value::as_str) {
        None => Ok(value),
        Some("gzip+base64") => {
            let data = value
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Record("compressed competition has no data".into()))?;
            let json = gunzip_base64(data)?;
            serde_json::from_slice(&json)
                .map_err(|e| Error::Record(format!("compressed competition: {e}")))
        }
        Some(other) => Err(Error::Record(format!(
            "competition encoding {other} is not supported"
        ))),
    }
}

fn gunzip_base64(data: &str) -> Result<Vec<u8>> {
    gunzip_base64_limited(data, MAX_COMPETITION_BYTES)
}

fn gunzip_base64_limited(data: &str, limit: u64) -> Result<Vec<u8>> {
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| Error::Record(format!("competition data is not base64: {e}")))?;
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .take(limit + 1)
        .read_to_end(&mut json)
        .map_err(|e| Error::Record(format!("competition data is not gzip: {e}")))?;
    if json.len() as u64 > limit {
        return Err(Error::Record("competition is too large".into()));
    }
    Ok(json)
}

/// Join a competition published in parts: the parts' `data`, concatenated in part order, under
/// the manifest's `encoding` (or the parts' own). Parts are numbered from 1; 0 is accepted too.
pub fn assemble_parts(mut parts: Vec<Value>, manifest: Option<&Value>) -> Result<Value> {
    let count = |value: &Value, key: &str| value.get(key).and_then(Value::as_u64);
    let total = manifest
        .and_then(|manifest| count(manifest, "parts"))
        .or_else(|| parts.first().and_then(|part| count(part, "parts")))
        .ok_or_else(|| Error::Record("competition part without a part count".into()))?;
    if total == 0 || total as usize > MAX_PARTS {
        return Err(Error::Record(format!("competition in {total} parts")));
    }
    parts.sort_by_key(|part| count(part, "part"));
    parts.dedup_by_key(|part| count(part, "part"));
    if parts.len() as u64 != total || parts.iter().any(|part| count(part, "parts") != Some(total)) {
        return Err(Error::NotYet(format!(
            "only {} of a competition's {total} parts were found",
            parts.len()
        )));
    }
    let first = count(&parts[0], "part").unwrap_or(0);
    let mut data = String::new();
    for (index, part) in parts.iter().enumerate() {
        if count(part, "part") != Some(first + index as u64) {
            return Err(Error::Record("competition parts are not contiguous".into()));
        }
        data.push_str(
            part.get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Record("competition part has no data".into()))?,
        );
    }
    let encoding = manifest
        .into_iter()
        .chain(parts.first())
        .find_map(|value| value.get("encoding").and_then(Value::as_str));
    match encoding {
        Some(encoding) => Ok(json!({"encoding": encoding, "data": data})),
        None => serde_json::from_str(&data)
            .map_err(|e| Error::Record(format!("joined competition parts: {e}"))),
    }
}

/// Competitions from event contents or recovery file objects, as published: whole (plain or
/// gzipped), or a manifest (`"parts": N`, no `data`) with its `competition_part` objects. Ones
/// that cannot be read yet are returned as errors, by competition.
pub fn assemble_competitions(
    values: impl IntoIterator<Item = Value>,
) -> BTreeMap<Uuid, Result<CompetitionRecord>> {
    let id_of = |value: &Value| {
        value
            .get("competition_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
    };
    let mut competitions: BTreeMap<Uuid, Result<CompetitionRecord>> = BTreeMap::new();
    let mut manifests: BTreeMap<Uuid, Value> = BTreeMap::new();
    let mut parts: BTreeMap<Uuid, Vec<Value>> = BTreeMap::new();
    for value in values {
        let is_part = value.get("type").and_then(Value::as_str) == Some("competition_part")
            || value.get("part").is_some();
        let is_manifest = !is_part && value.get("parts").is_some() && value.get("data").is_none();
        let id = id_of(&value);
        match (id, is_part, is_manifest) {
            (Some(id), true, _) => parts.entry(id).or_default().push(value),
            (Some(id), _, true) => {
                manifests.insert(id, value);
            }
            (_, false, false) => match competition_from_value(value) {
                Ok(competition) => {
                    let id = competition.competition_id;
                    // Keep one with a contract over one without.
                    let keep_known = competition.contract().is_none()
                        && matches!(competitions.get(&id), Some(Ok(known)) if known.contract().is_some());
                    if !keep_known {
                        competitions.insert(id, Ok(competition));
                    }
                }
                Err(e) => {
                    if let Some(id) = id {
                        competitions.entry(id).or_insert(Err(e));
                    }
                }
            },
            _ => {}
        }
    }
    let mut ids: Vec<Uuid> = manifests.keys().chain(parts.keys()).copied().collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        if matches!(competitions.get(&id), Some(Ok(_))) {
            continue;
        }
        let joined = assemble_parts(parts.remove(&id).unwrap_or_default(), manifests.get(&id))
            .and_then(competition_from_value);
        competitions.insert(id, joined);
    }
    competitions
}

/// The competitions among `events` (already the coordinator's newest per `d`).
pub fn competitions_from_events(
    events: &BTreeMap<String, Event>,
) -> BTreeMap<Uuid, Result<CompetitionRecord>> {
    assemble_competitions(
        events
            .iter()
            .filter(|(d, _)| d.starts_with("competition:"))
            .filter_map(|(_, event)| serde_json::from_str::<Value>(&event.content).ok()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    fn gzip_base64(text: &str) -> String {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(text.as_bytes()).unwrap();
        base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap())
    }

    const COMPETITION: &str = r#"{"v":1,"type":"competition","network":"signet",
        "competition_id":"0192f2a4-7b1c-7a00-8000-000000000001","contract_parameters":null,
        "signed_contract":null,"funding_outpoint":null,"funding_tx":null,
        "oracle":{"pubkey":"ab","event_id":"0192f2a4-7b1c-7a00-8000-000000000001","expiry":5},
        "attestation":null}"#;

    #[test]
    fn reads_plain_and_gzipped_competitions() {
        let plain = competition_from_value(serde_json::from_str(COMPETITION).unwrap()).unwrap();
        let wrapped = json!({"v":1,"type":"competition","encoding":"gzip+base64",
            "data": gzip_base64(COMPETITION)});
        let gzipped = competition_from_value(wrapped).unwrap();
        assert_eq!(plain.competition_id, gzipped.competition_id);
        assert_eq!(gzipped.oracle_event_id(), plain.competition_id.to_string());
        assert!(gzipped.contract().is_none());
    }

    #[test]
    fn joins_a_manifest_and_its_parts_in_order_and_waits_for_missing_ones() {
        let id = "0192f2a4-7b1c-7a00-8000-000000000001";
        let data = gzip_base64(COMPETITION);
        let (a, rest) = data.split_at(data.len() / 3);
        let (b, c) = rest.split_at(rest.len() / 2);
        // As the coordinator publishes it: a manifest, then 1-based parts without an encoding.
        let manifest = json!({"v":1,"type":"competition","competition_id":id,
            "encoding":"gzip+base64","parts":3});
        let part = |n: u64, data: &str| json!({"v":1,"type":"competition_part","competition_id":id,"part":n,"parts":3,"data":data});
        let all = vec![part(3, c), manifest.clone(), part(1, a), part(2, b)];
        let competitions = assemble_competitions(all);
        let competition = competitions[&Uuid::parse_str(id).unwrap()]
            .as_ref()
            .unwrap();
        assert_eq!(competition.competition_id.to_string(), id);

        let missing = assemble_competitions(vec![manifest, part(1, a), part(3, c)]);
        assert!(matches!(
            missing[&Uuid::parse_str(id).unwrap()],
            Err(Error::NotYet(_))
        ));
    }

    #[test]
    fn refuses_unknown_encodings_and_oversized_data() {
        assert!(competition_from_value(json!({"encoding":"zstd","data":""})).is_err());
        let data = gzip_base64(&"0".repeat(100));
        assert!(gunzip_base64_limited(&data, 99).is_err());
        assert_eq!(gunzip_base64_limited(&data, 100).unwrap().len(), 100);
    }

    #[test]
    fn reads_the_spec_kit() {
        let kit = Kit::parse(
            r#"{ "type": "coordinator-recovery-kit", "v": 1, "network": "signet", "created_at": 0,
              "coordinator_pubkey": "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
              "user_pubkey": "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
              "relays": ["wss://relay.example"], "wallet": "AgAB", "entries": ["AgAC"],
              "competitions": [] }"#,
        )
        .unwrap();
        assert_eq!(kit.relays, ["wss://relay.example"]);
        assert_eq!(kit.entries.len(), 1);
        assert!(kit.coordinator().is_ok());
        assert!(
            Kit::parse(r#"{"type":"other","v":1,"coordinator_pubkey":"","user_pubkey":""}"#)
                .is_err()
        );
    }

    #[test]
    fn reads_the_spec_entry_record() {
        let record: EntryRecord = serde_json::from_str(
            r#"{
              "v": 1, "type": "entry", "network": "signet",
              "coordinator_pubkey": "aa", "user_pubkey": "bb",
              "competition_id": "0192f2a4-7b1c-7a00-8000-000000000001",
              "entry_id": "0192f2a4-7b1c-7a00-8000-000000000002",
              "entry_pubkey": "02db32ae6adf4d575228bc8de8a99d3f856855bbe2d8d9ff84e9a1c81d9ea73a03",
              "status": "escrowed",
              "escrow": {
                "kind": "ark", "arkd_url": "https://arkd.example", "server_pubkey": "cc",
                "outpoint": "0000000000000000000000000000000000000000000000000000000000000001:0",
                "amount_sat": 5000, "refund_locktime": 1700000000, "exit_delay_secs": 86016,
                "unilateral_refund_delay_secs": 172032, "tap_tree": ["51"], "created_at": 1690000000
              },
              "ticket": { "hash": "dd", "preimage": null },
              "contract": null,
              "updated_at": 1690000001
            }"#,
        )
        .unwrap();
        assert_eq!(record.escrow.unwrap().amount_sat, Some(5000));
        assert!(record.ticket.unwrap().preimage.is_none());
        assert!(record.contract.is_none());
    }
}
