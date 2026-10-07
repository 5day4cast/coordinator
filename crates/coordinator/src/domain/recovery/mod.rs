//! Recovery records: what a player needs to get their money back without the coordinator.
//!
//! The coordinator publishes three kinds of Nostr events (kind 30078, replaceable by their `d`
//! tag), signed by a key used for nothing else:
//!
//! - the player's wallet backup (the seed blob their browser already encrypted to their nsec),
//! - one record per entry: its escrow, ticket and place in the contract, re-published as the
//!   entry advances,
//! - each competition's signed contract, which the public API already serves.
//!
//! Wallet and entry records are NIP-44 encrypted to the player and tagged with a blind tag only
//! the player can compute, so relays never learn who entered. The same events make up the
//! recovery file a player downloads from their account. See docs/RECOVERY.md for the formats.

mod admin;
mod publisher;
mod relay;
#[cfg(test)]
mod tests;

pub use admin::{recovery_status, republish, RecoveryStatus, RelayStatus, RepublishReport};
pub use publisher::{publish_due, RecoveryPublisher, RelayHealth, Retention};
pub use relay::publish as publish_to_relay;

use crate::domain::{RecoveryCompetitionRow, RecoveryTicketRow};
use anyhow::{anyhow, Context};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use bitcoin::{Network, OutPoint, Transaction};
use coordinator_ark_escrow::{EntryEscrow, RelativeTimelock, VtxoScript};
use dlctix::{
    secp::{MaybeScalar, Point},
    ContractParameters, ContractSignatures, EventLockingConditions, Outcome,
};
use flate2::{write::GzEncoder, Compression};
use log::warn;
use nostr::{
    nips::nip44::{self, Version},
    Event, EventBuilder, Keys, Kind, PublicKey, SecretKey, Tag, Timestamp,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, io::Write};
use uuid::Uuid;

/// NIP-78 application data, replaceable by the `d` tag.
pub const RECOVERY_EVENT_KIND: Kind = Kind::ApplicationSpecificData;

/// NIP-09 deletion request, which retires a record whose money is settled.
pub const DELETION_EVENT_KIND: Kind = Kind::EventDeletion;

/// Prefix of the blind tag's preimage.
const BLIND_TAG_DOMAIN: &[u8] = b"coordinator-recovery/v1";

/// The largest event content published as it is. A larger competition event is compressed,
/// and split into parts if it is still larger.
pub const MAX_CONTENT_BYTES: usize = 60 * 1024;

/// NIP-44 encrypts at most 65535 bytes. An entry record past this leaves out its contract
/// signatures, which the competition event carries in full.
const MAX_ENTRY_RECORD_BYTES: usize = 65_000;

/// `hex(sha256("coordinator-recovery/v1" || coordinator_pubkey || user_pubkey))`, with both keys
/// as 32-byte x-only keys. Tags the player's events without naming the player.
pub fn blind_tag(coordinator: &PublicKey, user: &PublicKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(BLIND_TAG_DOMAIN);
    hasher.update(coordinator.as_bytes());
    hasher.update(user.as_bytes());
    hex::encode(hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The wallet backup: the blob the player's browser stored at sign-up, unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletRecord {
    pub v: u8,
    #[serde(rename = "type")]
    pub record_type: String,
    pub network: String,
    pub wallet_blob: String,
}

/// How far an entry got. Each record carries its latest status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    /// A ticket is reserved; nothing is paid yet.
    Ticketed,
    /// The buy-in is in the Arkade escrow.
    Escrowed,
    /// The payment settled, so the player holds the ticket preimage.
    Paid,
    /// The contract is signed and funded with this entry in it.
    InContract,
    Won,
    Lost,
    /// The escrow went back to the player.
    Refunded,
    /// The win was paid out, or closed on chain.
    Settled,
}

