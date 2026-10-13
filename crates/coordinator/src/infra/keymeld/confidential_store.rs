//! Authorized application storage for private protocol retries. The Keymeld
//! gateway has no access to these records. Persist before any enclave side effect.
use super::{
    protocol_parts::{self, EntryCache, Manifest, Nodes, PartLengths, Parts, Shape, Tree},
    DeletedSigningState, KeymeldError, StoredDlcKeygenSession,
};
use crate::{
    config::CheckpointFormat,
    infra::db::DBConnection,
    metrics::{
        checkpoint_bytes, checkpoint_deleted, checkpoint_encode_buffer_bytes,
        checkpoint_written_bytes,
    },
};
use keymeld_core::{
    authorization::SignedRoster,
    crypto::{EncryptedData, SessionSecret},
    escrow::{protocol::EscrowResponse, SignedEscrowPolicy},
    protocol::ParticipantRegistrationData,
    EnclaveId, SessionId, UserId,
};
use keymeld_sdk::{
    confidential_session::{CheckpointFuture, ConfidentialCheckpoint, ConfidentialJournal},
    dlctix::DlcBatchItems,
    SdkError,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SigningPlan {
    pub input_digest: [u8; 32],
    pub session_id: SessionId,
    pub batch: DlcBatchItems,
    #[serde(default)]
    pub prior_preparations: BTreeMap<UserId, EscrowResponse>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SettlementPlan {
    pub claim_id: Uuid,
    pub prior_preparations: BTreeMap<String, EscrowResponse>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ProtocolState {
    pub schema_version: u16,
    pub session: StoredDlcKeygenSession,
    pub epochs: BTreeMap<EnclaveId, u64>,
    pub registrations: BTreeMap<UserId, ParticipantRegistrationData>,
    pub policies: BTreeMap<UserId, SignedEscrowPolicy>,
    pub journal: ConfidentialJournal,
    pub roster: Option<SignedRoster>,
    pub bindings: BTreeMap<UserId, EscrowResponse>,
    pub signing: Option<SigningPlan>,
    #[serde(default)]
    pub settlements: BTreeMap<UserId, SettlementPlan>,
}

pub(super) fn failure(message: impl Into<String>) -> KeymeldError {
    KeymeldError::Session(message.into())
}

/// Run checkpoint encoding or decoding on Tokio's blocking pool. Serde, gzip, AEAD and HMAC
/// over a state of up to 512 MiB would otherwise hold an async worker, and every task queued
/// on it, for as long as they take. The work owns its inputs: if the caller is cancelled, the
/// work still runs to its end and then drops them.
async fn off_runtime<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, KeymeldError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| failure("Confidential checkpoint encoding or decoding did not complete"))
}

fn context(session: &SessionId, version: i64) -> String {
    format!("coordinator-confidential-protocol-v1/{session}/{version}")
}
fn seal(
    key: &SessionSecret,
    session: &SessionId,
    version: i64,
    state: &ProtocolState,
) -> Result<String, KeymeldError> {
    let plaintext = Zeroizing::new(serde_json::to_vec(state).map_err(|e| failure(e.to_string()))?);
    checkpoint_bytes("monolithic", "encode", plaintext.len());
    key.encrypt(&plaintext, &context(session, version))
        .and_then(|encrypted| encrypted.to_hex())
        .map_err(|_| failure("Cannot encrypt confidential protocol checkpoint"))
}
/// Authenticate a row's ciphertext for its version.
fn open(
    key: &SessionSecret,
    session: &SessionId,
    version: i64,
    encrypted: &str,
) -> Result<Zeroizing<Vec<u8>>, KeymeldError> {
    let encrypted = EncryptedData::from_hex(encrypted)
        .map_err(|_| failure("Invalid confidential checkpoint encoding"))?;
    Ok(Zeroizing::new(
        key.decrypt(&encrypted, &context(session, version))
            .map_err(|_| failure("Confidential checkpoint authentication failed"))?,
    ))
}

/// Read-only checkpoint fields used by registration and readiness checks.
/// These reads never reconstruct the signing journal or settlement payloads.
#[derive(Deserialize)]
pub(super) struct ProtocolMetadata {
    pub schema_version: u16,
    pub session: StoredDlcKeygenSession,
    pub epochs: BTreeMap<EnclaveId, u64>,
    pub registrations: BTreeMap<UserId, ParticipantRegistrationData>,
    pub roster: Option<SignedRoster>,
}

pub(super) async fn load_metadata(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
) -> Result<Option<(i64, ProtocolMetadata)>, KeymeldError> {
    let result = load_record::<ProtocolMetadata>(
        db,
        key,
        session,
        Some(&[
            "schema_version",
            "session",
            "epochs",
            "registrations",
            "roster",
        ]),
    )
    .await?
    .map(|(version, state, _)| (version, state));
    if let Some((_, state)) = &result {
        validate_identity(state.schema_version, &state.session.session_id, session)?;
    }
    Ok(result)
}

/// One checkpoint version, read from one database snapshot and authenticated.
pub(super) struct Loaded {
    pub version: i64,
    pub state: ProtocolState,
    /// The parts this version's manifest references, and in format 3 the manifest nodes; none
    /// in format 1. Loading decrypted each one and checked it against its content address.
    parts: BTreeSet<[u8; 32]>,
    /// The stored encoding of each journal entry, under the identity this load gave the entry.
    entries: EntryCache,
}

pub(super) async fn load(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
) -> Result<Option<Loaded>, KeymeldError> {
    let Some((version, state, authenticated)) =
        load_record::<ProtocolState>(db, key, session, None).await?
    else {
        return Ok(None);
    };
    validate_identity(state.schema_version, &state.session.session_id, session)?;
    let mut parts = BTreeSet::new();
    let entries = match authenticated {
        Some(Authenticated {
            manifest,
            lengths,
            nodes,
        }) => {
            manifest.digests(&mut parts);
            parts.extend(nodes);
            EntryCache::loaded(&manifest, &state.journal, &lengths)
        }
        None => EntryCache::default(),
    };
    Ok(Some(Loaded {
        version,
        state,
        parts,
        entries,
    }))
}

fn validate_identity(version: u16, stored: &str, session: &SessionId) -> Result<(), KeymeldError> {
    if version != 1 || stored != session.to_string() {
        return Err(failure(
            "Confidential checkpoint belongs to another session or version",
        ));
    }
    Ok(())
}

/// A row's authenticated plaintext, parsed by its format: format 1 holds the value itself,
/// format 2 a manifest over parts, and format 3 the root of a manifest kept in nodes.
enum Opened<T> {
    State(T),
    Manifest(Manifest),
    Tree(Tree),
}

/// What a format-2 or format-3 read authenticated besides the value: the manifest it decoded,
/// the plaintext length of each part, and in format 3 the nodes it rebuilt the manifest from.
struct Authenticated {
    manifest: Manifest,
    lengths: PartLengths,
    nodes: BTreeSet<[u8; 32]>,
}

/// The stored bodies of `digests`, read in `snapshot`, at most 200 to a query. A digest without
/// a row is left out, and decoding then fails closed.
async fn fetch_bodies(
    snapshot: &mut sqlx::SqliteConnection,
    session: &SessionId,
    digests: &BTreeSet<[u8; 32]>,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, KeymeldError> {
    let digests: Vec<_> = digests.iter().collect();
    let mut bodies = BTreeMap::new();
    for batch in digests.chunks(200) {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT digest, body FROM keymeld_protocol_parts WHERE session_id = ",
        );
        query
            .push_bind(session.to_string())
            .push(" AND digest IN (");
        let mut list = query.separated(",");
        for digest in batch {
            list.push_bind(digest.as_slice());
        }
        list.push_unseparated(")");
        for row in query
            .build()
            .fetch_all(&mut *snapshot)
            .await
            .map_err(|e| failure(e.to_string()))?
        {
            bodies.insert(
                row.try_get::<Vec<u8>, _>("digest")
                    .map_err(|e| failure(e.to_string()))?,
                row.try_get::<Vec<u8>, _>("body")
                    .map_err(|e| failure(e.to_string()))?,
            );
        }
    }
    Ok(bodies)
}

async fn load_record<T: DeserializeOwned + Send + 'static>(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
    fields: Option<&[&str]>,
) -> Result<Option<(i64, T, Option<Authenticated>)>, KeymeldError> {
    let mut snapshot = db
        .read()
        .begin()
        .await
        .map_err(|e| failure(e.to_string()))?;
    let row = sqlx::query(
        "SELECT version, encrypted_state, format FROM keymeld_protocol_state WHERE session_id = ?",
    )
    .bind(session.to_string())
    .fetch_optional(&mut *snapshot)
    .await
    .map_err(|e| failure(e.to_string()))?;
    let Some(row) = row else { return Ok(None) };
    let version: i64 = row.try_get("version").map_err(|e| failure(e.to_string()))?;
    let encrypted: String = row
        .try_get("encrypted_state")
        .map_err(|e| failure(e.to_string()))?;
    let format: i64 = row.try_get("format").map_err(|e| failure(e.to_string()))?;
    let (owned_key, id) = (key.clone(), session.clone());
    let opened = off_runtime(move || {
        let plaintext = open(&owned_key, &id, version, &encrypted)?;
        match format {
            1 => {
                checkpoint_bytes("monolithic", "decode", plaintext.len());
                serde_json::from_slice::<T>(&plaintext)
                    .map(Opened::State)
                    .map_err(|_| failure("Invalid confidential checkpoint schema"))
            }
            2 => serde_json::from_slice::<Manifest>(&plaintext)
                .map(Opened::Manifest)
                .map_err(|_| failure("Invalid confidential checkpoint manifest")),
            3 => serde_json::from_slice::<Tree>(&plaintext)
                .map(Opened::Tree)
                .map_err(|_| failure("Invalid confidential checkpoint root")),
            _ => Err(failure("Unsupported confidential checkpoint format")),
        }
    });
    // All queries share the row's read transaction, so they see the nodes and parts of the
    // version the compare-and-swap names.
    let (manifest, nodes) = match opened.await?? {
        Opened::State(state) => return Ok(Some((version, state, None))),
        Opened::Manifest(manifest) => {
            let manifest = match fields {
                Some(fields) => manifest.select_fields(fields),
                None => manifest,
            };
            (manifest, BTreeSet::new())
        }
        Opened::Tree(root) => {
            // Project before fetching, so a metadata read fetches no journal node.
            let root = match fields {
                Some(fields) => root.select_fields(fields),
                None => root,
            };
            let mut nodes = Nodes::default();
            let mut wanted = root.wanted();
            let mut rounds = 0;
            while !wanted.is_empty() {
                rounds += 1;
                if rounds > protocol_parts::MAX_DEPTH {
                    return Err(failure("Confidential checkpoint nests too deeply"));
                }
                let bodies = fetch_bodies(&mut snapshot, session, &wanted).await?;
                let (owned_key, id) = (key.clone(), session.clone());
                let added = off_runtime(move || {
                    let next = nodes.authenticate(&owned_key, &id, &wanted, &bodies)?;
                    Ok::<_, KeymeldError>((nodes, next))
                });
                (nodes, wanted) = added.await??;
            }
            let unpacked = off_runtime(move || {
                let manifest = nodes.unpack(&root)?;
                Ok::<_, KeymeldError>((manifest, nodes.digests().collect::<BTreeSet<_>>()))
            });
            unpacked.await??
        }
    };
    let mut needed = BTreeSet::new();
    manifest.digests(&mut needed);
    // Bound each SQL result and fetch only the authenticated projection.
    let bodies = fetch_bodies(&mut snapshot, session, &needed).await?;
    snapshot
        .commit()
        .await
        .map_err(|e| failure(e.to_string()))?;
    let operation = if fields.is_some() {
        "metadata_decode"
    } else {
        "decode"
    };
    let (owned_key, id) = (key.clone(), session.clone());
    let decoded = off_runtime(move || {
        let (state, lengths) = manifest.decode_as::<T>(&owned_key, &id, &bodies, operation)?;
        let authenticated = Authenticated {
            manifest,
            lengths,
            nodes,
        };
        Ok::<_, KeymeldError>((state, authenticated))
    });
    let (state, authenticated) = decoded.await??;
    Ok(Some((version, state, Some(authenticated))))
}

