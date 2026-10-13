use super::*;
use crate::infra::db::{DatabasePoolConfig, DatabaseType};
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use futures::FutureExt;
use keymeld_core::{
    authorization::{
        EnclaveRecipientAuthorization, SessionAuthorizationManifest, SignedSessionManifest,
    },
    protocol::{
        EnclaveCommand, GetAggregatePublicKeyCommand, KeygenCommand, MusigCommand, TaprootTweak,
    },
};
use keymeld_sdk::{
    confidential_session::ConfidentialSession, AuthorizationCredentials, KeyMeldClient,
    SessionCredentials,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tempfile::TempDir;

/// A writer for a state the test wrote itself. It knows of no stored parts or entry encodings,
/// as after loading a format-1 row.
async fn make_checkpoint(
    db: DBConnection,
    key: SessionSecret,
    session: SessionId,
    version: i64,
    state: &ProtocolState,
    format: CheckpointFormat,
) -> DurableCheckpoint {
    make_writer(db, key, session, version, state, format.into()).await
}

/// As [`make_checkpoint`], writing in `layout`.
async fn make_writer(
    db: DBConnection,
    key: SessionSecret,
    session: SessionId,
    version: i64,
    state: &ProtocolState,
    layout: Layout,
) -> DurableCheckpoint {
    let written = Loaded {
        version,
        state: state.clone(),
        parts: BTreeSet::new(),
        entries: EntryCache::default(),
    };
    DurableCheckpoint::resume_in(db, key, session, written, layout)
        .await
        .unwrap()
        .0
}

/// The writer a new operation resumes from what the store holds, writing in `layout`.
async fn resume_layout(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
    layout: Layout,
) -> (DurableCheckpoint, ProtocolState) {
    let loaded = load(db, key, session).await.unwrap().unwrap();
    DurableCheckpoint::resume_in(db.clone(), key.clone(), session.clone(), loaded, layout)
        .await
        .unwrap()
}

/// A partitioned writer resumed from what the store holds, as a new operation starts one.
async fn resume_parts(
    db: &DBConnection,
    key: &SessionSecret,
    session: &SessionId,
) -> (DurableCheckpoint, ProtocolState) {
    let loaded = load(db, key, session).await.unwrap().unwrap();
    DurableCheckpoint::resume(
        db.clone(),
        key.clone(),
        session.clone(),
        loaded,
        CheckpointFormat::Parts,
    )
    .await
    .unwrap()
}

/// A partitioned write in a test build reads back what it committed and compares it with what
/// it was given. Every flow that writes format 2, including those the SDK drives through the
/// Coordinator service, then checks each journal it saves: the entry the SDK just changed in
/// place, and the others whose encodings the writer reused.
pub(super) async fn assert_stored_journal(
    checkpoint: &DurableCheckpoint,
    version: i64,
    journal: &ConfidentialJournal,
) {
    let stored = stored(checkpoint, version).await;
    assert!(
        serde_json::to_value(&stored.journal).unwrap() == serde_json::to_value(journal).unwrap(),
        "the stored journal differs from the one in memory"
    );
}

/// Every write in a test build also checks that the table holds exactly the parts and nodes
/// the writer counts as stored, and none after a format-1 write. A writer deletes what its own
/// previous write stored without reading the table, which this proves it may.
pub(super) async fn assert_stored_parts(checkpoint: &DurableCheckpoint, snapshot: &Snapshot) {
    let stored: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT digest FROM keymeld_protocol_parts WHERE session_id = ? ORDER BY digest",
    )
    .bind(checkpoint.session.to_string())
    .fetch_all(checkpoint.db.read())
    .await
    .unwrap();
    let expected: Vec<Vec<u8>> = match snapshot {
        Snapshot::Legacy(_) => Vec::new(),
        Snapshot::Parts { committed, .. } => {
            committed.iter().map(|digest| digest.to_vec()).collect()
        }
    };
    assert!(
        stored == expected,
        "the store holds {} parts and nodes; the writer counts {}",
        stored.len(),
        expected.len()
    );
}

/// The format a row is stored in.
async fn row_format(db: &DBConnection, session: &SessionId) -> i64 {
    sqlx::query_scalar("SELECT format FROM keymeld_protocol_state WHERE session_id = ?")
        .bind(session.to_string())
        .fetch_one(db.read())
        .await
        .unwrap()
}

/// As [`assert_stored_journal`], for a write of the whole state.
pub(super) async fn assert_stored_state(
    checkpoint: &DurableCheckpoint,
    version: i64,
    state: &ProtocolState,
) {
    let stored = stored(checkpoint, version).await;
    assert!(
        serde_json::to_value(&stored).unwrap() == serde_json::to_value(state).unwrap(),
        "the stored state differs from the one in memory"
    );
}

/// What the store holds, decoded without the identity checks `load` applies: some tests write a
/// state for another session or schema on purpose to prove that reads refuse it.
async fn stored(checkpoint: &DurableCheckpoint, version: i64) -> ProtocolState {
    let (stored_version, state, _) =
        load_record::<ProtocolState>(&checkpoint.db, &checkpoint.key, &checkpoint.session, None)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        stored_version, version,
        "another writer committed in between"
    );
    state
}

async fn database() -> (TempDir, DBConnection) {
    let directory = tempfile::tempdir().unwrap();
    let db = DBConnection::new(
        directory.path().to_str().unwrap(),
        "checkpoint",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap();
    (directory, db)
}
fn state(session: &SessionId) -> ProtocolState {
    let authority = AuthorizationCredentials::from_secret(&[11; 32]).unwrap();
    let invitation = AuthorizationCredentials::from_secret(&[12; 32]).unwrap();
    let credentials = SessionCredentials::from_session_secret(&[13; 32]).unwrap();
    let user = UserId::new_v7();
    let enclave = EnclaveId::new(1);
    let manifest = SignedSessionManifest::sign(
        SessionAuthorizationManifest {
            keygen_session_id: session.clone(),
            coordinator_user_id: user.clone(),
            creator_pubkey: authority.public_key_bytes(),
            signing_pubkey: authority.public_key_bytes(),
            session_public_key: credentials.public_key_bytes(),
            participant_verifiers: BTreeMap::from([(user.clone(), invitation.public_key_bytes())]),
            timeout_secs: 300,
            max_signing_sessions: Some(8),
            encrypted_taproot_tweak: credentials
                .encrypt(
                    &serde_json::to_vec(&TaprootTweak::None).unwrap(),
                    "taproot_tweak",
                )
                .unwrap(),
            subset_definitions: vec![],
            deposit_scope: None,
        },
        &authority.export_secret(),
    )
    .unwrap();
    let recipients = EnclaveRecipientAuthorization::sign(
        &manifest,
        BTreeMap::from([(user, enclave)]),
        BTreeMap::from([(
            enclave,
            AuthorizationCredentials::from_secret(&[15; 32])
                .unwrap()
                .public_key_bytes(),
        )]),
        &authority.export_secret(),
    )
    .unwrap();
    ProtocolState {
        schema_version: 1,
        session: StoredDlcKeygenSession {
            session_id: session.to_string(),
            encrypted_session_secret: "encrypted credential fixture".into(),
            authorization_manifest: manifest,
            recipient_authorization: recipients,
            encrypted_signing_authority: "encrypted authority fixture".into(),
            encrypted_registration_authorities: BTreeMap::new(),
            aggregate_key: vec![],
            outcome_subset_ids: BTreeMap::new(),
        },
        epochs: BTreeMap::from([(enclave, 1)]),
        registrations: BTreeMap::new(),
        policies: BTreeMap::new(),
        journal: ConfidentialJournal::default(),
        roster: None,
        bindings: BTreeMap::new(),
        signing: None,
        settlements: BTreeMap::new(),
    }
}
async fn row(db: &DBConnection, session: &SessionId) -> (i64, String) {
    let row = sqlx::query(
        "SELECT version,encrypted_state FROM keymeld_protocol_state WHERE session_id=?",
    )
    .bind(session.to_string())
    .fetch_one(db.read())
    .await
    .unwrap();
    (row.get("version"), row.get("encrypted_state"))
}
async fn replace(db: &DBConnection, session: &SessionId, version: i64, ciphertext: String) {
    let id = session.to_string();
    db.execute_write(move |pool| async move {
        sqlx::query(
            "UPDATE keymeld_protocol_state SET version=?,encrypted_state=? WHERE session_id=?",
        )
        .bind(version)
        .bind(ciphertext)
        .bind(id)
        .execute(&pool)
        .await
        .map(|_| ())
    })
    .await
    .unwrap();
}

/// Every format a writer can write.
const FORMATS: [CheckpointFormat; 3] = [
    CheckpointFormat::Monolithic,
    CheckpointFormat::Parts,
    CheckpointFormat::Tree,
];

#[tokio::test]
async fn checkpoint_aead_roundtrips_large_private_state_without_plaintext_in_database() {
    let (_directory, db) = database().await;
    assert_checkpoint_roundtrip(&db).await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn checkpoint_rejects_wrong_key_session_version_schema_and_ciphertext() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([24; 32]);
    let mut state = state(&session);
    create(&db, &key, &session, &state).await.unwrap();
    let (_, original) = row(&db, &session).await;
    assert!(load(&db, &SessionSecret::from_bytes([25; 32]), &session)
        .await
        .is_err());
    replace(&db, &session, 1, original.clone()).await;
    assert!(load(&db, &key, &session).await.is_err());
    let mut tampered = EncryptedData::from_hex(&original).unwrap();
    tampered.ciphertext[0] ^= 1;
    replace(&db, &session, 0, tampered.to_hex().unwrap()).await;
    assert!(load(&db, &key, &session).await.is_err());
    replace(&db, &session, 0, original.clone()).await;
    let foreign = SessionId::new_v7();
    let source = session.to_string();
    let target = foreign.to_string();
    db.execute_write(move |pool| async move {
        sqlx::query("UPDATE keymeld_protocol_state SET session_id=? WHERE session_id=?")
            .bind(target)
            .bind(source)
            .execute(&pool)
            .await
            .map(|_| ())
    })
    .await
    .unwrap();
    assert!(load(&db, &key, &foreign).await.is_err());
    // Valid AEAD with an invalid inner session/schema still fails validation.
    replace(&db, &foreign, 0, seal(&key, &foreign, 0, &state).unwrap()).await;
    assert!(load(&db, &key, &foreign).await.is_err());
    state.session.session_id = foreign.to_string();
    state.schema_version = 2;
    replace(&db, &foreign, 0, seal(&key, &foreign, 0, &state).unwrap()).await;
    assert!(load(&db, &key, &foreign).await.is_err());
    db.close().await.unwrap();
}

#[tokio::test]
async fn independently_loaded_checkpoints_use_compare_and_swap_without_lost_updates() {
    let (_directory, db) = database().await;
    for format in FORMATS {
        assert_checkpoint_compare_and_swap(&db, format).await;
    }
    db.close().await.unwrap();
}

async fn enclave_key() -> Json<serde_json::Value> {
    Json(
        serde_json::json!({"enclave_id":1,"public_key":hex::encode(AuthorizationCredentials::from_secret(&[15;32]).unwrap().public_key_bytes()),"attestation_document":"","pcr_measurements":{},"timestamp":0,"healthy":true,"key_epoch":1}),
    )
}
async fn should_not_send(State(count): State<Arc<AtomicUsize>>) -> StatusCode {
    count.fetch_add(1, Ordering::SeqCst);
    StatusCode::INTERNAL_SERVER_ERROR
}

#[tokio::test]
async fn sdk_sends_no_enclave_command_after_durable_checkpoint_cas_failure() {
    let (_directory, db) = database().await;
    for format in FORMATS {
        let server = assert_stale_checkpoint_stops_command(&db, format).await;
        server.abort();
    }
    db.close().await.unwrap();
}

async fn assert_checkpoint_roundtrip(db: &DBConnection) {
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([23; 32]);
    let mut state = state(&session);
    state.session.encrypted_session_secret = "private journal marker ".repeat(8192);
    assert!(serde_json::to_vec(&state).unwrap().len() > 64 * 1024);
    assert!(create(db, &key, &session, &state).await.unwrap());
    assert!(!create(db, &key, &session, &state).await.unwrap());
    let (version, ciphertext) = row(db, &session).await;
    assert_eq!(version, 0);
    assert!(!ciphertext.contains("private journal marker"));
    let loaded = load(db, &key, &session).await.unwrap().unwrap().state;
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::to_value(&state).unwrap()
    );
    let checkpoint = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        version,
        &loaded,
        CheckpointFormat::Monolithic,
    )
    .await;
    checkpoint
        .save(&ConfidentialJournal::default())
        .await
        .unwrap();
    assert_eq!(row(db, &session).await.0, 1);
    assert_eq!(
        load(db, &key, &session)
            .await
            .unwrap()
            .unwrap()
            .state
            .session
            .encrypted_session_secret,
        state.session.encrypted_session_secret
    );
}