/// The entry's Arkade escrow. Its leaves fix every term; the other fields restate them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscrowRecord {
    pub kind: String,
    pub arkd_url: String,
    /// The Arkade server's x-only signer key.
    pub server_pubkey: String,
    /// The escrow VTXO, `txid:vout`; null until it is funded.
    pub outpoint: Option<String>,
    pub amount_sat: Option<u64>,
    /// `T`, an absolute locktime (Unix seconds) after which the refund leaf opens.
    pub refund_locktime: u32,
    /// The unilateral delays in seconds; 0 when the escrow counts them in blocks.
    pub exit_delay_secs: u32,
    pub unilateral_refund_delay_secs: u32,
    /// The leaf scripts, hex, in leaf order: funding, refund, unilateral funding, unilateral
    /// refund.
    pub tap_tree: Vec<String>,
    /// When the ticket was reserved, about when the escrow was issued (Unix seconds).
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketRecord {
    pub hash: String,
    /// Known once the payment settled.
    pub preimage: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractRecord {
    pub funding_outpoint: String,
    pub player_index: usize,
    /// sha256 of the contract parameters serialized as JSON by dlctix.
    pub contract_parameters_sha256: String,
    /// dlctix `SignedContract::pruned_signatures(player_pubkey)`.
    pub pruned_signatures: Option<ContractSignatures>,
    pub relative_locktime_block_delta: u16,
}

/// One entry's recovery record. The entry key and payout preimage are never in it: the player
/// derives both from their wallet seed, the network and the entry id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRecord {
    pub v: u8,
    #[serde(rename = "type")]
    pub record_type: String,
    pub network: String,
    pub coordinator_pubkey: String,
    pub user_pubkey: String,
    pub competition_id: Uuid,
    pub entry_id: Uuid,
    /// The entry key, compressed (33 bytes), hex.
    pub entry_pubkey: String,
    pub status: RecordStatus,
    pub escrow: Option<EscrowRecord>,
    pub ticket: Option<TicketRecord>,
    pub contract: Option<ContractRecord>,
    pub updated_at: u64,
}

impl EntryRecord {
    /// Digest of the record without its `updated_at`, which changes on every publish.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        let record = EntryRecord {
            updated_at: 0,
            ..self.clone()
        };
        Ok(sha256_hex(&serde_json::to_vec(&record)?))
    }
}

/// The recovery file a player downloads: the same contents as their events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryKit {
    #[serde(rename = "type")]
    pub kit_type: String,
    pub v: u8,
    pub network: String,
    pub created_at: u64,
    pub coordinator_pubkey: String,
    pub user_pubkey: String,
    pub relays: Vec<String>,
    /// NIP-44 ciphertext of the wallet record; null for an account without one.
    pub wallet: Option<String>,
    /// NIP-44 ciphertexts of the entry records.
    pub entries: Vec<String>,
    /// Contents of the competition events, as published.
    pub competitions: Vec<serde_json::Value>,
}

/// What `GET /api/v1/recovery/info` serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryInfo {
    pub v: u8,
    pub coordinator_pubkey: String,
    pub network: String,
    pub relays: Vec<String>,
}

/// The parts of a stored signed contract the records use. Reading only these skips rebuilding
/// the contract's transactions, which parsing a `SignedContract` does.
/// The competition event's content. The contract keeps the JSON it is stored as.
#[derive(Serialize)]
struct CompetitionContent<'a> {
    v: u8,
    #[serde(rename = "type")]
    content_type: &'static str,
    network: String,
    competition_id: Uuid,
    contract_parameters: &'a RawValue,
    signed_contract: &'a RawValue,
    funding_outpoint: String,
    funding_tx: Option<String>,
    oracle: OracleReference,
    attestation: Option<String>,
}

#[derive(Serialize)]
struct OracleReference {
    /// The oracle's compressed public key, hex.
    pubkey: Option<String>,
    event_id: Option<serde_json::Value>,
    expiry: Option<u32>,
}

#[derive(Deserialize)]
struct StoredSignedContract {
    signatures: ContractSignatures,
    dlc: StoredDlc,
}

#[derive(Deserialize)]
struct StoredDlc {
    funding_outpoint: OutPoint,
}

#[derive(Deserialize)]
struct StoredFundingOutpoint {
    dlc: StoredDlc,
}

/// The coordinator's recovery key with the settings every record carries.
pub struct Recovery {
    keys: Keys,
    network: Network,
    relays: Vec<String>,
    arkd_url: String,
}

impl Recovery {
    pub fn new(secret: SecretKey, network: Network, relays: Vec<String>, arkd_url: String) -> Self {
        Self {
            keys: Keys::new(secret),
            network,
            relays,
            arkd_url,
        }
    }

