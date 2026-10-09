//! Authorized application storage for private protocol retries. The Keymeld
//! gateway has no access to these records. Persist before any enclave side effect.
use super::{
    protocol_parts::{EntryCache, Manifest, Parts},
    KeymeldError, StoredDlcKeygenSession,
};
use crate::{config::CheckpointFormat, infra::db::DBConnection};
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
    crate::metrics::checkpoint_bytes("monolithic", "encode", plaintext.len());
    key.encrypt(&plaintext, &context(session, version))
        .and_then(|encrypted| encrypted.to_hex())
        .map_err(|_| failure("Cannot encrypt confidential protocol checkpoint"))
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
}

pub(super) async fn load(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
) -> Result<Option<Loaded>, KeymeldError> {
    let Some((version, state, parts)) =
        load_record::<ProtocolState>(db, key, session, None).await?
    else {
        return Ok(None);
    };
    validate_identity(state.schema_version, &state.session.session_id, session)?;
    Ok(Some(Loaded {
        version,
        state,
        parts,
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

async fn load_record<T: DeserializeOwned>(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
    fields: Option<&[&str]>,
) -> Result<Option<(i64, T, BTreeSet<[u8; 32]>)>, KeymeldError> {
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
    let encrypted = EncryptedData::from_hex(&encrypted)
        .map_err(|_| failure("Invalid confidential checkpoint encoding"))?;
    let plaintext = Zeroizing::new(
        key.decrypt(&encrypted, &context(session, version))
            .map_err(|_| failure("Confidential checkpoint authentication failed"))?,
    );
    let format: i64 = row.try_get("format").map_err(|e| failure(e.to_string()))?;
    let mut parts = BTreeSet::new();
    let state = match format {
        1 => {
            crate::metrics::checkpoint_bytes("monolithic", "decode", plaintext.len());
            serde_json::from_slice::<T>(&plaintext)
                .map_err(|_| failure("Invalid confidential checkpoint schema"))?
        }
        2 => {
            let manifest: Manifest = serde_json::from_slice(&plaintext)
                .map_err(|_| failure("Invalid confidential checkpoint manifest"))?;
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
            let state = manifest.decode_as::<T>(
                key,
                session,
                &bodies,
                if fields.is_some() {
                    "metadata_decode"
                } else {
                    "decode"
                },
            )?;
            manifest.digests(&mut parts);
            state
        }
        _ => return Err(failure("Unsupported confidential checkpoint format")),
    };
    Ok(Some((version, state, parts)))
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

pub(super) struct DurableCheckpoint {
    db: DBConnection,
    key: SessionSecret,
    session: SessionId,
    current: Mutex<(i64, Snapshot)>,
}
impl DurableCheckpoint {
    /// A writer that resumes from `loaded`. Its first write is a compare-and-swap against
    /// `loaded.version`, and every committed write keeps exactly the parts its manifest
    /// references. While that swap can succeed, every loaded part is still stored, so the
    /// writer reuses them instead of encrypting and inserting them again. A part's ciphertext
    /// is bound to the session and its content address, never to a version.
    pub fn new(
        db: DBConnection,
        key: SessionSecret,
        session: SessionId,
        loaded: &Loaded,
        format: CheckpointFormat,
    ) -> Result<Self, KeymeldError> {
        let snapshot = match format {
            CheckpointFormat::Parts => {
                let mut base = Parts::base(&key, &session, &loaded.state, &loaded.parts)?;
                Snapshot::Parts {
                    pending: Arc::new(std::mem::take(&mut base.bodies)),
                    base,
                    committed: loaded.parts.clone(),
                    entries: EntryCache::default(),
                }
            }
            CheckpointFormat::Monolithic => Snapshot::Legacy(Box::new(loaded.state.clone())),
        };
        Ok(Self {
            db,
            key,
            session,
            current: Mutex::new((loaded.version, snapshot)),
        })
    }
    async fn persist(&self, previous: i64, write: CheckpointWrite<'_>) -> Result<i64, SdkError> {
        let next = previous.checked_add(1).ok_or_else(|| {
            SdkError::Internal("Confidential checkpoint version exhausted".into())
        })?;
        // A format-1 row keeps no parts, so its write removes every stored part.
        let mut keep = BTreeSet::new();
        let (encrypted, format, base, bodies) = match write {
            CheckpointWrite::Monolithic(state) => (
                seal(&self.key, &self.session, next, state)
                    .map_err(|e| SdkError::Internal(e.to_string()))?,
                1,
                Arc::default(),
                BTreeMap::new(),
            ),
            CheckpointWrite::Parts { parts, base } => {
                crate::metrics::checkpoint_bytes("parts", "encode", parts.serialized_len);
                crate::metrics::checkpoint_encode_buffer_bytes(parts.max_buffer_capacity);
                let plaintext = Zeroizing::new(
                    serde_json::to_vec(&parts.manifest)
                        .map_err(|e| SdkError::Internal(e.to_string()))?,
                );
                let encrypted = self
                    .key
                    .encrypt(&plaintext, &context(&self.session, next))
                    .and_then(|value| value.to_hex())
                    .map_err(|_| SdkError::Internal("Cannot seal checkpoint manifest".into()))?;
                parts.manifest.digests(&mut keep);
                (encrypted, 2, base, parts.bodies)
            }
        };
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
        Ok(next)
    }
    pub async fn finish(&self, state: &ProtocolState) -> Result<(), SdkError> {
        let mut current = self.current.lock().await;
        let (version, snapshot) = &mut *current;
        match snapshot {
            Snapshot::Legacy(_) => {
                let next = self
                    .persist(*version, CheckpointWrite::Monolithic(state))
                    .await?;
                *snapshot = Snapshot::Legacy(Box::new(state.clone()));
                *version = next;
            }
            Snapshot::Parts {
                committed, entries, ..
            } => {
                let mut base = Parts::base(&self.key, &self.session, state, committed)
                    .map_err(|e| SdkError::Internal(e.to_string()))?;
                // A failed write drops this base, and the snapshot keeps the previous one.
                let pending = Arc::new(std::mem::take(&mut base.bodies));
                let mut parts = Parts::journal(
                    &self.key,
                    &self.session,
                    &base,
                    &state.journal,
                    committed,
                    entries,
                )
                .map_err(|e| SdkError::Internal(e.to_string()))?;
                let mut digests = BTreeSet::new();
                parts.manifest.digests(&mut digests);
                let next_entries = std::mem::take(&mut parts.entries);
                let write = CheckpointWrite::Parts {
                    parts,
                    base: pending,
                };
                let next = self.persist(*version, write).await?;
                base.serialized_len = 0;
                base.max_buffer_capacity = 0;
                *snapshot = Snapshot::Parts {
                    base,
                    pending: Arc::default(),
                    committed: digests,
                    entries: next_entries,
                };
                *version = next;
                #[cfg(test)]
                tests::assert_stored_state(self, next, state).await;
            }
        }
        Ok(())
    }
}
impl ConfidentialCheckpoint for DurableCheckpoint {
    fn save<'a>(&'a self, journal: &'a ConfidentialJournal) -> CheckpointFuture<'a> {
        Box::pin(async move {
            let mut current = self.current.lock().await;
            let (version, snapshot) = &mut *current;
            match snapshot {
                Snapshot::Legacy(previous) => {
                    let mut state = (**previous).clone();
                    state.journal = journal.clone();
                    let next = self
                        .persist(*version, CheckpointWrite::Monolithic(&state))
                        .await?;
                    *snapshot = Snapshot::Legacy(Box::new(state));
                    *version = next;
                }
                Snapshot::Parts {
                    base,
                    pending,
                    committed,
                    entries,
                } => {
                    let mut parts =
                        Parts::journal(&self.key, &self.session, base, journal, committed, entries)
                            .map_err(|e| SdkError::Internal(e.to_string()))?;
                    let mut digests = BTreeSet::new();
                    parts.manifest.digests(&mut digests);
                    let next_entries = std::mem::take(&mut parts.entries);
                    let write = CheckpointWrite::Parts {
                        parts,
                        base: Arc::clone(pending),
                    };
                    let next = self.persist(*version, write).await?;
                    // Failed or cancelled writes never advance this cache. A later
                    // retry must perform CAS before any enclave command can run.
                    *pending = Arc::default();
                    base.serialized_len = 0;
                    base.max_buffer_capacity = 0;
                    *committed = digests;
                    *entries = next_entries;
                    *version = next;
                    #[cfg(test)]
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