async fn assert_checkpoint_compare_and_swap(db: &DBConnection, format: CheckpointFormat) {
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([26; 32]);
    let original = state(&session);
    create(db, &key, &session, &original).await.unwrap();
    let first = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        format,
    )
    .await;
    let second = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        format,
    )
    .await;
    let mut left = original.clone();
    left.session.encrypted_session_secret = "first committed state".into();
    let mut right = original;
    right.session.encrypted_session_secret = "second committed state".into();
    let (a, b) = tokio::join!(first.finish(left), second.finish(right));
    assert_ne!(a.is_ok(), b.is_ok());
    let Loaded {
        version,
        state: loaded,
        ..
    } = load(db, &key, &session).await.unwrap().unwrap();
    assert_eq!(version, 1);
    assert_eq!(
        loaded.session.encrypted_session_secret,
        if a.is_ok() {
            "first committed state"
        } else {
            "second committed state"
        }
    );
    let loser = if a.is_err() { &first } else { &second };
    assert_eq!(
        loser.current.lock().await.0,
        0,
        "failed CAS must leave in-memory checkpoint uncommitted"
    );
    assert!(loser.save(&ConfidentialJournal::default()).await.is_err());
    assert_eq!(row(db, &session).await.0, 1);
}

async fn assert_stale_checkpoint_stops_command(
    db: &DBConnection,
    format: CheckpointFormat,
) -> tokio::task::JoinHandle<()> {
    let session_id = SessionId::new_v7();
    let key = SessionSecret::from_bytes([27; 32]);
    let state = state(&session_id);
    create(db, &key, &session_id, &state).await.unwrap();
    let stale = make_checkpoint(
        db.clone(),
        key.clone(),
        session_id.clone(),
        0,
        &state,
        format,
    )
    .await;
    let winner = make_checkpoint(
        db.clone(),
        key.clone(),
        session_id.clone(),
        0,
        &state,
        format,
    )
    .await;
    winner.save(&ConfidentialJournal::default()).await.unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/api/v1/enclaves/1/public-key", get(enclave_key))
        .route("/api/v1/confidential", post(should_not_send))
        .with_state(count.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let authority = AuthorizationCredentials::from_secret(&[11; 32]).unwrap();
    let reply = AuthorizationCredentials::from_secret(&[14; 32]).unwrap();
    let credentials = SessionCredentials::from_session_secret(&[13; 32]).unwrap();
    let client = KeyMeldClient::builder(
        &format!("http://{address}"),
        state
            .session
            .authorization_manifest
            .manifest
            .coordinator_user_id
            .clone(),
    )
    .dangerous_trust_unattested_enclaves()
    .build()
    .unwrap();
    let mut journal = ConfidentialJournal::default();
    let mut session = ConfidentialSession::connect(
        &client,
        &state.session.authorization_manifest,
        &state.session.recipient_authorization,
        &state.epochs,
        &credentials,
        &authority,
        &reply,
        &mut journal,
        &stale,
    )
    .await
    .unwrap();
    let result = session
        .command_once(
            "must-persist-before-send",
            EnclaveId::new(1),
            &session_id,
            || {
                Ok(EnclaveCommand::Musig(MusigCommand::Keygen(
                    KeygenCommand::GetAggregatePublicKey(GetAggregatePublicKeyCommand {
                        keygen_session_id: session_id.clone(),
                    }),
                )))
            },
        )
        .await;
    assert!(result.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(row(db, &session_id).await.0, 1);
    server
}

#[tokio::test]
async fn partitioned_checkpoints_reuse_ciphertext_and_fail_closed_on_missing_parts() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([41; 32]);
    let mut original = state(&session);
    original.session.encrypted_session_secret = "large private checkpoint ".repeat(8192);
    original.session.aggregate_key = (0..=255).cycle().take(64 * 1024).collect();
    create(&db, &key, &session, &original).await.unwrap();
    let checkpoint = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        CheckpointFormat::Parts,
    )
    .await;
    checkpoint.finish(original.clone()).await.unwrap();
    let read_parts = || async {
        sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
            "SELECT digest, body FROM keymeld_protocol_parts WHERE session_id = ? ORDER BY digest",
        )
        .bind(session.to_string())
        .fetch_all(db.read())
        .await
        .unwrap()
    };
    let first = read_parts().await;
    assert!(!first.is_empty());
    assert!(
        first.len() < 100,
        "binary vectors must not create a part per byte"
    );
    let encoded_bytes: usize = first.iter().map(|(_, body)| body.len()).sum();
    assert!(encoded_bytes < serde_json::to_vec(&original).unwrap().len() / 4);
    checkpoint.finish(original.clone()).await.unwrap();
    assert_eq!(
        read_parts().await,
        first,
        "unchanged ciphertext is never rewritten"
    );
    let Loaded {
        version,
        state: restored,
        ..
    } = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(version, 2);
    assert_eq!(
        serde_json::to_value(&restored).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    let stale = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        1,
        &original,
        CheckpointFormat::Parts,
    )
    .await;
    assert!(stale.finish(original.clone()).await.is_err());
    assert_eq!(
        read_parts().await,
        first,
        "failed CAS must leave parts unchanged"
    );
    let mut changed = original.clone();
    changed.session.encrypted_session_secret = "another private checkpoint ".repeat(8192);
    checkpoint.finish(changed.clone()).await.unwrap();
    let after = read_parts().await;
    assert!(
        after.iter().any(|part| first.contains(part)),
        "unchanged protocol fields keep their ciphertext"
    );
    let wanted = Parts::encode(&key, &session, &changed)
        .unwrap()
        .bodies
        .len();
    assert_eq!(
        after.len(),
        wanted,
        "only parts outside the current manifest are pruned"
    );
    assert!(load(&db, &SessionSecret::from_bytes([42; 32]), &session)
        .await
        .is_err());
    let id = session.to_string();
    let digest = after[0].0.clone();
    db.execute_write(move |pool| async move {
        sqlx::query("DELETE FROM keymeld_protocol_parts WHERE session_id=? AND digest=?")
            .bind(id)
            .bind(digest)
            .execute(&pool)
            .await
            .map(|_| ())
    })
    .await
    .unwrap();
    assert!(
        load(&db, &key, &session).await.is_err(),
        "a missing part never becomes empty protocol state"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn compatible_reader_can_write_a_partitioned_checkpoint_back_to_legacy_format() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([43; 32]);
    let original = state(&session);
    create(&db, &key, &session, &original).await.unwrap();
    let writer = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        CheckpointFormat::Parts,
    )
    .await;
    writer.finish(original.clone()).await.unwrap();
    let Loaded {
        version,
        state: restored,
        ..
    } = load(&db, &key, &session).await.unwrap().unwrap();
    let legacy = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        version,
        &restored,
        CheckpointFormat::Monolithic,
    )
    .await;
    legacy.finish(original.clone()).await.unwrap();
    let (version, ciphertext) = row(&db, &session).await;
    let plaintext = key
        .decrypt(
            &EncryptedData::from_hex(&ciphertext).unwrap(),
            &context(&session, version),
        )
        .unwrap();
    let restored: ProtocolState = serde_json::from_slice(&plaintext).unwrap();
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM keymeld_protocol_parts WHERE session_id=?")
            .bind(session.to_string())
            .fetch_one(db.read())
            .await
            .unwrap();
    assert_eq!(count, 0);
    db.close().await.unwrap();
}