pub(super) async fn create(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
    state: &ProtocolState,
) -> Result<bool, KeymeldError> {
    let encrypted = seal(key, session, 0, state)?;
    let id = session.to_string();
    let count=db.execute_write(move |pool|async move {
        sqlx::query("INSERT INTO keymeld_protocol_state(session_id,version,encrypted_state) VALUES(?,0,?) ON CONFLICT(session_id) DO NOTHING")
            .bind(id).bind(encrypted).execute(&pool).await.map(|result|result.rows_affected())
    }).await.map_err(|e|failure(e.to_string()))?;
    Ok(count == 1)
}

/// How many parts and manifest nodes one write of [`delete`] removes, about a megabyte of
/// sealed bodies, so other writes wait for one batch no longer than for a checkpoint.
pub(super) const DELETE_BATCH_ROWS: i64 = 256;

/// Delete what is stored for `session`: its parts and manifest nodes, `batch` to a write and
/// at most `budget` of them, then, once none is left, its row and its refund invoice
/// authorizations in one last write. Other writes run between the batches. The caller holds
/// the session (`Keymeld::try_hold_session`), so no operation of this process uses it
/// meanwhile.
///
/// Once one batch is gone the checkpoint no longer loads, and once the row is gone a stale
/// writer's compare-and-swap matches no row, so either fails closed and stores nothing. A
/// deletion that stops part way, at its budget or otherwise, leaves the rest for the next
/// call.
pub(super) async fn delete(
    db: &DBConnection,
    session: &SessionId,
    batch: i64,
    budget: u64,
) -> Result<DeletedSigningState, KeymeldError> {
    let batch = batch.max(1);
    let id = session.to_string();
    let mut deleted = DeletedSigningState::default();
    loop {
        let left = budget.saturating_sub(deleted.parts);
        if left == 0 {
            return Ok(deleted);
        }
        let limit = batch.min(i64::try_from(left).unwrap_or(i64::MAX));
        let session_id = id.clone();
        let lengths: Vec<i64> = db
            .execute_write(move |pool| async move {
                sqlx::query_scalar(
                    "DELETE FROM keymeld_protocol_parts WHERE session_id = ? AND digest IN (
                         SELECT digest FROM keymeld_protocol_parts WHERE session_id = ? LIMIT ?)
                     RETURNING length(body)",
                )
                .bind(&session_id)
                .bind(&session_id)
                .bind(limit)
                .fetch_all(&pool)
                .await
            })
            .await
            .map_err(|e| failure(format!("Cannot delete checkpoint parts: {e}")))?;
        let bytes = lengths.iter().map(|length| *length as u64).sum::<u64>();
        checkpoint_deleted("part", lengths.len() as u64, bytes);
        deleted.parts += lengths.len() as u64;
        deleted.part_bytes += bytes;
        if (lengths.len() as i64) < limit {
            break;
        }
    }
    let (parts, row, authorizations) = db
        .execute_write(move |pool| async move {
            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
            // Nothing should be left. The row's cascade would delete a part another process
            // stored between the batches; it is counted here instead.
            let parts: Vec<i64> = sqlx::query_scalar(
                "DELETE FROM keymeld_protocol_parts WHERE session_id = ? RETURNING length(body)",
            )
            .bind(&id)
            .fetch_all(&mut *tx)
            .await?;
            let row: Vec<i64> = sqlx::query_scalar(
                "DELETE FROM keymeld_protocol_state WHERE session_id = ?
                 RETURNING length(encrypted_state)",
            )
            .bind(&id)
            .fetch_all(&mut *tx)
            .await?;
            let authorizations = sqlx::query(
                "DELETE FROM refund_invoice_authorizations WHERE keygen_session_id = ?",
            )
            .bind(&id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            Ok((parts, row, authorizations))
        })
        .await
        .map_err(|e| failure(format!("Cannot delete the checkpoint: {e}")))?;
    let part_bytes = parts.iter().map(|length| *length as u64).sum::<u64>();
    checkpoint_deleted("part", parts.len() as u64, part_bytes);
    deleted.parts += parts.len() as u64;
    deleted.part_bytes += part_bytes;
    let row_bytes = row.iter().map(|length| *length as u64).sum::<u64>();
    checkpoint_deleted("row", row.len() as u64, row_bytes);
    deleted.rows = row.len() as u64;
    deleted.row_bytes = row_bytes;
    deleted.authorizations = authorizations;
    deleted.finished = true;
    Ok(deleted)
}

