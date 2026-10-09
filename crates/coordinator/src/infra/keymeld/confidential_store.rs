//! Authorized application storage for private protocol retries. The Keymeld
//! gateway has no access to these records. Persist before any enclave side effect.
use super::{
    protocol_parts::{EntryCache, Manifest, PartLengths, Parts},
    KeymeldError, StoredDlcKeygenSession,
};
use crate::{
    config::CheckpointFormat,
    infra::db::DBConnection,
    metrics::{checkpoint_bytes, checkpoint_encode_buffer_bytes},
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
    /// The parts this version's manifest references; none in format 1. Loading decrypted each
    /// one and checked it against its content address.
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
        Some(Authenticated { manifest, lengths }) => {
            manifest.digests(&mut parts);
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
/// format 2 a manifest over parts.
enum Opened<T> {
    State(T),
    Manifest(Manifest),
}

/// What a format-2 read authenticated besides the value: the manifest it decoded, and the
/// plaintext length of each part.
struct Authenticated {
    manifest: Manifest,
    lengths: PartLengths,
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
            _ => Err(failure("Unsupported confidential checkpoint format")),
        }
    });
    let (state, authenticated) = match opened.await?? {
        Opened::State(state) => (state, None),
        Opened::Manifest(manifest) => {
            let manifest = match fields {
                Some(fields) => manifest.select_fields(fields),
                None => manifest,
            };
            let mut needed = BTreeSet::new();
            manifest.digests(&mut needed);
            let digests: Vec<_> = needed.into_iter().collect();
            let mut bodies = BTreeMap::new();
            // Bound each SQL result and fetch only the authenticated projection.
            // All queries share the manifest's read transaction / CAS version.
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
                let (state, lengths) =
                    manifest.decode_as::<T>(&owned_key, &id, &bodies, operation)?;
                Ok::<_, KeymeldError>((state, Some(Authenticated { manifest, lengths })))
            });
            decoded.await??
        }
    };
    Ok(Some((version, state, authenticated)))
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

enum Snapshot {
    Legacy(Box<ProtocolState>),
    Parts {
        /// The non-journal fields' manifest and sizes. Their new ciphertext is in `pending`.
        base: Parts,
        /// Base ciphertext that no write has committed yet. Each write shares it rather than
        /// copying it, and a failed or cancelled write leaves it here for the retry.
        pending: Arc<BTreeMap<[u8; 32], Vec<u8>>>,
        /// Parts stored at the current version: those the writer loaded, then those of its
        /// last successful CAS. Only these are reusable.
        committed: BTreeSet<[u8; 32]>,
        entries: EntryCache,
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
                CheckpointWrite::Monolithic(state).into_sealed(key, session, next)?,
                Self::Legacy(Box::new(state.clone())),
            )),
            Self::Parts {
                committed, entries, ..
            } => {
                let mut base = Parts::base(key, session, state, committed)
                    .map_err(|e| SdkError::Internal(e.to_string()))?;
                // A failed write drops this base, and the snapshot keeps the previous one.
                let pending = Arc::new(std::mem::take(&mut base.bodies));
                let mut parts =
                    Parts::journal(key, session, &base, &state.journal, committed, entries)
                        .map_err(|e| SdkError::Internal(e.to_string()))?;
                let mut digests = BTreeSet::new();
                parts.manifest.digests(&mut digests);
                let next_entries = std::mem::take(&mut parts.entries);
                let write = CheckpointWrite::Parts {
                    parts,
                    base: pending,
                }
                .into_sealed(key, session, next)?;
                base.serialized_len = 0;
                base.max_buffer_capacity = 0;
                Ok((
                    write,
                    Self::Parts {
                        base,
                        pending: Arc::default(),
                        committed: digests,
                        entries: next_entries,
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
                let write = CheckpointWrite::Monolithic(&state).into_sealed(key, session, next)?;
                Ok((write, Self::Legacy(Box::new(state))))
            }
            Self::Parts {
                base,
                pending,
                committed,
                entries,
            } => {
                let mut parts = Parts::journal(key, session, base, &journal, committed, entries)
                    .map_err(|e| SdkError::Internal(e.to_string()))?;
                let mut digests = BTreeSet::new();
                parts.manifest.digests(&mut digests);
                let next_entries = std::mem::take(&mut parts.entries);
                let write = CheckpointWrite::Parts {
                    parts,
                    base: Arc::clone(pending),
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
                        committed: digests,
                        entries: next_entries,
                    },
                ))
            }
        }
    }
}

