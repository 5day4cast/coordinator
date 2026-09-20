//! Authorized application storage for private protocol retries. The Keymeld
//! gateway has no access to these records. Persist before any enclave side effect.
use super::{KeymeldError, StoredDlcKeygenSession};
use crate::infra::db::DBConnection;
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
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::BTreeMap;
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

fn failure(message: impl Into<String>) -> KeymeldError {
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
    key.encrypt(&plaintext, &context(session, version))
        .and_then(|encrypted| encrypted.to_hex())
        .map_err(|_| failure("Cannot encrypt confidential protocol checkpoint"))
}

pub(super) async fn load(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
) -> Result<Option<(i64, ProtocolState)>, KeymeldError> {
    let row = sqlx::query(
        "SELECT version, encrypted_state FROM keymeld_protocol_state WHERE session_id = ?",
    )
    .bind(session.to_string())
    .fetch_optional(db.read())
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
    let state: ProtocolState = serde_json::from_slice(&plaintext)
        .map_err(|_| failure("Invalid confidential checkpoint schema"))?;
    if state.schema_version != 1 || state.session.session_id != session.to_string() {
        return Err(failure(
            "Confidential checkpoint belongs to another session or version",
        ));
    }
    Ok(Some((version, state)))
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

pub(super) struct DurableCheckpoint {
    db: DBConnection,
    key: SessionSecret,
    session: SessionId,
    current: Mutex<(i64, ProtocolState)>,
}
impl DurableCheckpoint {
    pub fn new(
        db: DBConnection,
        key: SessionSecret,
        session: SessionId,
        version: i64,
        state: ProtocolState,
    ) -> Self {
        Self {
            db,
            key,
            session,
            current: Mutex::new((version, state)),
        }
    }
    async fn persist(&self, previous: i64, state: &ProtocolState) -> Result<i64, SdkError> {
        let next = previous.checked_add(1).ok_or_else(|| {
            SdkError::Internal("Confidential checkpoint version exhausted".into())
        })?;
        let encrypted = seal(&self.key, &self.session, next, state)
            .map_err(|e| SdkError::Internal(e.to_string()))?;
        let id = self.session.to_string();
        let count=self.db.execute_write(move |pool|async move {
            sqlx::query("UPDATE keymeld_protocol_state SET version=?, encrypted_state=? WHERE session_id=? AND version=?")
                .bind(next).bind(encrypted).bind(id).bind(previous).execute(&pool).await.map(|result|result.rows_affected())
        }).await.map_err(|e|SdkError::Internal(format!("Confidential checkpoint write failed: {e}")))?;
        if count != 1 {
            return Err(SdkError::Internal(
                "Concurrent confidential checkpoint changed; reload before retrying".into(),
            ));
        }
        Ok(next)
    }
    pub async fn finish(&self, state: ProtocolState) -> Result<(), SdkError> {
        let mut current = self.current.lock().await;
        let next = self.persist(current.0, &state).await?;
        *current = (next, state);
        Ok(())
    }
}
impl ConfidentialCheckpoint for DurableCheckpoint {
    fn save<'a>(&'a self, journal: &'a ConfidentialJournal) -> CheckpointFuture<'a> {
        Box::pin(async move {
            let mut current = self.current.lock().await;
            let mut state = current.1.clone();
            state.journal = journal.clone();
            let next = self.persist(current.0, &state).await?;
            *current = (next, state);
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "confidential_store_tests.rs"]
mod tests;