/// How a writer stores what it writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    /// Format 1: one sealed document.
    Monolithic,
    /// Format 2: a sealed manifest over parts.
    Parts,
    /// Format 3: a sealed root over manifest nodes and the same parts, split as the shape says.
    Tree(Shape),
}

impl From<CheckpointFormat> for Layout {
    fn from(format: CheckpointFormat) -> Self {
        match format {
            CheckpointFormat::Monolithic => Self::Monolithic,
            CheckpointFormat::Parts => Self::Parts,
            CheckpointFormat::Tree => Self::Tree(Shape::writer()),
        }
    }
}

enum Snapshot {
    Legacy(Box<ProtocolState>),
    Parts {
        /// The non-journal fields' manifest and sizes. Their new ciphertext is in `pending`.
        base: Parts,
        /// Base ciphertext that no write has committed yet. Each write shares it rather than
        /// copying it, and a failed or cancelled write leaves it here for the retry.
        pending: Arc<BTreeMap<[u8; 32], Vec<u8>>>,
        /// Parts and manifest nodes stored at the current version: those the writer loaded,
        /// then those of its last successful CAS. Only these are reusable.
        committed: BTreeSet<[u8; 32]>,
        /// Whether `committed` comes from this writer's own last commit, and so lists exactly
        /// what is stored. A writer's first write deletes every stored part its version does
        /// not reference instead, as every write did before.
        exact: bool,
        entries: EntryCache,
        /// Write format 3 split this way; format 2 when `None`.
        tree: Option<Shape>,
    },
}