/// What one compare-and-swap stores: the whole state as one sealed document, or a sealed
/// manifest and the parts it references that are not stored yet.
enum CheckpointWrite<'a> {
    Monolithic(&'a ProtocolState),
    Parts {
        parts: Parts,
        /// The base's new ciphertext, shared with the writer's snapshot.
        base: Arc<BTreeMap<[u8; 32], Vec<u8>>>,
    },
}

/// A [`CheckpointWrite`] sealed for its version.
struct SealedWrite {
    encrypted: String,
    format: i64,
    /// The parts the new manifest references. The write deletes the session's others.
    keep: BTreeSet<[u8; 32]>,
    base: Arc<BTreeMap<[u8; 32], Vec<u8>>>,
    bodies: BTreeMap<[u8; 32], Vec<u8>>,
}

impl CheckpointWrite<'_> {
    fn into_sealed(
        self,
        key: &SessionSecret,
        session: &SessionId,
        next: i64,
    ) -> Result<SealedWrite, SdkError> {
        // A format-1 row keeps no parts, so its write removes every stored part.
        let mut keep = BTreeSet::new();
        Ok(match self {
            Self::Monolithic(state) => SealedWrite {
                encrypted: seal(key, session, next, state)
                    .map_err(|e| SdkError::Internal(e.to_string()))?,
                format: 1,
                keep,
                base: Arc::default(),
                bodies: BTreeMap::new(),
            },
            Self::Parts { parts, base } => {
                checkpoint_bytes("parts", "encode", parts.serialized_len);
                checkpoint_encode_buffer_bytes(parts.max_buffer_capacity);
                let plaintext = Zeroizing::new(
                    serde_json::to_vec(&parts.manifest)
                        .map_err(|e| SdkError::Internal(e.to_string()))?,
                );
                let encrypted = key
                    .encrypt(&plaintext, &context(session, next))
                    .and_then(|value| value.to_hex())
                    .map_err(|_| SdkError::Internal("Cannot seal checkpoint manifest".into()))?;
                parts.manifest.digests(&mut keep);
                SealedWrite {
                    encrypted,
                    format: 2,
                    keep,
                    base,
                    bodies: parts.bodies,
                }
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
    /// encoding, so the writer does not serialize it again.
    pub async fn resume(
        db: DBConnection,
        key: SessionSecret,
        session: SessionId,
        loaded: Loaded,
        format: CheckpointFormat,
    ) -> Result<(Self, ProtocolState), KeymeldError> {
        let Loaded {
            version,
            state,
            parts,
            entries,
        } = loaded;
        let (state, snapshot) = match format {
            CheckpointFormat::Parts => {
                let (owned_key, id) = (key.clone(), session.clone());
                let encoded = off_runtime(move || {
                    let mut base = Parts::base(&owned_key, &id, &state, &parts)?;
                    let snapshot = Snapshot::Parts {
                        pending: Arc::new(std::mem::take(&mut base.bodies)),
                        base,
                        committed: parts,
                        entries,
                    };
                    Ok::<_, KeymeldError>((state, snapshot))
                });
                encoded.await??
            }
            CheckpointFormat::Monolithic => {
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
            keep,
            base,
            bodies,
        } = write;
        let id = self.session.to_string();
        let count = self.db.execute_write(move |pool| async move {
            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
            let count = sqlx::query("UPDATE keymeld_protocol_state SET version=?, encrypted_state=?, format=? WHERE session_id=? AND version=?")
                .bind(next).bind(encrypted).bind(format).bind(&id).bind(previous).execute(&mut *tx).await?.rows_affected();
            if count != 1 { tx.rollback().await?; return Ok(count); }
            for (digest, body) in base.iter().chain(&bodies) {
                sqlx::query("INSERT INTO keymeld_protocol_parts(session_id, digest, body) VALUES (?, ?, ?) ON CONFLICT(session_id,digest) DO NOTHING")
                    .bind(&id).bind(digest.to_vec()).bind(body.as_slice()).execute(&mut *tx).await?;
            }
            let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT digest FROM keymeld_protocol_parts WHERE session_id = ?")
                .bind(&id).fetch_all(&mut *tx).await?;
            for digest in stored {
                if !keep.contains(digest.as_slice()) {
                    sqlx::query("DELETE FROM keymeld_protocol_parts WHERE session_id = ? AND digest = ?")
                        .bind(&id).bind(digest).execute(&mut *tx).await?;
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