/// Synthetic opaque command bodies: no fixture is sent to an enclave.
fn large_journal(entries: usize, payload_bytes: usize) -> ConfidentialJournal {
    use keymeld_core::{
        confidential::{ConfidentialRequest, EnclaveEnvelope},
        protocol::Command,
    };
    let mut commands = serde_json::Map::new();
    for index in 0..entries {
        let envelope = EnclaveEnvelope {
            transport_version: 1,
            destination_enclave: EnclaveId::new(1),
            opaque_route_id: Uuid::now_v7(),
            correlation_id: "ab".repeat(32),
            ciphertext: format!("{index:08x}{}", "ab".repeat(payload_bytes / 2)),
        };
        let request = ConfidentialRequest {
            header: envelope.header(),
            enclave_public_key: vec![2; 33],
            enclave_key_epoch: 1,
            authority_public_key: vec![3; 33],
            response_public_key: vec![4; 33],
            command: Command::new(EnclaveCommand::Musig(MusigCommand::Keygen(
                KeygenCommand::GetAggregatePublicKey(GetAggregatePublicKeyCommand {
                    keygen_session_id: SessionId::new_v7(),
                }),
            ))),
            request_nonce: [0; 32],
            signature: vec![0; 64],
        };
        commands.insert(
            format!("fixture/{index}/1"),
            serde_json::json!({
                "input_commitment": vec![index as u8; 32],
                "request": {"envelope": envelope, "request": request}, "outcome": null,
            }),
        );
    }
    let mut value = serde_json::to_value(ConfidentialJournal::default()).unwrap();
    value["commands"] = commands.into();
    // Over 1 KiB, so serializing the batch again shows in a save's serialized bytes.
    let message = format!("opaque\"batch\nvalue{}", "ef".repeat(1024));
    value["signing_batches"] = serde_json::json!({SessionId::new_v7().to_string(): {
        "input_commitment": vec![7; 32],
        "items": [{"batch_item_id": Uuid::now_v7(), "encrypted_message": message,
            "encrypted_adaptor_configs": null, "encrypted_taproot_tweak": "fixture", "subset_id": null}]
    }});
    serde_json::from_value(value).unwrap()
}

/// The longest JSON of one application field or journal entry. The partitioned writer
/// serializes each into its own buffer, so none needs more than about twice this.
fn largest_item_json(state: &ProtocolState) -> usize {
    let serde_json::Value::Object(mut fields) = serde_json::to_value(state).unwrap() else {
        panic!("protocol state serializes as an object");
    };
    let journal = fields.remove("journal").unwrap();
    let entries = ["commands", "signing_batches"]
        .into_iter()
        .flat_map(|collection| journal[collection].as_object().unwrap().values());
    fields
        .values()
        .chain(entries)
        .chain([
            &journal["opaque_route_id"],
            &journal["aborted_signing_sessions"],
        ])
        .map(|value| serde_json::to_vec(value).unwrap().len())
        .max()
        .unwrap()
}

#[test]
fn partitioned_encoder_bounds_buffers_and_reuses_unchanged_ciphertext() {
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([44; 32]);
    let mut original = state(&session);
    original.session.encrypted_session_secret = "private base field".repeat(64 * 1024);
    original.journal = large_journal(64, 64 * 1024);
    let mut base = Parts::base(&key, &session, &original, &BTreeSet::new()).unwrap();
    let first = Parts::journal(
        &key,
        &session,
        &base,
        &original.journal,
        &BTreeSet::new(),
        &EntryCache::default(),
    )
    .unwrap();
    let largest = largest_item_json(&original);
    assert!(
        2 * largest < first.plaintext_len,
        "the fixture must tell one field's buffer from the whole document"
    );
    assert!(
        first.max_buffer_capacity <= 2 * largest,
        "a full write must buffer one field or journal entry at a time, not the document"
    );
    let batches = serde_json::to_value(&original.journal).unwrap()["signing_batches"].to_string();
    assert!(
        batches.len() > 1024,
        "the signing batch alone exceeds the reuse bound below"
    );
    let mut known = BTreeSet::new();
    first.manifest.digests(&mut known);
    // The first write stores the base's new parts beside the journal's.
    let base_bodies = std::mem::take(&mut base.bodies);
    base.serialized_len = 0;
    base.max_buffer_capacity = 0;
    let unchanged = Parts::journal(
        &key,
        &session,
        &base,
        &original.journal,
        &known,
        &first.entries,
    )
    .unwrap();
    assert!(
        unchanged.bodies.is_empty(),
        "unchanged parts must not be recompressed or encrypted"
    );
    assert!(
        unchanged.serialized_len < 1024,
        "unchanged journal entries, signing batches and static fields must not be reserialized"
    );
    let bodies = base_bodies
        .into_iter()
        .chain(first.bodies)
        .map(|(key, body)| (key.to_vec(), body))
        .collect();
    let restored = unchanged.manifest.decode(&key, &session, &bodies).unwrap();
    assert_eq!(
        serde_json::to_value(&restored).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert_eq!(
        unchanged.plaintext_len,
        serde_json::to_vec(&restored).unwrap().len()
    );
}

#[test]
fn cloned_journal_reuses_entry_encodings_and_a_reloaded_one_reserializes_them() {
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([48; 32]);
    let mut original = state(&session);
    original.journal = large_journal(8, 16 * 1024);
    let mut base = Parts::base(&key, &session, &original, &BTreeSet::new()).unwrap();
    let first = Parts::journal(
        &key,
        &session,
        &base,
        &original.journal,
        &BTreeSet::new(),
        &EntryCache::default(),
    )
    .unwrap();
    let mut known = BTreeSet::new();
    first.manifest.digests(&mut known);
    // The first write stores the base's new parts beside the journal's.
    let base_bodies = std::mem::take(&mut base.bodies);
    base.serialized_len = 0;
    base.max_buffer_capacity = 0;
    let cloned = original.journal.clone();
    let reused = Parts::journal(&key, &session, &base, &cloned, &known, &first.entries).unwrap();
    assert!(reused.serialized_len < 1024);
    let mut json = serde_json::to_value(&original.journal).unwrap();
    json["commands"]["fixture/0/1"]["request"]["envelope"]["ciphertext"] =
        "changed opaque request".into();
    let replacement = serde_json::from_value(json.clone()).unwrap();
    let fresh =
        Parts::journal(&key, &session, &base, &replacement, &known, &first.entries).unwrap();
    assert!(fresh.serialized_len > 7 * 16 * 1024);
    assert_eq!(
        fresh.bodies.len(),
        1,
        "reserialized entries still reuse their committed parts; only the changed one is new"
    );
    let bodies = base_bodies
        .into_iter()
        .chain(first.bodies)
        .chain(fresh.bodies)
        .map(|(key, body)| (key.to_vec(), body))
        .collect();
    let restored = fresh.manifest.decode(&key, &session, &bodies).unwrap();
    assert_eq!(serde_json::to_value(restored.journal).unwrap(), json);
}

#[tokio::test]
async fn failed_part_insert_leaves_version_and_committed_parts_for_the_retry() {
    for format in [CheckpointFormat::Parts, CheckpointFormat::Tree] {
        failed_part_insert_leaves_the_writer_for_the_retry(format).await;
    }
}

async fn failed_part_insert_leaves_the_writer_for_the_retry(format: CheckpointFormat) {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([45; 32]);
    let mut original = state(&session);
    original.journal = large_journal(4, 64 * 1024);
    create(&db, &key, &session, &original).await.unwrap();
    let writer = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        format,
    )
    .await;
    writer.save(&original.journal).await.unwrap();
    let before = row(&db, &session).await;
    db.execute_write(|pool| async move {
        sqlx::query("CREATE TRIGGER fail_checkpoint_insert BEFORE INSERT ON keymeld_protocol_parts BEGIN SELECT RAISE(ABORT, 'injected checkpoint write failure'); END").execute(&pool).await.map(|_| ())
    }).await.unwrap();
    let changed = large_journal(5, 64 * 1024);
    assert!(writer.save(&changed).await.is_err());
    assert_eq!(writer.current.lock().await.0, 1);
    assert_eq!(
        row(&db, &session).await,
        before,
        "manifest update must roll back with failed part insertion"
    );
    let restored = load(&db, &key, &session).await.unwrap().unwrap().state;
    assert_eq!(
        serde_json::to_value(restored.journal).unwrap(),
        serde_json::to_value(&original.journal).unwrap()
    );
    db.execute_write(|pool| async move {
        sqlx::query("DROP TRIGGER fail_checkpoint_insert")
            .execute(&pool)
            .await
            .map(|_| ())
    })
    .await
    .unwrap();
    writer.save(&changed).await.unwrap();
    let Loaded {
        version,
        state: restored,
        ..
    } = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(version, 2);
    assert_eq!(
        serde_json::to_value(restored.journal).unwrap(),
        serde_json::to_value(changed).unwrap()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn journal_replacement_and_removal_survive_reload_and_legacy_rollback() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([46; 32]);
    let mut original = state(&session);
    original.journal = large_journal(3, 64 * 1024);
    create(&db, &key, &session, &original).await.unwrap();
    let writer = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        CheckpointFormat::Parts,
    )
    .await;
    writer.save(&original.journal).await.unwrap();
    let mut value = serde_json::to_value(&original.journal).unwrap();
    value["commands"]
        .as_object_mut()
        .unwrap()
        .remove("fixture/0/1");
    value["commands"]["fixture/1/1"]["request"]["envelope"]["ciphertext"] =
        "cd".repeat(8192).into();
    value["commands"]["fixture/1/1"]["outcome"] =
        serde_json::to_value(keymeld_core::protocol::Outcome {
            command_id: serde_json::from_value(
                value["commands"]["fixture/1/1"]["request"]["request"]["command"]["command_id"]
                    .clone(),
            )
            .unwrap(),
            created_at: std::time::UNIX_EPOCH,
            completed_at: std::time::UNIX_EPOCH,
            response: keymeld_core::protocol::EnclaveOutcome::Musig(
                keymeld_core::protocol::MusigOutcome::Keygen(
                    keymeld_core::protocol::KeygenOutcome::Success,
                ),
            ),
        })
        .unwrap();
    value["aborted_signing_sessions"] = serde_json::json!([SessionId::new_v7()]);
    let journal: ConfidentialJournal = serde_json::from_value(value.clone()).unwrap();
    writer.save(&journal).await.unwrap();
    drop(writer);
    let loaded = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(&loaded.state.journal).unwrap(), value);
    // A writer resumed after a process restart reuses the parts its load authenticated.
    let (restarted, mut restored) = DurableCheckpoint::resume(
        db.clone(),
        key.clone(),
        session.clone(),
        loaded,
        CheckpointFormat::Parts,
    )
    .await
    .unwrap();
    restarted.save(&restored.journal).await.unwrap();
    restored.session.encrypted_session_secret = "updated static state".into();
    let restored = restarted.finish(restored).await.unwrap();
    restarted.save(&restored.journal).await.unwrap();
    let Loaded {
        version,
        state: restored,
        ..
    } = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(
        restored.session.encrypted_session_secret,
        "updated static state"
    );
    assert_eq!(serde_json::to_value(&restored.journal).unwrap(), value);
    let legacy = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        version,
        &restored,
        CheckpointFormat::Monolithic,
    )
    .await;
    legacy.finish(restored.clone()).await.unwrap();
    let rolled_back = load(&db, &key, &session).await.unwrap().unwrap().state;
    assert_eq!(
        serde_json::to_value(rolled_back).unwrap(),
        serde_json::to_value(restored).unwrap()
    );
    db.close().await.unwrap();
}