impl Snapshot {
    /// The write that stores `state` as version `next`, and the snapshot that follows it once
    /// it commits.
    fn encode_state(
        &self,
        key: &SessionSecret,
        session: &SessionId,
        next: i64,
        state: &ProtocolState,
    ) -> Result<(SealedWrite, Self), SdkError> {
        match self {
            Self::Legacy(_) => Ok((
                CheckpointWrite::Monolithic(state)
                    .into_sealed(key, session, next)?
                    .0,
                Self::Legacy(Box::new(state.clone())),
            )),
            Self::Parts {
                committed,
                exact,
                entries,
                tree,
                ..
            } => {
                let mut base = Parts::base(key, session, state, committed)
                    .map_err(|e| SdkError::Internal(e.to_string()))?;
                // A failed write drops this base, and the snapshot keeps the previous one.
                let pending = Arc::new(std::mem::take(&mut base.bodies));
                let mut parts =
                    Parts::journal(key, session, &base, &state.journal, committed, entries)
                        .map_err(|e| SdkError::Internal(e.to_string()))?;
                let next_entries = std::mem::take(&mut parts.entries);
                let (write, stored) = CheckpointWrite::Parts {
                    parts,
                    base: pending,
                    tree: *tree,
                    committed,
                    exact: *exact,
                }
                .into_sealed(key, session, next)?;
                base.serialized_len = 0;
                base.max_buffer_capacity = 0;
                Ok((
                    write,
                    Self::Parts {
                        base,
                        pending: Arc::default(),
                        committed: stored,
                        exact: true,
                        entries: next_entries,
                        tree: *tree,
                    },
                ))
            }
        }
    }

