use super::*;
use crate::infra::db::{DatabasePoolConfig, DatabaseType};
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
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

/// A writer for a state the test wrote itself. It knows of no stored parts, as after loading a
/// format-1 row.
fn make_checkpoint(
    db: DBConnection,
    key: SessionSecret,
    session: SessionId,
    version: i64,
    state: &ProtocolState,
    format: CheckpointFormat,
) -> DurableCheckpoint {
    let written = Loaded {
        version,
        state: state.clone(),
        parts: BTreeSet::new(),
    };
    DurableCheckpoint::new(db, key, session, &written, format).unwrap()
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
    for format in [CheckpointFormat::Monolithic, CheckpointFormat::Parts] {
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
    for format in [CheckpointFormat::Monolithic, CheckpointFormat::Parts] {
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
    );
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
    );
    let second = make_checkpoint(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        &original,
        format,
    );
    let mut left = original.clone();
    left.session.encrypted_session_secret = "first committed state".into();
    let mut right = original;
    right.session.encrypted_session_secret = "second committed state".into();
    let (a, b) = tokio::join!(first.finish(&left), second.finish(&right));
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
    );
    let winner = make_checkpoint(
        db.clone(),
        key.clone(),
        session_id.clone(),
        0,
        &state,
        format,
    );
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
    );
    checkpoint.finish(&original).await.unwrap();
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
    checkpoint.finish(&original).await.unwrap();
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
    );
    assert!(stale.finish(&original).await.is_err());
    assert_eq!(
        read_parts().await,
        first,
        "failed CAS must leave parts unchanged"
    );
    let mut changed = original.clone();
    changed.session.encrypted_session_secret = "another private checkpoint ".repeat(8192);
    checkpoint.finish(&changed).await.unwrap();
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
    );
    writer.finish(&original).await.unwrap();
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
    );
    legacy.finish(&original).await.unwrap();
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
        CheckpointFormat::Parts,
    );
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
    );
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
    let restarted = DurableCheckpoint::new(
        db.clone(),
        key.clone(),
        session.clone(),
        &loaded,
        CheckpointFormat::Parts,
    )
    .unwrap();
    let mut restored = loaded.state;
    restarted.save(&restored.journal).await.unwrap();
    restored.session.encrypted_session_secret = "updated static state".into();
    restarted.finish(&restored).await.unwrap();
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
    );
    legacy.finish(&restored).await.unwrap();
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
    .finish(&original)
    .await
    .unwrap();
    let stored = part_rows(&db, &session).await;
    let loaded = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(loaded.version, 1);
    let resume = |loaded: &Loaded| {
        DurableCheckpoint::new(
            db.clone(),
            key.clone(),
            session.clone(),
            loaded,
            CheckpointFormat::Parts,
        )
        .unwrap()
    };
    // Two operations resume from the same version.
    let (first, second) = (resume(&loaded), resume(&loaded));
    for writer in [&first, &second] {
        assert!(
            matches!(&writer.current.lock().await.1, Snapshot::Parts { pending, .. } if pending.is_empty()),
            "a resumed writer encrypts none of the fields it loaded"
        );
    }

    // Saving the unchanged journal compresses, encrypts and inserts no part.
    run(&db, "CREATE TRIGGER refuse_part_insert BEFORE INSERT ON keymeld_protocol_parts BEGIN SELECT RAISE(ABORT, 'no part may be inserted'); END").await;
    first.save(&loaded.state.journal).await.unwrap();
    run(&db, "DROP TRIGGER refuse_part_insert").await;
    assert_eq!(row(&db, &session).await.0, 2);
    assert_eq!(part_rows(&db, &session).await, stored);

    // `first` removes an entry and with it the entry's part. `second` still counts that part as
    // stored, but its compare-and-swap names version 1, so it fails and changes nothing.
    let mut shorter = serde_json::to_value(&loaded.state.journal).unwrap();
    shorter["commands"]
        .as_object_mut()
        .unwrap()
        .remove("fixture/0/1");
    let removed: ConfidentialJournal = serde_json::from_value(shorter.clone()).unwrap();
    first.save(&removed).await.unwrap();
    let after_removal = part_rows(&db, &session).await;
    assert_eq!(after_removal.len(), stored.len() - 1);
    assert!(second.save(&loaded.state.journal).await.is_err());
    assert_eq!(second.current.lock().await.0, 1);
    assert_eq!(row(&db, &session).await.0, 3);
    assert_eq!(part_rows(&db, &session).await, after_removal);

    // A reload resumes from the version that won and stores the removed entry's part again.
    let reloaded = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&reloaded.state.journal).unwrap(),
        shorter
    );
    resume(&reloaded).save(&loaded.state.journal).await.unwrap();
    let restored = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(restored.version, 4);
    assert_eq!(
        serde_json::to_value(&restored.state.journal).unwrap(),
        serde_json::to_value(&loaded.state.journal).unwrap()
    );
    db.close().await.unwrap();
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
    for format in [CheckpointFormat::Monolithic, CheckpointFormat::Parts] {
        let version = row(&db, &session).await.0;
        let writer = make_checkpoint(
            db.clone(),
            key.clone(),
            session.clone(),
            version,
            &original,
            format,
        );
        writer.finish(&original).await.unwrap();
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
    for format in [CheckpointFormat::Monolithic, CheckpointFormat::Parts] {
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
            );
            writer.finish(&original).await.unwrap();
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
    );
    let writes = async {
        for version in 1..=16 {
            original.epochs.insert(EnclaveId::new(1), version);
            original.session.encrypted_session_secret = version.to_string();
            writer.finish(&original).await.unwrap();
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
    );
    checkpoint.finish(&original).await.unwrap();
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