async fn part_rows(db: &DBConnection, session: &SessionId) -> Vec<(Vec<u8>, Vec<u8>)> {
    sqlx::query_as(
        "SELECT digest, body FROM keymeld_protocol_parts WHERE session_id = ? ORDER BY digest",
    )
    .bind(session.to_string())
    .fetch_all(db.read())
    .await
    .unwrap()
}

async fn run(db: &DBConnection, statement: &'static str) {
    db.execute_write(
        move |pool| async move { sqlx::query(statement).execute(&pool).await.map(|_| ()) },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn resumed_writer_reuses_loaded_parts_and_fails_closed_after_a_conflicting_write() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([50; 32]);
    let mut original = state(&session);
    original.session.encrypted_session_secret = "resumed private field ".repeat(4096);
    original.journal = large_journal(4, 16 * 1024);
    create(&db, &key, &session, &original).await.unwrap();
    make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        CheckpointFormat::Parts,
    )
    .await
    .finish(original)
    .await
    .unwrap();
    let stored = part_rows(&db, &session).await;
    // Two operations resume from the same version.
    let (first, loaded) = resume_parts(&db, &key, &session).await;
    let (second, _) = resume_parts(&db, &key, &session).await;
    for writer in [&first, &second] {
        let current = writer.current.lock().await;
        assert_eq!(current.0, 1);
        assert!(
            matches!(&current.1, Snapshot::Parts { pending, .. } if pending.is_empty()),
            "a resumed writer encrypts none of the fields it loaded"
        );
    }

    // Saving the unchanged journal compresses, encrypts and inserts no part.
    run(&db, "CREATE TRIGGER refuse_part_insert BEFORE INSERT ON keymeld_protocol_parts BEGIN SELECT RAISE(ABORT, 'no part may be inserted'); END").await;
    first.save(&loaded.journal).await.unwrap();
    run(&db, "DROP TRIGGER refuse_part_insert").await;
    assert_eq!(row(&db, &session).await.0, 2);
    assert_eq!(part_rows(&db, &session).await, stored);

    // `first` removes an entry and with it the entry's part. `second` still counts that part as
    // stored, but its compare-and-swap names version 1, so it fails and changes nothing.
    let mut shorter = serde_json::to_value(&loaded.journal).unwrap();
    shorter["commands"]
        .as_object_mut()
        .unwrap()
        .remove("fixture/0/1");
    let removed: ConfidentialJournal = serde_json::from_value(shorter.clone()).unwrap();
    first.save(&removed).await.unwrap();
    let after_removal = part_rows(&db, &session).await;
    assert_eq!(after_removal.len(), stored.len() - 1);
    assert!(second.save(&loaded.journal).await.is_err());
    assert_eq!(second.current.lock().await.0, 1);
    assert_eq!(row(&db, &session).await.0, 3);
    assert_eq!(part_rows(&db, &session).await, after_removal);

    // A reload resumes from the version that won and stores the removed entry's part again.
    let (third, reloaded) = resume_parts(&db, &key, &session).await;
    assert_eq!(serde_json::to_value(&reloaded.journal).unwrap(), shorter);
    third.save(&loaded.journal).await.unwrap();
    let restored = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(restored.version, 4);
    assert_eq!(
        serde_json::to_value(&restored.state.journal).unwrap(),
        serde_json::to_value(&loaded.journal).unwrap()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn resumed_writer_reuses_the_journal_entry_encodings_it_loaded() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([51; 32]);
    let mut original = state(&session);
    original.journal = large_journal(8, 16 * 1024);
    create(&db, &key, &session, &original).await.unwrap();
    make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        CheckpointFormat::Parts,
    )
    .await
    .finish(original)
    .await
    .unwrap();
    let (writer, loaded) = resume_parts(&db, &key, &session).await;
    {
        // What the writer's next save encodes for the journal it loaded.
        let current = writer.current.lock().await;
        let Snapshot::Parts {
            base,
            committed,
            entries,
            ..
        } = &current.1
        else {
            panic!("a partitioned writer");
        };
        let unchanged =
            Parts::journal(&key, &session, base, &loaded.journal, committed, entries).unwrap();
        assert!(
            unchanged.serialized_len - base.serialized_len < 1024,
            "no loaded command or signing batch is serialized again"
        );
        assert!(unchanged.bodies.is_empty());
        assert_eq!(
            unchanged.plaintext_len,
            serde_json::to_vec(&loaded).unwrap().len(),
            "reused entries keep their exact size for the state limit"
        );
        // Equal contents decoded again are other entries: each one is serialized again.
        let decoded: ConfidentialJournal =
            serde_json::from_value(serde_json::to_value(&loaded.journal).unwrap()).unwrap();
        let fresh = Parts::journal(&key, &session, base, &decoded, committed, entries).unwrap();
        assert!(fresh.serialized_len - base.serialized_len > 8 * 16 * 1024);
    }
    run(&db, "CREATE TRIGGER refuse_part_insert BEFORE INSERT ON keymeld_protocol_parts BEGIN SELECT RAISE(ABORT, 'no part may be inserted'); END").await;
    writer.save(&loaded.journal).await.unwrap();
    run(&db, "DROP TRIGGER refuse_part_insert").await;
    let restored = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(restored.version, 2);
    assert_eq!(
        serde_json::to_vec(&restored.state).unwrap(),
        serde_json::to_vec(&loaded).unwrap()
    );
    db.close().await.unwrap();
}