    /// As [`Self::encode_state`], for `journal` beside the fields this snapshot holds.
    fn encode_journal(
        &self,
        key: &SessionSecret,
        session: &SessionId,
        next: i64,
        journal: ConfidentialJournal,
    ) -> Result<(SealedWrite, Self), SdkError> {
        match self {
            Self::Legacy(previous) => {
                let mut state = (**previous).clone();
                state.journal = journal;
                let (write, _) =
                    CheckpointWrite::Monolithic(&state).into_sealed(key, session, next)?;
                Ok((write, Self::Legacy(Box::new(state))))
            }
            Self::Parts {
                base,
                pending,
                committed,
                exact,
                entries,
                tree,
            } => {
                let mut parts = Parts::journal(key, session, base, &journal, committed, entries)
                    .map_err(|e| SdkError::Internal(e.to_string()))?;
                let next_entries = std::mem::take(&mut parts.entries);
                let (write, stored) = CheckpointWrite::Parts {
                    parts,
                    base: Arc::clone(pending),
                    tree: *tree,
                    committed,
                    exact: *exact,
                }
                .into_sealed(key, session, next)?;
                // The base itself is unchanged; only the write that stores it counts its bytes.
                let base = Parts {
                    manifest: base.manifest.clone(),
                    bodies: BTreeMap::new(),
                    plaintext_len: base.plaintext_len,
                    serialized_len: 0,
                    max_buffer_capacity: 0,
                    entries: EntryCache::default(),
                };
                Ok((
                    write,
                    Self::Parts {
                        base,
                        pending: Arc::default(),
                        committed: stored,
                        exact: true,
                        entries: next_entries,
                        tree: *tree,
                    },
                ))
            }
        }
    }
}

