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
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([23; 32]);
    let mut state = state(&session);
    state.session.encrypted_session_secret = "private journal marker ".repeat(8192);
    assert!(serde_json::to_vec(&state).unwrap().len() > 64 * 1024);
    assert!(create(&db, &key, &session, &state).await.unwrap());
    assert!(!create(&db, &key, &session, &state).await.unwrap());
    let (version, ciphertext) = row(&db, &session).await;
    assert_eq!(version, 0);
    assert!(!ciphertext.contains("private journal marker"));
    let (_, loaded) = load(&db, &key, &session).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::to_value(&state).unwrap()
    );
    let checkpoint =
        DurableCheckpoint::new(db.clone(), key.clone(), session.clone(), version, loaded);
    checkpoint
        .save(&ConfidentialJournal::default())
        .await
        .unwrap();
    assert_eq!(row(&db, &session).await.0, 1);
    assert_eq!(
        load(&db, &key, &session)
            .await
            .unwrap()
            .unwrap()
            .1
            .session
            .encrypted_session_secret,
        state.session.encrypted_session_secret
    );
    drop(checkpoint);
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
    let session = SessionId::new_v7();
    let key = SessionSecret::from_bytes([26; 32]);
    let original = state(&session);
    create(&db, &key, &session, &original).await.unwrap();
    let first = DurableCheckpoint::new(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        original.clone(),
    );
    let second = DurableCheckpoint::new(
        db.clone(),
        key.clone(),
        session.clone(),
        0,
        original.clone(),
    );
    let mut left = original.clone();
    left.session.encrypted_session_secret = "first committed state".into();
    let mut right = original;
    right.session.encrypted_session_secret = "second committed state".into();
    let (a, b) = tokio::join!(first.finish(left), second.finish(right));
    assert_ne!(a.is_ok(), b.is_ok());
    let (version, loaded) = load(&db, &key, &session).await.unwrap().unwrap();
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
    assert_eq!(row(&db, &session).await.0, 1);
    drop(first);
    drop(second);
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
    let session_id = SessionId::new_v7();
    let key = SessionSecret::from_bytes([27; 32]);
    let state = state(&session_id);
    create(&db, &key, &session_id, &state).await.unwrap();
    let stale = DurableCheckpoint::new(
        db.clone(),
        key.clone(),
        session_id.clone(),
        0,
        state.clone(),
    );
    let winner = DurableCheckpoint::new(
        db.clone(),
        key.clone(),
        session_id.clone(),
        0,
        state.clone(),
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
    assert_eq!(row(&db, &session_id).await.0, 1);
    drop(session);
    drop(stale);
    drop(winner);
    server.abort();
    db.close().await.unwrap();
}