/// Encoding moved to the blocking pool; the stored formats did not change. Format 1 seals the
/// state's exact JSON, format 2 the manifest an inline encode builds over the same parts, and
/// both load back as the state's exact JSON.
#[tokio::test]
async fn checkpoints_encoded_off_the_runtime_store_and_load_the_same_bytes() {
    let (_directory, db) = database().await;
    let key = SessionSecret::from_bytes([52; 32]);
    for format in FORMATS {
        let session = SessionId::new_v7();
        let mut original = state(&session);
        original.journal = large_journal(4, 16 * 1024);
        let json = serde_json::to_vec(&original).unwrap();
        create(&db, &key, &session, &original).await.unwrap();
        make_checkpoint(
            db.clone(),
            key.clone(),
            session.clone(),
            0,
            &original,
            format,
        )
        .await
        .finish(original.clone())
        .await
        .unwrap();
        let (version, ciphertext) = row(&db, &session).await;
        let sealed = key
            .decrypt(
                &EncryptedData::from_hex(&ciphertext).unwrap(),
                &context(&session, version),
            )
            .unwrap();
        let inline = Parts::encode(&key, &session, &original).unwrap();
        let (expected, digests): (_, BTreeSet<_>) = match format {
            CheckpointFormat::Monolithic => (json.clone(), BTreeSet::new()),
            CheckpointFormat::Parts => (
                serde_json::to_vec(&inline.manifest).unwrap(),
                inline.bodies.keys().map(|digest| digest.to_vec()).collect(),
            ),
            CheckpointFormat::Tree => {
                let packed = protocol_parts::pack(
                    &key,
                    &session,
                    &inline.manifest,
                    &BTreeSet::new(),
                    Shape::writer(),
                )
                .unwrap();
                (
                    serde_json::to_vec(&packed.root).unwrap(),
                    inline
                        .bodies
                        .keys()
                        .chain(&packed.nodes)
                        .map(|digest| digest.to_vec())
                        .collect(),
                )
            }
        };
        assert_eq!(sealed, expected);
        assert_eq!(
            row_format(&db, &session).await,
            match format {
                CheckpointFormat::Monolithic => 1,
                CheckpointFormat::Parts => 2,
                CheckpointFormat::Tree => 3,
            }
        );
        let stored: BTreeSet<_> = part_rows(&db, &session)
            .await
            .into_iter()
            .map(|(digest, _)| digest)
            .collect();
        assert_eq!(stored, digests);
        let loaded = load(&db, &key, &session).await.unwrap().unwrap();
        assert_eq!(serde_json::to_vec(&loaded.state).unwrap(), json);
    }
    db.close().await.unwrap();
}

/// A save cancelled while its encoding waits for the blocking pool keeps the writer locked
/// until that encoding ends, then leaves the version, the cache and the database as they were,
/// so the retry commits.
#[test]
fn cancelled_save_leaves_the_writer_and_store_unchanged_for_the_retry() {
    for format in [CheckpointFormat::Parts, CheckpointFormat::Tree] {
        cancelled_save_leaves_the_writer_for_the_retry(format);
    }
}

fn cancelled_save_leaves_the_writer_for_the_retry(format: CheckpointFormat) {
    // One blocking thread, so the test decides when the save's encoding can run.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (_directory, db) = database().await;
        let session = SessionId::new_v7();
        let key = SessionSecret::from_bytes([53; 32]);
        let mut original = state(&session);
        original.session.encrypted_session_secret = "pending base field ".repeat(4096);
        create(&db, &key, &session, &original).await.unwrap();
        let writer = make_checkpoint(
            db.clone(),
            key.clone(),
            session.clone(),
            0,
            &original,
            format,
        )
        .await;
        let before = row(&db, &session).await;
        let pending = match &writer.current.lock().await.1 {
            Snapshot::Parts { pending, .. } => Arc::clone(pending),
            Snapshot::Legacy(_) => panic!("a partitioned writer"),
        };
        assert!(!pending.is_empty(), "the base waits for its first write");

        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || blocked.recv());
        let journal = large_journal(2, 16 * 1024);
        assert!(writer.save(&journal).now_or_never().is_none());
        assert!(
            writer.current.try_lock().is_err(),
            "the cancelled save's encoding still holds the writer"
        );
        release.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        {
            let current = writer.current.lock().await;
            assert_eq!(current.0, 0);
            let Snapshot::Parts {
                pending: kept,
                committed,
                ..
            } = &current.1
            else {
                panic!("a partitioned writer");
            };
            assert!(Arc::ptr_eq(kept, &pending), "the base still waits");
            assert!(committed.is_empty());
        }
        assert_eq!(row(&db, &session).await, before);

        writer.save(&journal).await.unwrap();
        assert_eq!(row(&db, &session).await.0, 1);
        db.close().await.unwrap();
    });
}

#[test]
fn cached_fields_count_toward_the_reconstructed_checkpoint_limit() {
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([47; 32]);
    let original = state(&session);
    let mut base = Parts::base(&key, &session, &original, &BTreeSet::new()).unwrap();
    // Represent an already-sized cached base without allocating half a GiB.
    base.plaintext_len = 512 * 1024 * 1024;
    base.serialized_len = 0;
    assert!(Parts::journal(
        &key,
        &session,
        &base,
        &original.journal,
        &BTreeSet::new(),
        &EntryCache::default()
    )
    .is_err());
}

#[tokio::test]
async fn metadata_reads_match_legacy_and_parts_without_loading_the_journal() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([61; 32]);
    let mut original = state(&session);
    original.journal = large_journal(8, 128 * 1024);
    original.epochs.insert(EnclaveId::new(2), 7);
    create(&db, &key, &session, &original).await.unwrap();
    for format in FORMATS {
        let version = row(&db, &session).await.0;
        let writer = make_checkpoint(
            db.clone(),
            key.clone(),
            session.clone(),
            version,
            &original,
            format,
        )
        .await;
        writer.finish(original.clone()).await.unwrap();
        let (read_version, metadata) = load_metadata(&db, &key, &session).await.unwrap().unwrap();
        assert_eq!(read_version, version + 1);
        assert_eq!(metadata.epochs, original.epochs);
        assert_eq!(
            serde_json::to_value(metadata.session).unwrap(),
            serde_json::to_value(&original.session).unwrap()
        );
        assert_eq!(metadata.registrations.len(), original.registrations.len());
        assert!(metadata.roster.is_none());
        assert!(
            load_metadata(&db, &SessionSecret::from_bytes([62; 32]), &session)
                .await
                .is_err()
        );
    }
    let full = Parts::encode(&key, &session, &original).unwrap().manifest;
    let selected = full.select_fields(&[
        "schema_version",
        "session",
        "epochs",
        "registrations",
        "roster",
    ]);
    let mut all = BTreeSet::new();
    full.digests(&mut all);
    let mut needed = BTreeSet::new();
    selected.digests(&mut needed);
    let excluded = *all.difference(&needed).next().expect("journal-only part");
    let id = session.to_string();
    db.execute_write(move |pool| async move {
        sqlx::query("DELETE FROM keymeld_protocol_parts WHERE session_id=? AND digest=?")
            .bind(id)
            .bind(excluded.to_vec())
            .execute(&pool)
            .await
            .map(|_| ())
    })
    .await
    .unwrap();
    // This read certifies metadata only. Full recovery must still authenticate
    // every journal part and reject the now-incomplete checkpoint.
    assert!(load_metadata(&db, &key, &session).await.unwrap().is_some());
    assert!(load(&db, &key, &session).await.is_err());
    let required = *needed.first().unwrap();
    let id = session.to_string();
    db.execute_write(move |pool| async move {
        sqlx::query("UPDATE keymeld_protocol_parts SET body=x'00' WHERE session_id=? AND digest=?")
            .bind(id)
            .bind(required.to_vec())
            .execute(&pool)
            .await
            .map(|_| ())
    })
    .await
    .unwrap();
    assert!(load_metadata(&db, &key, &session).await.is_err());
    db.close().await.unwrap();
}

#[tokio::test]
async fn metadata_reads_reject_wrong_identity_and_schema() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([63; 32]);
    let mut original = state(&session);
    create(&db, &key, &session, &original).await.unwrap();
    for format in FORMATS {
        for bad_schema in [false, true] {
            original.schema_version = if bad_schema { 2 } else { 1 };
            original.session.session_id = if bad_schema {
                session.to_string()
            } else {
                SessionId::new_v7().to_string()
            };
            let version = row(&db, &session).await.0;
            let writer = make_checkpoint(
                db.clone(),
                key.clone(),
                session.clone(),
                version,
                &original,
                format,
            )
            .await;
            writer.finish(original.clone()).await.unwrap();
            assert!(load_metadata(&db, &key, &session).await.is_err());
        }
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn metadata_projection_uses_one_committed_snapshot() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([64; 32]);
    let mut original = state(&session);
    original.epochs.insert(EnclaveId::new(1), 0);
    original.session.encrypted_session_secret = "0".into();
    create(&db, &key, &session, &original).await.unwrap();
    let writer = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        CheckpointFormat::Parts,
    )
    .await;
    let writes = async {
        for version in 1..=16 {
            original.epochs.insert(EnclaveId::new(1), version);
            original.session.encrypted_session_secret = version.to_string();
            writer.finish(original.clone()).await.unwrap();
            tokio::task::yield_now().await;
        }
    };
    let reads = async {
        for _ in 0..32 {
            let (version, view) = load_metadata(&db, &key, &session).await.unwrap().unwrap();
            assert_eq!(view.epochs[&EnclaveId::new(1)], version as u64);
            assert_eq!(view.session.encrypted_session_secret, version.to_string());
            tokio::task::yield_now().await;
        }
    };
    tokio::join!(writes, reads);
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "isolated checkpoint read comparison; CHECKPOINT_BENCH_FULL=1 selects full reads"]
async fn checkpoint_read_memory_benchmark() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([65; 32]);
    let mut original = state(&session);
    original.journal = large_journal(32, 256 * 1024);
    let full_bytes = serde_json::to_vec(&original).unwrap().len();
    create(&db, &key, &session, &original).await.unwrap();
    let checkpoint = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        CheckpointFormat::Parts,
    )
    .await;
    checkpoint.finish(original.clone()).await.unwrap();
    drop(checkpoint);
    drop(original);
    let full = std::env::var_os("CHECKPOINT_BENCH_FULL").is_some();
    let start = std::time::Instant::now();
    for _ in 0..40 {
        if full {
            std::hint::black_box(load(&db, &key, &session).await.unwrap().unwrap());
        } else {
            std::hint::black_box(load_metadata(&db, &key, &session).await.unwrap().unwrap());
        }
    }
    println!(
        "checkpoint_full_bytes={full_bytes} reads=40 full={full} elapsed_ms={:.3}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    #[cfg(target_os = "linux")]
    for line in std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
    {
        if ["VmRSS:", "VmHWM:", "VmSwap:"]
            .iter()
            .any(|field| line.starts_with(field))
        {
            println!("{line}");
        }
    }
    db.close().await.unwrap();
}