/// What one compare-and-swap stores: the whole state as one sealed document, or a sealed
/// manifest (format 2) or root (format 3) and the parts and nodes it references that are not
/// stored yet.
enum CheckpointWrite<'a> {
    Monolithic(&'a ProtocolState),
    Parts {
        parts: Parts,
        /// The base's new ciphertext, shared with the writer's snapshot.
        base: Arc<BTreeMap<[u8; 32], Vec<u8>>>,
        tree: Option<Shape>,
        /// What is stored at the version this write replaces.
        committed: &'a BTreeSet<[u8; 32]>,
        /// Whether `committed` is exact; see [`Snapshot::Parts`].
        exact: bool,
    },
}

/// Which stored parts and nodes a write deletes.
enum Removal {
    /// Every one: a format-1 row keeps none.
    All,
    /// Every one the new version does not reference, read from the table.
    Unreferenced(BTreeSet<[u8; 32]>),
    /// These: stored at the replaced version and not referenced by the new one.
    Only(Vec<[u8; 32]>),
}

/// A [`CheckpointWrite`] sealed for its version.
struct SealedWrite {
    encrypted: String,
    format: i64,
    base: Arc<BTreeMap<[u8; 32], Vec<u8>>>,
    /// New parts besides the base's.
    bodies: BTreeMap<[u8; 32], Vec<u8>>,
    /// New manifest nodes.
    nodes: BTreeMap<[u8; 32], Vec<u8>>,
    remove: Removal,
}

impl CheckpointWrite<'_> {
    /// The write that stores this as version `next`, and every part and node stored once it
    /// commits.
    fn into_sealed(
        self,
        key: &SessionSecret,
        session: &SessionId,
        next: i64,
    ) -> Result<(SealedWrite, BTreeSet<[u8; 32]>), SdkError> {
        let internal = |error: KeymeldError| SdkError::Internal(error.to_string());
        Ok(match self {
            Self::Monolithic(state) => (
                SealedWrite {
                    encrypted: seal(key, session, next, state).map_err(internal)?,
                    format: 1,
                    base: Arc::default(),
                    bodies: BTreeMap::new(),
                    nodes: BTreeMap::new(),
                    remove: Removal::All,
                },
                BTreeSet::new(),
            ),
            Self::Parts {
                parts,
                base,
                tree,
                committed,
                exact,
            } => {
                checkpoint_bytes("parts", "encode", parts.serialized_len);
                checkpoint_encode_buffer_bytes(parts.max_buffer_capacity);
                let mut keep = BTreeSet::new();
                parts.manifest.digests(&mut keep);
                let (plaintext, format, nodes) = match tree {
                    None => (serde_json::to_vec(&parts.manifest), 2, BTreeMap::new()),
                    Some(shape) => {
                        // Unchanged subtrees pack to nodes `committed` lists, which this write
                        // neither seals nor inserts again.
                        let packed =
                            protocol_parts::pack(key, session, &parts.manifest, committed, shape)
                                .map_err(internal)?;
                        keep.extend(packed.nodes);
                        (serde_json::to_vec(&packed.root), 3, packed.bodies)
                    }
                };
                let plaintext =
                    Zeroizing::new(plaintext.map_err(|e| SdkError::Internal(e.to_string()))?);
                let encrypted = key
                    .encrypt(&plaintext, &context(session, next))
                    .and_then(|value| value.to_hex())
                    .map_err(|_| SdkError::Internal("Cannot seal checkpoint manifest".into()))?;
                let remove = if exact {
                    Removal::Only(committed.difference(&keep).copied().collect())
                } else {
                    Removal::Unreferenced(keep.clone())
                };
                (
                    SealedWrite {
                        encrypted,
                        format,
                        base,
                        bodies: parts.bodies,
                        nodes,
                        remove,
                    },
                    keep,
                )
            }
        })
    }
}