    /// Load the recovery key from its PEM file, creating it on first start.
    pub fn load(
        key_file: &str,
        network: Network,
        relays: Vec<String>,
        arkd_url: String,
    ) -> Result<Self, anyhow::Error> {
        let key: bitcoin::secp256k1::SecretKey = crate::infra::secrets::get_key(key_file)
            .with_context(|| format!("recovery key {key_file}"))?;
        let secret = SecretKey::from_slice(&key.secret_bytes())?;
        Ok(Self::new(secret, network, relays, arkd_url))
    }

    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    pub fn relays(&self) -> &[String] {
        &self.relays
    }

    pub fn network(&self) -> String {
        self.network.to_string()
    }

    pub fn info(&self) -> RecoveryInfo {
        RecoveryInfo {
            v: 1,
            coordinator_pubkey: self.public_key().to_hex(),
            network: self.network(),
            relays: self.relays.clone(),
        }
    }

    pub fn blind(&self, user: &PublicKey) -> String {
        blind_tag(&self.public_key(), user)
    }

    pub fn wallet_d_tag(&self, user: &PublicKey) -> String {
        format!("{}:wallet", self.blind(user))
    }

    pub fn entry_d_tag(&self, user: &PublicKey, entry_id: Uuid) -> String {
        format!("{}:entry:{entry_id}", self.blind(user))
    }

    pub fn wallet_record(&self, wallet_blob: &str) -> WalletRecord {
        WalletRecord {
            v: 1,
            record_type: "wallet".into(),
            network: self.network(),
            wallet_blob: wallet_blob.to_owned(),
        }
    }

    /// NIP-44 v2 from the recovery key to the player.
    pub fn encrypt(&self, user: &PublicKey, plaintext: &str) -> Result<String, anyhow::Error> {
        Ok(nip44::encrypt(
            self.keys.secret_key(),
            user,
            plaintext,
            Version::V2,
        )?)
    }