/// The digests the store holds for a session: its parts and, in format 3, its manifest nodes.
async fn stored_digests(db: &DBConnection, session: &SessionId) -> BTreeSet<Vec<u8>> {
    part_rows(db, session)
        .await
        .into_iter()
        .map(|(digest, _)| digest)
        .collect()
}

/// A successful outcome for the command a journal records under `name`.
fn outcome(journal: &serde_json::Value, name: &str) -> serde_json::Value {
    serde_json::to_value(keymeld_core::protocol::Outcome {
        command_id: serde_json::from_value(
            journal["commands"][name]["request"]["request"]["command"]["command_id"].clone(),
        )
        .unwrap(),
        created_at: std::time::UNIX_EPOCH,
        completed_at: std::time::UNIX_EPOCH,
        response: keymeld_core::protocol::EnclaveOutcome::Musig(
            keymeld_core::protocol::MusigOutcome::Keygen(
                keymeld_core::protocol::KeygenOutcome::Success,
            ),
        ),
    })
    .unwrap()
}

/// `journal` with a new command under `name`, as the SDK records one before sending it.
fn with_command(journal: &ConfidentialJournal, name: &str) -> serde_json::Value {
    let mut value = serde_json::to_value(journal).unwrap();
    let fresh = serde_json::to_value(large_journal(1, 512)).unwrap();
    value["commands"][name] = fresh["commands"]["fixture/0/1"].clone();
    value
}

#[tokio::test]
async fn checkpoints_convert_between_all_three_formats_through_reloads() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([91; 32]);
    let mut original = state(&session);
    original.session.encrypted_session_secret = "converted private field ".repeat(2048);
    original.journal = large_journal(24, 2 * 1024);
    create(&db, &key, &session, &original).await.unwrap();
    let expected = serde_json::to_vec(&original).unwrap();
    // Each operation resumes from what the store holds, as the service does, so every format
    // reads every other, and a shape change rewrites the nodes.
    for (layout, format) in [
        (Layout::Parts, 2),
        (Layout::Tree(Shape::writer()), 3),
        (Layout::Parts, 2),
        (Layout::Tree(Shape::WRITER), 3),
        (Layout::Tree(Shape::writer()), 3),
        (Layout::Monolithic, 1),
        (Layout::Tree(Shape::WRITER), 3),
    ] {
        let (writer, restored) = resume_layout(&db, &key, &session, layout).await;
        assert_eq!(serde_json::to_vec(&restored).unwrap(), expected);
        writer.save(&restored.journal).await.unwrap();
        let restored = writer.finish(restored).await.unwrap();
        writer.save(&restored.journal).await.unwrap();
        assert_eq!(row_format(&db, &session).await, format);
    }
    let restored = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(restored.version, 21);
    assert_eq!(serde_json::to_vec(&restored.state).unwrap(), expected);
    db.close().await.unwrap();
}

#[tokio::test]
async fn a_tree_write_stores_the_changed_path_and_deletes_only_what_it_replaced() {
    let (_directory, db) = database().await;
    let key = SessionSecret::from_bytes([92; 32]);
    let mut rows = Vec::new();
    for layout in [Layout::Parts, Layout::Tree(Shape::WRITER)] {
        let session = SessionId::new_v7();
        let mut original = state(&session);
        original.journal = large_journal(600, 256);
        create(&db, &key, &session, &original).await.unwrap();
        let writer = make_writer(
            db.clone(),
            key.clone(),
            session.clone(),
            0,
            &original,
            layout,
        )
        .await;
        writer.finish(original.clone()).await.unwrap();
        let before = stored_digests(&db, &session).await;

        // A new command lands between others in key order.
        let mut value = with_command(&original.journal, "fixture/5000/1");
        let journal: ConfidentialJournal = serde_json::from_value(value.clone()).unwrap();
        writer.save(&journal).await.unwrap();
        let added = stored_digests(&db, &session).await;
        // Its outcome changes that command.
        value["commands"]["fixture/5000/1"]["outcome"] = outcome(&value, "fixture/5000/1");
        let journal: ConfidentialJournal = serde_json::from_value(value).unwrap();
        writer.save(&journal).await.unwrap();
        let answered = stored_digests(&db, &session).await;
        let row_bytes = row(&db, &session).await.1.len();
        rows.push(row_bytes);

        if let Layout::Tree(_) = layout {
            for (from, to) in [(&before, &added), (&added, &answered)] {
                let inserted = to.difference(from).count();
                let deleted = from.difference(to).count();
                // The command's part, its chunk, and the nodes above the chunk.
                assert!((2..=6).contains(&inserted), "{inserted} inserted");
                assert!(deleted <= 5, "{deleted} deleted");
            }
            assert!(
                answered.difference(&added).count() >= 2,
                "the answered command and its chunk are new"
            );
            assert!(
                added.difference(&answered).count() >= 2,
                "the unanswered command and its old chunk are gone"
            );
            assert!(row_bytes < 8 * 1024, "the root takes {row_bytes} bytes");
        } else {
            assert_eq!(added.len(), before.len() + 1);
            assert_eq!(answered.len(), added.len());
        }
    }
    // Format 2 seals a manifest that names all 600 commands at every write.
    assert!(rows[0] > 20 * rows[1], "{rows:?}");
    db.close().await.unwrap();
}

#[tokio::test]
async fn tree_loads_fail_closed_on_a_missing_altered_or_misread_node() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([93; 32]);
    let mut original = state(&session);
    original.journal = large_journal(600, 256);
    create(&db, &key, &session, &original).await.unwrap();
    let writer = make_writer(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        Layout::Tree(Shape::WRITER),
    )
    .await;
    writer.finish(original.clone()).await.unwrap();
    assert!(load(&db, &key, &session).await.is_ok());

    // The nodes only the journal reaches: 600 commands make at least three chunks, since a chunk
    // holds at most 256.
    let manifest = Parts::encode(&key, &session, &original).unwrap().manifest;
    let fields = [
        "schema_version",
        "session",
        "epochs",
        "registrations",
        "roster",
    ];
    let pack = |manifest: &Manifest| {
        protocol_parts::pack(&key, &session, manifest, &BTreeSet::new(), Shape::WRITER)
            .unwrap()
            .nodes
    };
    let all = pack(&manifest);
    let metadata = pack(&manifest.select_fields(&fields));
    let journal_only: Vec<_> = all.difference(&metadata).copied().collect();
    assert!(journal_only.len() >= 2, "the journal is kept in nodes");
    let stored = stored_digests(&db, &session).await;
    assert!(all.iter().all(|node| stored.contains(node.as_slice())));

    let set_body = |digest: [u8; 32], body: Option<Vec<u8>>| {
        let (db, id) = (db.clone(), session.to_string());
        async move {
            db.execute_write(move |pool| async move {
                match body {
                    Some(body) => sqlx::query(
                        "INSERT INTO keymeld_protocol_parts(session_id, digest, body) VALUES (?, ?, ?) \
                         ON CONFLICT(session_id, digest) DO UPDATE SET body = excluded.body",
                    )
                    .bind(id)
                    .bind(digest.to_vec())
                    .bind(body)
                    .execute(&pool)
                    .await
                    .map(|_| ()),
                    None => sqlx::query(
                        "DELETE FROM keymeld_protocol_parts WHERE session_id = ? AND digest = ?",
                    )
                    .bind(id)
                    .bind(digest.to_vec())
                    .execute(&pool)
                    .await
                    .map(|_| ()),
                }
            })
            .await
            .unwrap()
        }
    };
    let bodies: BTreeMap<Vec<u8>, Vec<u8>> = part_rows(&db, &session).await.into_iter().collect();
    let (node, other) = (journal_only[0], journal_only[1]);
    let original_body = bodies[node.as_slice()].clone();

    // A missing node fails a full load. A metadata read never fetches it.
    set_body(node, None).await;
    assert!(load(&db, &key, &session).await.is_err());
    assert!(load_metadata(&db, &key, &session).await.unwrap().is_some());
    // An altered node, and another node's body under its address.
    let mut altered = original_body.clone();
    let last = altered.len() - 1;
    altered[last] ^= 1;
    set_body(node, Some(altered)).await;
    assert!(load(&db, &key, &session).await.is_err());
    set_body(node, Some(bodies[other.as_slice()].clone())).await;
    assert!(load(&db, &key, &session).await.is_err());
    set_body(node, Some(original_body)).await;
    assert!(load(&db, &key, &session).await.is_ok());

    // A root read as a format-2 manifest, or a format-2 reader's view of it, is refused.
    let set_format = |format: i64| {
        let (db, id) = (db.clone(), session.to_string());
        async move {
            db.execute_write(move |pool| async move {
                sqlx::query("UPDATE keymeld_protocol_state SET format = ? WHERE session_id = ?")
                    .bind(format)
                    .bind(id)
                    .execute(&pool)
                    .await
                    .map(|_| ())
            })
            .await
            .unwrap()
        }
    };
    for format in [1, 2, 4] {
        set_format(format).await;
        assert!(load(&db, &key, &session).await.is_err(), "format {format}");
        assert!(load_metadata(&db, &key, &session).await.is_err());
    }
    set_format(3).await;
    let loaded = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_vec(&loaded.state).unwrap(),
        serde_json::to_vec(&original).unwrap()
    );
    // A load counts the nodes it authenticated as stored, beside the parts.
    let mut expected: BTreeSet<[u8; 32]> = all;
    manifest.digests(&mut expected);
    assert_eq!(loaded.parts, expected);
    db.close().await.unwrap();
}