fn next_version(version: i64) -> Result<i64, SdkError> {
    version
        .checked_add(1)
        .ok_or_else(|| SdkError::Internal("Confidential checkpoint version exhausted".into()))
}

pub(super) struct DurableCheckpoint {
    db: DBConnection,
    key: SessionSecret,
    session: SessionId,
    /// Held for a whole write: encoding, compare-and-swap, then the snapshot that follows.
    /// The encoding owns the guard on the blocking pool, so a write cancelled meanwhile holds
    /// it until the encoding ends, then releases the snapshot unchanged.
    current: Arc<Mutex<(i64, Snapshot)>>,
}
impl DurableCheckpoint {
    /// A writer that resumes from `loaded`, and the state it loaded. Its first write is a
    /// compare-and-swap against `loaded.version`, and every committed write keeps exactly the
    /// parts its manifest references. While that swap can succeed, every loaded part is still
    /// stored, so the writer reuses them instead of encrypting and inserting them again. A
    /// part's ciphertext is bound to the session and its content address, never to a version.
    /// A journal entry that keeps the identity the load gave it also keeps its loaded
    /// encoding, so the writer does not serialize it again. In format 3 the same holds for
    /// the manifest nodes the load authenticated.
    pub async fn resume(
        db: DBConnection,
        key: SessionSecret,
        session: SessionId,
        loaded: Loaded,
        format: CheckpointFormat,
    ) -> Result<(Self, ProtocolState), KeymeldError> {
        Self::resume_in(db, key, session, loaded, format.into()).await
    }