    fn sign(
        &self,
        content: String,
        tags: Vec<Tag>,
        created_at: i64,
    ) -> Result<Event, anyhow::Error> {
        Ok(EventBuilder::new(RECOVERY_EVENT_KIND, content)
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at.max(0) as u64))
            .sign_with_keys(&self.keys)?)
    }

    fn player_event(
        &self,
        user: &PublicKey,
        d_tag: String,
        plaintext: &str,
        created_at: i64,
    ) -> Result<Event, anyhow::Error> {
        let content = self.encrypt(user, plaintext)?;
        let blind = self.blind(user);
        self.sign(
            content,
            vec![Tag::identifier(d_tag), Tag::parse(["b", blind.as_str()])?],
            created_at,
        )
    }

    pub fn wallet_event(
        &self,
        user: &PublicKey,
        record: &WalletRecord,
        created_at: i64,
    ) -> Result<Event, anyhow::Error> {
        self.player_event(
            user,
            self.wallet_d_tag(user),
            &serde_json::to_string(record)?,
            created_at,
        )
    }

    pub fn entry_event(
        &self,
        user: &PublicKey,
        record: &EntryRecord,
        created_at: i64,
    ) -> Result<Event, anyhow::Error> {
        let mut plaintext = serde_json::to_string(record)?;
        if plaintext.len() > MAX_ENTRY_RECORD_BYTES {
            warn!(
                "Recovery record of entry {} is {} bytes; publishing it without its contract signatures",
                record.entry_id,
                plaintext.len()
            );
            let mut record = record.clone();
            if let Some(contract) = record.contract.as_mut() {
                contract.pruned_signatures = None;
            }
            plaintext = serde_json::to_string(&record)?;
        }
        self.player_event(
            user,
            self.entry_d_tag(user, record.entry_id),
            &plaintext,
            created_at,
        )
    }

    /// The NIP-09 deletion of the record under `d_tag`, whose current version is `event_id`.
    ///
    /// It names the record both ways: by its address (`a`, every version up to the deletion's
    /// `created_at`) as NIP-09 asks, and by the version's id (`e`), the only form some relays
    /// (nostr-rs-relay among them) act on. `created_at` must follow the version's own.
    pub fn deletion_event(
        &self,
        d_tag: &str,
        event_id: &str,
        created_at: i64,
    ) -> Result<Event, anyhow::Error> {
        let kind = RECOVERY_EVENT_KIND.as_u16().to_string();
        let address = format!("{kind}:{}:{d_tag}", self.public_key().to_hex());
        Ok(EventBuilder::new(DELETION_EVENT_KIND, "settled")
            .tags(vec![
                Tag::parse(["e", event_id])?,
                Tag::parse(["a", address.as_str()])?,
                Tag::parse(["k", kind.as_str()])?,
            ])
            .custom_created_at(Timestamp::from(created_at.max(0) as u64))
            .sign_with_keys(&self.keys)?)
    }

    /// A competition event: public, tagged with the competition id.
    pub fn competition_event(
        &self,
        competition_id: Uuid,
        d_tag: String,
        content: String,
        created_at: i64,
    ) -> Result<Event, anyhow::Error> {
        let id = competition_id.to_string();
        self.sign(
            content,
            vec![Tag::identifier(d_tag), Tag::parse(["c", id.as_str()])?],
            created_at,
        )
    }

    /// The records of every entry of a competition, with each one's player.
    pub fn entry_records(
        &self,
        competition: &RecoveryCompetitionRow,
        tickets: &[RecoveryTicketRow],
    ) -> Vec<(PublicKey, EntryRecord)> {
        let contract = ContractView::read(competition)
            .inspect_err(|error| {
                warn!(
                    "Cannot read the contract of competition {} for recovery records: {error:#}",
                    competition.id
                )
            })
            .ok()
            .flatten();
        tickets
            .iter()
            .filter_map(|ticket| self.entry_record(competition.id, ticket, contract.as_ref()))
            .collect()
    }

    fn entry_record(
        &self,
        competition_id: Uuid,
        ticket: &RecoveryTicketRow,
        contract: Option<&ContractView>,
    ) -> Option<(PublicKey, EntryRecord)> {
        let user = ticket.entry_user.as_ref().or(ticket.reserved_by.as_ref())?;
        let user = PublicKey::from_hex(user).ok()?;
        let entry_id = ticket_entry_id(ticket)?;
        let entry_pubkey = ticket
            .entry_pubkey
            .as_ref()
            .or(ticket.policy_entry_pubkey.as_ref())?
            .clone();
        let point = Point::from_hex(&entry_pubkey).ok();
        let contract = contract.and_then(|contract| {
            let index = contract
                .params
                .players
                .iter()
                .position(|player| Some(player.pubkey) == point)?;
            Some((contract, index))
        });
        let record = EntryRecord {
            v: 1,
            record_type: "entry".into(),
            network: self.network(),
            coordinator_pubkey: self.public_key().to_hex(),
            user_pubkey: user.to_hex(),
            competition_id,
            entry_id,
            entry_pubkey,
            status: status(ticket, contract),
            escrow: self.escrow_record(ticket),
            ticket: Some(TicketRecord {
                hash: ticket.ticket_hash.clone(),
                preimage: ticket
                    .settled
                    .then(|| ticket.ticket_preimage.clone())
                    .flatten(),
            }),
            contract: contract.and_then(|(contract, index)| {
                Some(ContractRecord {
                    funding_outpoint: contract.funding_outpoint.to_string(),
                    player_index: index,
                    contract_parameters_sha256: contract.params_sha256.clone(),
                    pruned_signatures: pruned_signatures(
                        &contract.params,
                        &contract.signatures,
                        point?,
                    ),
                    relative_locktime_block_delta: contract.params.relative_locktime_block_delta,
                })
            }),
            updated_at: 0,
        };
        Some((user, record))
    }

    fn escrow_record(&self, ticket: &RecoveryTicketRow) -> Option<EscrowRecord> {
        let tap_tree = ticket.escrow_tap_tree.as_ref()?;
        let escrow = hex::decode(tap_tree)
            .map_err(|e| anyhow!("not hex: {e}"))
            .and_then(|bytes| Ok(VtxoScript::decode_tap_tree(&bytes)?))
            .and_then(|vtxo| Ok(EntryEscrow::from_vtxo_script(&vtxo)?));
        let escrow = match escrow {
            Ok(escrow) => escrow,
            Err(error) => {
                warn!(
                    "Cannot read the escrow of ticket {} for its recovery record: {error:#}",
                    ticket.ticket_id
                );
                return None;
            }
        };
        let terms = escrow.terms();
        let seconds = |delay: RelativeTimelock| match delay {
            RelativeTimelock::Seconds(seconds) => seconds,
            RelativeTimelock::Blocks(_) => 0,
        };
        Some(EscrowRecord {
            kind: "ark".into(),
            arkd_url: self.arkd_url.clone(),
            server_pubkey: terms.server.to_string(),
            outpoint: ticket.vtxo_outpoint.clone(),
            amount_sat: ticket.vtxo_sats,
            refund_locktime: terms.refund_locktime.to_consensus_u32(),
            exit_delay_secs: seconds(terms.exit_delay),
            unilateral_refund_delay_secs: seconds(terms.unilateral_refund_delay),
            tap_tree: escrow
                .vtxo_script()
                .scripts()
                .iter()
                .map(|script| hex::encode(script.as_bytes()))
                .collect(),
            created_at: ticket.reserved_at.unwrap_or(0).max(0) as u64,
        })
    }

    /// The competition event's contents with their `d` tags, once the contract is signed:
    /// one event, or a manifest and its parts when even compressed it is too large.
    pub fn competition_contents(
        &self,
        competition: &RecoveryCompetitionRow,
        oracle_pubkey: Option<&str>,
    ) -> Result<Option<Vec<(String, String)>>, anyhow::Error> {
        let (Some(params), Some(signed)) = (
            competition.contract_parameters.as_deref(),
            competition.signed_contract.as_deref(),
        ) else {
            return Ok(None);
        };
        let contract_parameters: Box<RawValue> = serde_json::from_slice(params)?;
        let signed_contract: Box<RawValue> = serde_json::from_slice(signed)?;
        let funding_outpoint = match competition.funding_outpoint.as_deref() {
            Some(bytes) => serde_json::from_slice::<OutPoint>(bytes)?,
            None => {
                serde_json::from_slice::<StoredFundingOutpoint>(signed)?
                    .dlc
                    .funding_outpoint
            }
        };
        let funding_tx = competition
            .funding_transaction
            .as_deref()
            .map(serde_json::from_slice::<Transaction>)
            .transpose()?
            .map(|tx| bitcoin::consensus::encode::serialize_hex(&tx));
        let event = competition
            .event_announcement
            .as_deref()
            .map(serde_json::from_slice::<EventLockingConditions>)
            .transpose()?;
        let submission: serde_json::Value = serde_json::from_slice(&competition.event_submission)?;
        let attestation = competition
            .attestation
            .as_deref()
            .map(serde_json::from_slice::<MaybeScalar>)
            .transpose()?
            .map(|attestation| hex::encode(attestation.serialize()));
        let content = CompetitionContent {
            v: 1,
            content_type: "competition",
            network: self.network(),
            competition_id: competition.id,
            contract_parameters: &contract_parameters,
            signed_contract: &signed_contract,
            funding_outpoint: funding_outpoint.to_string(),
            funding_tx,
            oracle: OracleReference {
                pubkey: oracle_pubkey.map(str::to_owned),
                event_id: submission.get("id").cloned(),
                expiry: event.and_then(|event| event.expiry),
            },
            attestation,
        };
        Ok(Some(split_competition_content(
            competition.id,
            &serde_json::to_string(&content)?,
        )?))
    }

    /// The recovery file of a player from their outbox events, as `(kind, event_json)` pairs.
    pub fn kit(
        &self,
        user: &PublicKey,
        wallet_blob: Option<&str>,
        events: &[(String, String)],
        now: u64,
    ) -> Result<RecoveryKit, anyhow::Error> {
        let wallet = wallet_blob
            .map(|blob| self.encrypt(user, &serde_json::to_string(&self.wallet_record(blob))?))
            .transpose()?;
        let mut entries = Vec::new();
        let mut competitions = Vec::new();
        for (kind, event_json) in events {
            let event: Event = serde_json::from_str(event_json)?;
            match kind.as_str() {
                "entry" => entries.push(event.content),
                "competition" => competitions.push(serde_json::from_str(&event.content)?),
                _ => {}
            }
        }
        Ok(RecoveryKit {
            kit_type: "coordinator-recovery-kit".into(),
            v: 1,
            network: self.network(),
            created_at: now,
            coordinator_pubkey: self.public_key().to_hex(),
            user_pubkey: user.to_hex(),
            relays: self.relays.clone(),
            wallet,
            entries,
            competitions,
        })
    }
}