#[tokio::test]
async fn a_resumed_tree_writer_reuses_every_node_and_part_it_loaded() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([95; 32]);
    let mut original = state(&session);
    original.session.encrypted_session_secret = "resumed tree field ".repeat(2048);
    original.journal = large_journal(40, 1024);
    create(&db, &key, &session, &original).await.unwrap();
    let layout = Layout::Tree(Shape::writer());
    make_writer(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        layout,
    )
    .await
    .finish(original)
    .await
    .unwrap();
    let stored = part_rows(&db, &session).await;
    let (writer, loaded) = resume_layout(&db, &key, &session, layout).await;
    // Saving the unchanged journal or state seals and inserts no part and no node.
    run(&db, "CREATE TRIGGER refuse_part_insert BEFORE INSERT ON keymeld_protocol_parts BEGIN SELECT RAISE(ABORT, 'no part may be inserted'); END").await;
    writer.save(&loaded.journal).await.unwrap();
    let loaded = writer.finish(loaded).await.unwrap();
    writer.save(&loaded.journal).await.unwrap();
    run(&db, "DROP TRIGGER refuse_part_insert").await;
    assert_eq!(row(&db, &session).await.0, 4);
    assert_eq!(part_rows(&db, &session).await, stored);
    db.close().await.unwrap();
}

#[tokio::test]
async fn tree_writers_resumed_from_one_version_cannot_both_commit() {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([96; 32]);
    let mut original = state(&session);
    original.journal = large_journal(30, 1024);
    create(&db, &key, &session, &original).await.unwrap();
    let layout = Layout::Tree(Shape::writer());
    make_writer(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        layout,
    )
    .await
    .finish(original)
    .await
    .unwrap();
    let (first, loaded) = resume_layout(&db, &key, &session, layout).await;
    let (second, _) = resume_layout(&db, &key, &session, layout).await;
    first.save(&loaded.journal).await.unwrap();
    // `first` drops a command, and with it the command's part and the nodes above it, from
    // what its own previous write stored. `second` still counts them as stored, but its
    // swap names version 1, so it fails and changes nothing.
    let mut shorter = serde_json::to_value(&loaded.journal).unwrap();
    shorter["commands"]
        .as_object_mut()
        .unwrap()
        .remove("fixture/7/1");
    let removed: ConfidentialJournal = serde_json::from_value(shorter.clone()).unwrap();
    first.save(&removed).await.unwrap();
    let after_removal = part_rows(&db, &session).await;
    assert!(second.save(&loaded.journal).await.is_err());
    assert!(second.finish(loaded.clone()).await.is_err());
    assert_eq!(second.current.lock().await.0, 1);
    assert_eq!(row(&db, &session).await.0, 3);
    assert_eq!(part_rows(&db, &session).await, after_removal);
    // A reload resumes from the version that won and stores the command again.
    let (third, reloaded) = resume_layout(&db, &key, &session, layout).await;
    assert_eq!(serde_json::to_value(&reloaded.journal).unwrap(), shorter);
    third.save(&loaded.journal).await.unwrap();
    let restored = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(restored.version, 4);
    assert_eq!(
        serde_json::to_value(&restored.state.journal).unwrap(),
        serde_json::to_value(&loaded.journal).unwrap()
    );
    db.close().await.unwrap();
}

/// `PRAGMA wal_checkpoint` on the write connection: (busy, frames in the log, frames copied).
async fn wal_checkpoint(db: &DBConnection, mode: &'static str) -> (i64, i64, i64) {
    db.execute_write(move |pool| async move {
        let statement = format!("PRAGMA wal_checkpoint({mode})");
        sqlx::query_as::<_, (i64, i64, i64)>(sqlx::AssertSqlSafe(statement))
            .fetch_one(&pool)
            .await
    })
    .await
    .unwrap()
}

/// Empty the write-ahead log, so the next write's pages are all it holds.
async fn empty_log(db: &DBConnection) {
    let (busy, _, _) = wal_checkpoint(db, "TRUNCATE").await;
    assert_eq!(busy, 0, "no reader holds the log between writes");
}

/// The 4 KiB pages written to the log since [`empty_log`]: what a checkpoint copies into the
/// database and replication uploads.
async fn logged_pages(db: &DBConnection) -> i64 {
    wal_checkpoint(db, "PASSIVE").await.1
}

/// Log pages per write, and the sealed row's size, for one format and state size.
struct Measured {
    parts: i64,
    first: i64,
    saves: Vec<i64>,
    finishes: Vec<i64>,
    row_bytes: usize,
}

/// A state with `outcomes` outcome subsets, each its own part as in a contract with that many
/// outcomes, and 128 journal commands. Two rounds of what a refund does per entry: record a
/// command, record its outcome, and update the application state.
async fn measure_writes(layout: Layout, outcomes: usize) -> Measured {
    let (_directory, db) = database().await;
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([94; 32]);
    let mut original = state(&session);
    original.session.outcome_subset_ids =
        (0..outcomes).map(|index| (index, Uuid::now_v7())).collect();
    original.journal = large_journal(128, 512);
    create(&db, &key, &session, &original).await.unwrap();
    let writer = make_writer(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        layout,
    )
    .await;
    empty_log(&db).await;
    let mut state = writer.finish(original).await.unwrap();
    let first = logged_pages(&db).await;
    let (mut saves, mut finishes) = (Vec::new(), Vec::new());
    for round in 0..2 {
        let name = format!("refund/{round}/1");
        let mut value = with_command(&state.journal, &name);
        state.journal = serde_json::from_value(value.clone()).unwrap();
        empty_log(&db).await;
        writer.save(&state.journal).await.unwrap();
        saves.push(logged_pages(&db).await);

        value["commands"][&name]["outcome"] = outcome(&value, &name);
        state.journal = serde_json::from_value(value).unwrap();
        empty_log(&db).await;
        writer.save(&state.journal).await.unwrap();
        saves.push(logged_pages(&db).await);

        state
            .session
            .encrypted_registration_authorities
            .insert(UserId::new_v7(), format!("registration authority {round}"));
        empty_log(&db).await;
        state = writer.finish(state).await.unwrap();
        finishes.push(logged_pages(&db).await);
    }
    let parts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM keymeld_protocol_parts WHERE session_id = ?")
            .bind(session.to_string())
            .fetch_one(db.read())
            .await
            .unwrap();
    let row_bytes = row(&db, &session).await.1.len();
    db.close().await.unwrap();
    Measured {
        parts,
        first,
        saves,
        finishes,
        row_bytes,
    }
}

/// Measures what each write adds to the write-ahead log in formats 2 and 3, at two state sizes.
/// The table goes to stderr directly, so the test log shows it even when the test passes.
#[tokio::test]
async fn checkpoint_writes_grow_with_the_change_not_the_state() {
    use std::io::Write as _;
    let mut table = String::from(
        "| format | stored rows | first write | journal saves (each) | state updates (each) | sealed row bytes |\n| --- | --- | --- | --- | --- | --- |\n",
    );
    let mut results = BTreeMap::new();
    for outcomes in [1024, 4096] {
        for (name, layout) in [("2", Layout::Parts), ("3", Layout::Tree(Shape::WRITER))] {
            let measured = measure_writes(layout, outcomes).await;
            table += &format!(
                "| {name} | {} | {} | {:?} | {:?} | {} |\n",
                measured.parts,
                measured.first,
                measured.saves,
                measured.finishes,
                measured.row_bytes
            );
            results.insert((name, outcomes), measured);
        }
    }
    let _ = writeln!(
        std::io::stderr(),
        "\ncheckpoint write-ahead log pages per write (4 KiB each):\n{table}"
    );
    let max = |values: &[i64]| *values.iter().max().unwrap();
    let min = |values: &[i64]| *values.iter().min().unwrap();
    let (small_2, large_2) = (&results[&("2", 1024)], &results[&("2", 4096)]);
    let (small_3, large_3) = (&results[&("3", 1024)], &results[&("3", 4096)]);
    // Format 2 rewrites a manifest of every part at every write.
    assert!(min(&large_2.saves) >= 2 * min(&small_2.saves), "{table}");
    // Format 3 writes the same few pages at either size.
    assert!(max(&large_3.saves) <= max(&small_3.saves) + 6, "{table}");
    assert!(
        max(&large_3.finishes) <= max(&small_3.finishes) + 6,
        "{table}"
    );
    assert!(max(&large_3.saves) <= 32, "{table}");
    assert!(max(&large_3.finishes) <= 32, "{table}");
    assert!(min(&large_2.saves) >= 8 * max(&large_3.saves), "{table}");
    assert!(
        min(&large_2.finishes) >= 8 * max(&large_3.finishes),
        "{table}"
    );
}