    /// As [`Self::resume`], writing in `layout`.
    pub(super) async fn resume_in(
        db: DBConnection,
        key: SessionSecret,
        session: SessionId,
        loaded: Loaded,
        layout: Layout,
    ) -> Result<(Self, ProtocolState), KeymeldError> {
        let Loaded {
            version,
            state,
            parts,
            entries,
        } = loaded;
        let (state, snapshot) = match layout {
            Layout::Parts | Layout::Tree(_) => {
                let tree = match layout {
                    Layout::Tree(shape) => Some(shape),
                    _ => None,
                };
                let (owned_key, id) = (key.clone(), session.clone());
                let encoded = off_runtime(move || {
                    let mut base = Parts::base(&owned_key, &id, &state, &parts)?;
                    let snapshot = Snapshot::Parts {
                        pending: Arc::new(std::mem::take(&mut base.bodies)),
                        base,
                        committed: parts,
                        exact: false,
                        entries,
                        tree,
                    };
                    Ok::<_, KeymeldError>((state, snapshot))
                });
                encoded.await??
            }
            Layout::Monolithic => {
                let snapshot = Snapshot::Legacy(Box::new(state.clone()));
                (state, snapshot)
            }
        };
        let current = Arc::new(Mutex::new((version, snapshot)));
        Ok((
            Self {
                db,
                key,
                session,
                current,
            },
            state,
        ))
    }
    async fn persist(&self, previous: i64, next: i64, write: SealedWrite) -> Result<(), SdkError> {
        let SealedWrite {
            encrypted,
            format,
            base,
            bodies,
            nodes,
            remove,
        } = write;
        let written = [
            ("row", encrypted.len()),
            (
                "part",
                base.values().chain(bodies.values()).map(Vec::len).sum(),
            ),
            ("node", nodes.values().map(Vec::len).sum()),
        ];
        let id = self.session.to_string();
        let count = self.db.execute_write(move |pool| async move {
            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
            let count = sqlx::query("UPDATE keymeld_protocol_state SET version=?, encrypted_state=?, format=? WHERE session_id=? AND version=?")
                .bind(next).bind(encrypted).bind(format).bind(&id).bind(previous).execute(&mut *tx).await?.rows_affected();
            if count != 1 { tx.rollback().await?; return Ok(count); }
            for (digest, body) in base.iter().chain(&bodies).chain(&nodes) {
                sqlx::query("INSERT INTO keymeld_protocol_parts(session_id, digest, body) VALUES (?, ?, ?) ON CONFLICT(session_id,digest) DO NOTHING")
                    .bind(&id).bind(digest.to_vec()).bind(body.as_slice()).execute(&mut *tx).await?;
            }
            // The swap proved the row was at `previous`, and every write keeps exactly what its
            // version references, so a writer that made `previous` itself knows what is stored.
            match remove {
                Removal::All => {
                    sqlx::query("DELETE FROM keymeld_protocol_parts WHERE session_id = ?")
                        .bind(&id).execute(&mut *tx).await?;
                }
                Removal::Unreferenced(keep) => {
                    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT digest FROM keymeld_protocol_parts WHERE session_id = ?")
                        .bind(&id).fetch_all(&mut *tx).await?;
                    for digest in stored {
                        if !keep.contains(digest.as_slice()) {
                            sqlx::query("DELETE FROM keymeld_protocol_parts WHERE session_id = ? AND digest = ?")
                                .bind(&id).bind(digest).execute(&mut *tx).await?;
                        }
                    }
                }
                Removal::Only(digests) => {
                    for digest in digests {
                        sqlx::query("DELETE FROM keymeld_protocol_parts WHERE session_id = ? AND digest = ?")
                            .bind(&id).bind(digest.to_vec()).execute(&mut *tx).await?;
                    }
                }
            }
            tx.commit().await?;
            Ok(count)
        }).await.map_err(|e|SdkError::Internal(format!("Confidential checkpoint write failed: {e}")))?;
        if count != 1 {
            return Err(SdkError::Internal(
                "Concurrent confidential checkpoint changed; reload before retrying".into(),
            ));
        }
        for (kind, bytes) in written {
            checkpoint_written_bytes(kind, bytes);
        }
        Ok(())
    }
    /// Store `state` as the next version, and hand it back.
    pub async fn finish(&self, state: ProtocolState) -> Result<ProtocolState, SdkError> {
        let current = Arc::clone(&self.current).lock_owned().await;
        let next = next_version(current.0)?;
        let (key, session) = (self.key.clone(), self.session.clone());
        let encoded = off_runtime(move || {
            let encoded = current.1.encode_state(&key, &session, next, &state);
            (current, state, encoded)
        });
        let (mut current, state, encoded) = encoded
            .await
            .map_err(|e| SdkError::Internal(e.to_string()))?;
        let (write, following) = encoded?;
        self.persist(current.0, next, write).await?;
        *current = (next, following);
        #[cfg(test)]
        {
            tests::assert_stored_parts(self, &current.1).await;
            if matches!(current.1, Snapshot::Parts { .. }) {
                tests::assert_stored_state(self, next, &state).await;
            }
        }
        Ok(state)
    }
}
impl ConfidentialCheckpoint for DurableCheckpoint {
    fn save<'a>(&'a self, journal: &'a ConfidentialJournal) -> CheckpointFuture<'a> {
        Box::pin(async move {
            let current = Arc::clone(&self.current).lock_owned().await;
            let next = next_version(current.0)?;
            let (key, session) = (self.key.clone(), self.session.clone());
            // A clone shares the journal's entries and their identities; it copies no payload.
            let owned = journal.clone();
            let encoded = off_runtime(move || {
                let encoded = current.1.encode_journal(&key, &session, next, owned);
                (current, encoded)
            });
            let (mut current, encoded) = encoded
                .await
                .map_err(|e| SdkError::Internal(e.to_string()))?;
            let (write, following) = encoded?;
            self.persist(current.0, next, write).await?;
            // Failed or cancelled writes never advance this cache. A later
            // retry must perform CAS before any enclave command can run.
            *current = (next, following);
            #[cfg(test)]
            {
                tests::assert_stored_parts(self, &current.1).await;
                if matches!(current.1, Snapshot::Parts { .. }) {
                    tests::assert_stored_journal(self, next, journal).await;
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "confidential_store_tests.rs"]
mod tests;