/// The recovery file's name: the first characters of the player's npub, enough to tell
/// accounts apart.
pub fn kit_file_name(npub: &str) -> String {
    let prefix: String = npub.chars().take(16).collect();
    format!("coordinator-recovery-{prefix}.json")
}

/// Split a competition event's content into events no larger than [`MAX_CONTENT_BYTES`].
///
/// A small content is one event, `competition:<id>`. A larger one is gzipped and base64
/// encoded into `{"encoding":"gzip+base64","data":...}`. If that is still too large,
/// `competition:<id>` holds a manifest with `"parts": N` and no data, and
/// `competition:<id>:part:<n>` for n = 1..=N hold the data in order.
pub fn split_competition_content(
    competition_id: Uuid,
    content: &str,
) -> Result<Vec<(String, String)>, anyhow::Error> {
    let d_tag = format!("competition:{competition_id}");
    if content.len() <= MAX_CONTENT_BYTES {
        return Ok(vec![(d_tag, content.to_owned())]);
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(content.as_bytes())?;
    let data = BASE64.encode(encoder.finish()?);
    let compressed = json!({
        "v": 1,
        "type": "competition",
        "competition_id": competition_id,
        "encoding": "gzip+base64",
        "data": data,
    })
    .to_string();
    if compressed.len() <= MAX_CONTENT_BYTES {
        return Ok(vec![(d_tag, compressed)]);
    }
    // Room for the part's other fields.
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(MAX_CONTENT_BYTES - 1024).collect();
    let parts = chunks.len();
    let mut contents = vec![(
        d_tag.clone(),
        json!({
            "v": 1,
            "type": "competition",
            "competition_id": competition_id,
            "encoding": "gzip+base64",
            "parts": parts,
        })
        .to_string(),
    )];
    for (index, chunk) in chunks.into_iter().enumerate() {
        let part = index + 1;
        contents.push((
            format!("{d_tag}:part:{part}"),
            json!({
                "v": 1,
                "type": "competition_part",
                "competition_id": competition_id,
                "part": part,
                "parts": parts,
                "data": String::from_utf8_lossy(chunk),
            })
            .to_string(),
        ));
    }
    Ok(contents)
}

/// Undo [`split_competition_content`]: the content from the events' contents in `d` tag order.
pub fn join_competition_contents(contents: &[String]) -> Result<String, anyhow::Error> {
    use std::io::Read;
    let first: serde_json::Value = serde_json::from_str(
        contents
            .first()
            .ok_or_else(|| anyhow!("no competition event"))?,
    )?;
    if first.get("encoding").and_then(|e| e.as_str()) != Some("gzip+base64") {
        return Ok(contents[0].clone());
    }
    let data = match first.get("data").and_then(|data| data.as_str()) {
        Some(data) => data.to_owned(),
        None => contents[1..]
            .iter()
            .map(|part| {
                let part: serde_json::Value = serde_json::from_str(part)?;
                Ok(part
                    .get("data")
                    .and_then(|data| data.as_str())
                    .ok_or_else(|| anyhow!("competition part without data"))?
                    .to_owned())
            })
            .collect::<Result<String, anyhow::Error>>()?,
    };
    let mut content = String::new();
    flate2::read::GzDecoder::new(BASE64.decode(data)?.as_slice()).read_to_string(&mut content)?;
    Ok(content)
}

/// The signed contract as the records read it.
struct ContractView {
    params: ContractParameters,
    params_sha256: String,
    signatures: ContractSignatures,
    funding_outpoint: OutPoint,
    /// The outcome the contract settles on, once the oracle attested or the expiry passed.
    outcome: Option<Outcome>,
}

impl ContractView {
    fn read(competition: &RecoveryCompetitionRow) -> Result<Option<Self>, anyhow::Error> {
        let (Some(params), Some(signed)) = (
            competition.contract_parameters.as_deref(),
            competition.signed_contract.as_deref(),
        ) else {
            return Ok(None);
        };
        let params: ContractParameters = serde_json::from_slice(params)?;
        let signed: StoredSignedContract = serde_json::from_slice(signed)?;
        let attestation = competition
            .attestation
            .as_deref()
            .map(serde_json::from_slice::<MaybeScalar>)
            .transpose()?;
        let event = competition
            .event_announcement
            .as_deref()
            .map(serde_json::from_slice::<EventLockingConditions>)
            .transpose()?;
        let outcome = match (attestation, event) {
            (Some(attestation), Some(event)) => {
                let point = attestation.base_point_mul();
                event
                    .locking_points
                    .iter()
                    .position(|locking_point| *locking_point == point)
                    .map(Outcome::Attestation)
            }
            _ => competition.expiry_broadcasted.then_some(Outcome::Expiry),
        };
        Ok(Some(Self {
            params_sha256: sha256_hex(&serde_json::to_vec(&params)?),
            params,
            signatures: signed.signatures,
            funding_outpoint: signed.dlc.funding_outpoint,
            outcome,
        }))
    }
}

/// The entry id a ticket's record is published under: its entry's, or the one its payout policy
/// names before the entry is submitted.
pub fn ticket_entry_id(ticket: &RecoveryTicketRow) -> Option<Uuid> {
    ticket.entry_id.or_else(|| policy_entry_id(ticket))
}

/// The entry id a ticket's payout policy names, for a ticket no entry used yet.
fn policy_entry_id(ticket: &RecoveryTicketRow) -> Option<Uuid> {
    let policy: serde_json::Value = serde_json::from_str(ticket.policy_json.as_ref()?).ok()?;
    let terms = ["queued_entry", "contract_terms"]
        .iter()
        .filter_map(|field| policy.get(field)?.as_str())
        .find(|terms| !terms.is_empty())?;
    let terms: serde_json::Value = serde_json::from_str(terms).ok()?;
    terms.get("entry_id")?.as_str()?.parse().ok()
}

fn status(ticket: &RecoveryTicketRow, contract: Option<(&ContractView, usize)>) -> RecordStatus {
    if matches!(ticket.refund_state.as_deref(), Some("paid" | "settled")) {
        return RecordStatus::Refunded;
    }
    if ticket.paid_out || ticket.closed_on_chain {
        return RecordStatus::Settled;
    }
    if let Some((contract, index)) = contract {
        return match contract.outcome {
            Some(outcome) => {
                let won = contract
                    .params
                    .outcome_payouts
                    .get(&outcome)
                    .and_then(|weights| weights.get(&index))
                    .is_some_and(|weight| *weight > 0);
                if won {
                    RecordStatus::Won
                } else {
                    RecordStatus::Lost
                }
            }
            None => RecordStatus::InContract,
        };
    }
    if ticket.settled {
        RecordStatus::Paid
    } else if ticket.vtxo_outpoint.is_some() {
        RecordStatus::Escrowed
    } else {
        RecordStatus::Ticketed
    }
}

/// The signatures a player needs, as dlctix's `SignedContract::pruned_signatures` picks them,
/// from the parameters alone.
pub fn pruned_signatures(
    params: &ContractParameters,
    signatures: &ContractSignatures,
    player: Point,
) -> Option<ContractSignatures> {
    if player == params.market_maker.pubkey {
        return Some(signatures.clone());
    }
    let conditions = params.win_conditions_claimable_by_pubkey(player)?;
    let outcomes: BTreeSet<Outcome> = conditions
        .iter()
        .map(|condition| condition.outcome)
        .collect();
    Some(ContractSignatures {
        expiry_tx_signature: signatures
            .expiry_tx_signature
            .filter(|_| outcomes.contains(&Outcome::Expiry)),
        outcome_tx_signatures: signatures
            .outcome_tx_signatures
            .iter()
            .filter(|(index, _)| outcomes.contains(&Outcome::Attestation(**index)))
            .map(|(index, signature)| (*index, *signature))
            .collect(),
        split_tx_signatures: signatures
            .split_tx_signatures
            .iter()
            .filter(|(condition, _)| conditions.contains(*condition))
            .map(|(condition, signature)| (*condition, *signature))
            .collect(),
    })
}

/// A new version's `created_at`: now, but always after the version it replaces, so relays keep
/// the newer one.
pub fn next_created_at(now: i64, previous: Option<i64>) -> i64 {
    previous.map_or(now, |previous| now.max(previous + 1))
}

pub fn content_digest(content: &str) -> String {
    sha256_hex(content.as_bytes())
}