/// A session a writer in `layout` stored as a kickoff leaves one: `outcomes` outcome subsets,
/// a journal of 32 commands, written once whole and once for a journal save.
async fn kickoff_session(
    db: &DBConnection,
    key: &SessionSecret,
    layout: Layout,
    outcomes: usize,
) -> SessionId {
    let session = SessionId::new_v7();
    let mut original = state(&session);
    original.session.outcome_subset_ids =
        (0..outcomes).map(|index| (index, Uuid::now_v7())).collect();
    original.journal = large_journal(32, 512);
    create(db, key, &session, &original).await.unwrap();
    let writer = make_writer(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        layout,
    )
    .await;
    let mut state = writer.finish(original).await.unwrap();
    state.journal = serde_json::from_value(with_command(&state.journal, "kickoff/sign/1")).unwrap();
    writer.save(&state.journal).await.unwrap();
    session
}

/// A refund invoice authorization stored for `session`, as a refund stores one per player.
async fn authorize_refund(db: &DBConnection, session: &SessionId, user: &str) {
    let (session, user) = (session.to_string(), user.to_owned());
    db.execute_write(move |pool| async move {
        sqlx::query(
            "INSERT INTO refund_invoice_authorizations
                 (keygen_session_id, user_id, invoice, authorization)
             VALUES (?, ?, 'lnbcrt1refund', 'authenticated receipt')",
        )
        .bind(session)
        .bind(user)
        .execute(&pool)
        .await
        .map(|_| ())
    })
    .await
    .unwrap();
}

async fn authorizations(db: &DBConnection, session: &SessionId) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM refund_invoice_authorizations WHERE keygen_session_id = ?",
    )
    .bind(session.to_string())
    .fetch_one(db.read())
    .await
    .unwrap()
}

/// A deletion removes the row, every part and manifest node, and every refund invoice
/// authorization of its session, a batch at a time, in every format, and nothing of another
/// session, which still loads as it did.
#[tokio::test]
async fn deleting_a_session_removes_everything_it_stored_and_nothing_else_in_every_format() {
    const BATCH: i64 = 16;
    for format in FORMATS {
        let (_directory, db) = database().await;
        let key = SessionSecret::from_bytes([101; 32]);
        let layout = Layout::from(format);
        let target = kickoff_session(&db, &key, layout, 100).await;
        let bystander = kickoff_session(&db, &key, layout, 100).await;
        authorize_refund(&db, &target, "first player").await;
        authorize_refund(&db, &target, "second player").await;
        authorize_refund(&db, &bystander, "first player").await;

        let (_, sealed) = row(&db, &target).await;
        let parts = part_rows(&db, &target).await;
        if format == CheckpointFormat::Monolithic {
            assert!(parts.is_empty());
        } else {
            assert!(
                parts.len() as i64 > 2 * BATCH,
                "{format:?} stores several batches of parts and nodes: {}",
                parts.len()
            );
        }
        let kept_row = row(&db, &bystander).await;
        let kept_parts = part_rows(&db, &bystander).await;
        let kept_state = load(&db, &key, &bystander).await.unwrap().unwrap().state;

        let deleted = delete(&db, &target, BATCH).await.unwrap();
        assert_eq!(
            deleted,
            DeletedSigningState {
                rows: 1,
                row_bytes: sealed.len() as u64,
                parts: parts.len() as u64,
                part_bytes: parts.iter().map(|(_, body)| body.len() as u64).sum(),
                authorizations: 2,
            },
            "{format:?}"
        );
        assert!(load(&db, &key, &target).await.unwrap().is_none());
        assert!(load_metadata(&db, &key, &target).await.unwrap().is_none());
        assert!(part_rows(&db, &target).await.is_empty());
        assert_eq!(authorizations(&db, &target).await, 0);

        assert_eq!(row(&db, &bystander).await, kept_row, "{format:?}");
        assert_eq!(part_rows(&db, &bystander).await, kept_parts, "{format:?}");
        assert_eq!(authorizations(&db, &bystander).await, 1);
        let reloaded = load(&db, &key, &bystander).await.unwrap().unwrap().state;
        assert_eq!(
            serde_json::to_value(&reloaded).unwrap(),
            serde_json::to_value(&kept_state).unwrap()
        );

        assert_eq!(
            delete(&db, &target, BATCH).await.unwrap(),
            DeletedSigningState::default(),
            "a second deletion finds nothing"
        );
        db.close().await.unwrap();
    }
}

/// A writer an operation resumed before its session was deleted cannot write after it: its
/// compare-and-swap matches no row, so it stores no row, part or node, and the session stays
/// gone rather than coming back without its history.
#[tokio::test]
async fn a_writer_resumed_before_its_session_was_deleted_stores_nothing_after() {
    for format in FORMATS {
        let (_directory, db) = database().await;
        let key = SessionSecret::from_bytes([102; 32]);
        let layout = Layout::from(format);
        let session = kickoff_session(&db, &key, layout, 50).await;
        let (writer, mut state) = resume_layout(&db, &key, &session, layout).await;

        delete(&db, &session, DELETE_BATCH_ROWS).await.unwrap();

        state.journal =
            serde_json::from_value(with_command(&state.journal, "payout/late/1")).unwrap();
        assert!(writer.save(&state.journal).await.is_err(), "{format:?}");
        assert!(writer.finish(state).await.is_err(), "{format:?}");
        assert!(load(&db, &key, &session).await.unwrap().is_none());
        assert!(part_rows(&db, &session).await.is_empty(), "{format:?}");
        db.close().await.unwrap();
    }
}

/// A deletion that stopped after its first batch, as when the process stops, leaves a
/// checkpoint that fails closed rather than loading without its parts, and the next deletion
/// removes the rest.
#[tokio::test]
async fn a_deletion_stopped_part_way_fails_closed_until_the_next_one_finishes_it() {
    for format in [CheckpointFormat::Parts, CheckpointFormat::Tree] {
        let (_directory, db) = database().await;
        let key = SessionSecret::from_bytes([103; 32]);
        let session = kickoff_session(&db, &key, Layout::from(format), 50).await;
        let stored = part_rows(&db, &session).await.len() as u64;
        let id = session.to_string();
        db.execute_write(move |pool| async move {
            sqlx::query(
                "DELETE FROM keymeld_protocol_parts WHERE session_id = ? AND digest IN (
                     SELECT digest FROM keymeld_protocol_parts WHERE session_id = ? LIMIT 16)",
            )
            .bind(&id)
            .bind(&id)
            .execute(&pool)
            .await
            .map(|_| ())
        })
        .await
        .unwrap();
        assert!(
            load(&db, &key, &session).await.is_err(),
            "{format:?}: a checkpoint missing parts fails closed"
        );

        let rest = delete(&db, &session, DELETE_BATCH_ROWS).await.unwrap();
        assert_eq!(rest.rows, 1);
        assert_eq!(rest.parts, stored - 16, "{format:?}");
        assert!(load(&db, &key, &session).await.unwrap().is_none());
        db.close().await.unwrap();
    }
}

/// `PRAGMA page_count`, or `freelist_count`, as the connections see the database now.
async fn pages(db: &DBConnection, pragma: &'static str) -> i64 {
    sqlx::query_scalar(pragma)
        .fetch_one(db.read())
        .await
        .unwrap()
}

/// What deleting a kickoff's state costs and frees. The database does not use auto_vacuum, so
/// the freed pages go to SQLite's free list (`PRAGMA incremental_vacuum` would do nothing) and
/// the next checkpoints reuse them before the file grows. The table goes to stderr.
#[tokio::test]
async fn deleted_checkpoint_pages_are_reused_before_the_file_grows() {
    use std::io::Write as _;
    let mut table = String::from(
        "| format | outcomes | pages stored | pages freed | log pages to delete | growth for the next session |\n| --- | --- | --- | --- | --- | --- |\n",
    );
    for (name, layout) in [
        ("1", Layout::Monolithic),
        ("2", Layout::Parts),
        ("3", Layout::Tree(Shape::WRITER)),
    ] {
        let (_directory, db) = database().await;
        assert_eq!(
            pages(&db, "PRAGMA auto_vacuum").await,
            0,
            "the database keeps freed pages on its free list"
        );
        let key = SessionSecret::from_bytes([104; 32]);
        let (size, free) = (
            pages(&db, "PRAGMA page_count").await,
            pages(&db, "PRAGMA freelist_count").await,
        );
        let session = kickoff_session(&db, &key, layout, 1024).await;
        let stored = (pages(&db, "PRAGMA page_count").await - size)
            - (pages(&db, "PRAGMA freelist_count").await - free);
        let (size, free) = (
            pages(&db, "PRAGMA page_count").await,
            pages(&db, "PRAGMA freelist_count").await,
        );
        empty_log(&db).await;
        delete(&db, &session, DELETE_BATCH_ROWS).await.unwrap();
        let logged = logged_pages(&db).await;
        let freed = pages(&db, "PRAGMA freelist_count").await - free;
        assert_eq!(
            pages(&db, "PRAGMA page_count").await,
            size,
            "the file keeps its size"
        );

        kickoff_session(&db, &key, layout, 1024).await;
        let growth = pages(&db, "PRAGMA page_count").await - size;
        table += &format!("| {name} | 1024 | {stored} | {freed} | {logged} | {growth} |\n");
        assert!(
            freed * 4 >= stored * 3,
            "{name}: most pages are freed\n{table}"
        );
        assert!(
            growth * 2 <= stored,
            "{name}: the next session reuses them\n{table}"
        );
        db.close().await.unwrap();
    }
    let _ = writeln!(
        std::io::stderr(),
        "\npages (4 KiB) per kickoff session, its deletion, and the next one:\n{table}"
    );
}
