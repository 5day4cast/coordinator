use super::*;
use crate::domain::{CompetitionStore, SearchBy};
use futures::poll;
use keymeld_sdk::{
    types::{EnclaveId, RegistrationContext},
    SessionId, UserId,
};
use tempfile::TempDir;
use uuid::Uuid;

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(StdDuration::from_secs(10), future)
        .await
        .expect("database operation timed out")
}

async fn database() -> (TempDir, DBConnection) {
    let directory = tempfile::tempdir().unwrap();
    let config = DatabasePoolConfig {
        read_max_connections: 2,
        read_min_connections: 2,
        // Legacy configuration must not produce concurrent writable connections.
        write_max_connections: 8,
        write_min_connections: 2,
        ..DatabasePoolConfig::default()
    };
    let database = bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "users",
        config,
        DatabaseType::Users,
    ))
    .await
    .unwrap();
    bounded(database.execute_write(|pool| async move {
        sqlx::query("CREATE TABLE writer_test (value INTEGER NOT NULL)")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO writer_test VALUES (0)")
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();
    (directory, database)
}

#[tokio::test]
async fn writes_commit_before_reply_and_readers_observe_them_without_checkpoint() {
    let (_directory, database) = database().await;
    assert_eq!(database.write_pool.options().get_max_connections(), 1);
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut write = Box::pin(database.execute_write(move |pool| async move {
        let mut transaction = pool.begin().await?;
        sqlx::query("UPDATE writer_test SET value = 1")
            .execute(&mut *transaction)
            .await?;
        started.send(()).unwrap();
        released.await.unwrap();
        transaction.commit().await?;
        Ok(())
    }));
    assert!(poll!(write.as_mut()).is_pending());
    bounded(start).await.unwrap();

    let before: i64 =
        bounded(sqlx::query_scalar("SELECT value FROM writer_test").fetch_one(database.read()))
            .await
            .unwrap();
    assert_eq!(before, 0);
    release.send(()).unwrap();
    bounded(write).await.unwrap();
    let after: i64 =
        bounded(sqlx::query_scalar("SELECT value FROM writer_test").fetch_one(database.read()))
            .await
            .unwrap();
    assert_eq!(after, 1);
    bounded(database.close()).await.unwrap();
}

#[tokio::test]
async fn readers_reject_persistent_and_temporary_writes() {
    let (_directory, database) = database().await;
    assert!(
        bounded(sqlx::query("UPDATE writer_test SET value = 9").execute(database.read()))
            .await
            .is_err()
    );
    assert!(bounded(
        sqlx::query("CREATE TEMP TABLE bypass (value INTEGER)").execute(database.read())
    )
    .await
    .is_err());
    bounded(database.close()).await.unwrap();
}

#[tokio::test]
async fn full_queue_rejects_new_writes_before_admission() {
    let (_directory, database) = database().await;
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut first = Box::pin(database.execute_write(move |_pool| async move {
        started.send(()).unwrap();
        released.await.unwrap();
        Ok(())
    }));
    assert!(poll!(first.as_mut()).is_pending());
    bounded(start).await.unwrap();

    let mut queued = Vec::new();
    for _ in 0..WRITE_QUEUE_CAPACITY {
        let mut write = Box::pin(database.execute_write(|pool| async move {
            sqlx::query("UPDATE writer_test SET value = value + 1")
                .execute(&pool)
                .await?;
            Ok(())
        }));
        assert!(poll!(write.as_mut()).is_pending());
        queued.push(write);
    }
    let rejected = bounded(database.execute_write(|pool| async move {
        sqlx::query("UPDATE writer_test SET value = -1")
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await;
    assert!(matches!(rejected, Err(DatabaseWriteError::QueueFull)));
    release.send(()).unwrap();
    bounded(first).await.unwrap();
    for write in queued {
        bounded(write).await.unwrap();
    }
    let value: i64 =
        bounded(sqlx::query_scalar("SELECT value FROM writer_test").fetch_one(database.read()))
            .await
            .unwrap();
    assert_eq!(value, WRITE_QUEUE_CAPACITY as i64);
    bounded(database.close()).await.unwrap();
}

#[tokio::test]
async fn shutdown_drains_accepted_writes_after_callers_drop() {
    let (directory, database) = database().await;
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut first = Box::pin(database.execute_write(move |_pool| async move {
        started.send(()).unwrap();
        released.await.unwrap();
        Ok(())
    }));
    assert!(poll!(first.as_mut()).is_pending());
    bounded(start).await.unwrap();
    let mut accepted = Box::pin(database.execute_write(|pool| async move {
        sqlx::query("UPDATE writer_test SET value = 42")
            .execute(&pool)
            .await?;
        Ok(())
    }));
    assert!(poll!(accepted.as_mut()).is_pending());
    drop(accepted);
    drop(first);

    let mut close = Box::pin(database.clone().close());
    assert!(poll!(close.as_mut()).is_pending());
    assert!(matches!(
        database.execute_write(|_| async { Ok(()) }).await,
        Err(DatabaseWriteError::ChannelClosed)
    ));
    release.send(()).unwrap();
    bounded(close).await.unwrap();

    let reopened = bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "users",
        DatabasePoolConfig::default(),
        DatabaseType::Users,
    ))
    .await
    .unwrap();
    let value: i64 =
        bounded(sqlx::query_scalar("SELECT value FROM writer_test").fetch_one(reopened.read()))
            .await
            .unwrap();
    assert_eq!(value, 42);
    bounded(reopened.close()).await.unwrap();
}

#[tokio::test]
async fn shared_memory_readers_are_query_only() {
    let directory = tempfile::tempdir().unwrap();
    let database = bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "users",
        DatabasePoolConfig::testing(),
        DatabaseType::Users,
    ))
    .await
    .unwrap();
    assert!(
        bounded(sqlx::query("CREATE TABLE bypass (value INTEGER)").execute(database.read()))
            .await
            .is_err()
    );
    bounded(database.close()).await.unwrap();
}

#[tokio::test]
async fn writer_failure_reports_unknown_outcome_and_notifies_supervision() {
    let (_directory, database) = database().await;
    let mut stopped = Box::pin(database.writer_stopped());
    assert!(poll!(stopped.as_mut()).is_pending());
    let result = bounded(database.execute_write::<(), _, _>(|_| async {
        panic!("simulated writer failure");
    }))
    .await;
    assert!(matches!(
        result,
        Err(DatabaseWriteError::ResultChannelClosed)
    ));
    bounded(stopped).await;
    assert!(matches!(
        database.ping().await,
        Err(sqlx::Error::PoolClosed)
    ));
    assert!(bounded(database.close()).await.is_err());
}

#[tokio::test]
async fn registration_migration_preserves_legacy_entries_and_new_context_roundtrips() {
    let directory = tempfile::tempdir().unwrap();
    let legacy_migrations = directory.path().join("legacy-migrations");
    std::fs::create_dir(&legacy_migrations).unwrap();
    let migration_source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/competitions");
    for file in std::fs::read_dir(migration_source).unwrap() {
        let file = file.unwrap();
        if file.file_name().to_str().unwrap() >= "20260916000000" {
            continue;
        }
        std::fs::copy(file.path(), legacy_migrations.join(file.file_name())).unwrap();
    }
    let legacy = bounded(
        SqlitePoolOptions::new().max_connections(1).connect_with(
            SqliteConnectOptions::new()
                .filename(directory.path().join("competitions.db"))
                .create_if_missing(true),
        ),
    )
    .await
    .unwrap();
    let migrator = sqlx::migrate::Migrator::new(legacy_migrations)
        .await
        .unwrap();
    bounded(migrator.run(&legacy)).await.unwrap();
    let event_id = Uuid::now_v7();
    let old_ticket = Uuid::now_v7();
    let new_ticket = Uuid::now_v7();
    let old_entry = Uuid::now_v7();
    let duplicate_tickets = [Uuid::now_v7(), Uuid::now_v7()];
    sqlx::query("INSERT INTO competitions (id, created_at, event_submission) VALUES (?, datetime('now'), '{}')")
        .bind(event_id.to_string())
        .execute(&legacy)
        .await
        .unwrap();
    for ticket in [
        old_ticket,
        new_ticket,
        duplicate_tickets[0],
        duplicate_tickets[1],
    ] {
        sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'encrypted', 'hash')")
            .bind(ticket.to_string())
            .bind(event_id.to_string())
            .execute(&legacy)
            .await
            .unwrap();
    }
    let submission = serde_json::json!({
        "id": old_entry,
        "event_id": event_id,
        "expected_observations": [],
    });
    sqlx::query("INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey, ephemeral_privatekey_encrypted, payout_preimage_encrypted, payout_hash, entry_submission) VALUES (?, ?, ?, 'owner', 'ephemeral', 'encrypted', 'encrypted', 'hash', ?)")
        .bind(old_entry.to_string())
        .bind(event_id.to_string())
        .bind(old_ticket.to_string())
        .bind(serde_json::to_vec(&submission).unwrap())
        .execute(&legacy)
        .await
        .unwrap();
    bounded(legacy.close()).await;

    let database = bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    ))
    .await
    .unwrap();
    let store = CompetitionStore::new(database.clone());
    let legacy_entry = bounded(store.get_entry_by_id(old_entry))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(legacy_entry.pubkey, "owner");
    assert!(legacy_entry.keymeld_registration_context.is_none());
    let context = RegistrationContext {
        keygen_session_id: SessionId::new_v7(),
        manifest_hash: vec![1; 32],
        user_id: UserId::new_v7(),
        enclave_id: EnclaveId::new(1),
        enclave_key_epoch: 2,
        public_key: vec![2; 33],
        auth_pubkey: vec![3; 33],
        require_signing_approval: false,
    };
    let mut new_entry = legacy_entry;
    new_entry.id = Uuid::now_v7();
    new_entry.ticket_id = new_ticket;
    new_entry.entry_submission.id = new_entry.id;
    new_entry.ephemeral_pubkey = "ephemeral-2".into();
    new_entry.payout_hash = "hash-2".into();
    new_entry.keymeld_registration_context = Some(context.clone());
    bounded(store.add_entry(new_entry.clone(), new_ticket))
        .await
        .unwrap();

    // Every entry needs its own DLC key and payout hash.
    for (ticket, (ephemeral_pubkey, payout_hash)) in duplicate_tickets
        .into_iter()
        .zip([("ephemeral", "hash-3"), ("ephemeral-3", "hash")])
    {
        let mut duplicate = new_entry.clone();
        duplicate.id = Uuid::now_v7();
        duplicate.ticket_id = ticket;
        duplicate.entry_submission.id = duplicate.id;
        duplicate.ephemeral_pubkey = ephemeral_pubkey.into();
        duplicate.payout_hash = payout_hash.into();
        assert!(
            bounded(store.add_entry(duplicate, ticket)).await.is_err(),
            "duplicate ephemeral pubkey or payout hash must be rejected"
        );
    }
    let loaded = bounded(store.get_entry_by_id(new_entry.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.keymeld_registration_context, Some(context.clone()));
    for entries in [
        bounded(store.get_competition_entries(event_id, vec![]))
            .await
            .unwrap(),
        bounded(store.get_user_entries("owner".into(), SearchBy { event_ids: None }))
            .await
            .unwrap(),
    ] {
        assert_eq!(entries.len(), 2);
        let loaded = entries
            .iter()
            .find(|entry| entry.id == new_entry.id)
            .unwrap();
        assert_eq!(loaded.keymeld_registration_context, Some(context.clone()));
    }
    bounded(database.close()).await.unwrap();
}

#[tokio::test]
async fn failed_competition_delete_preserves_related_tickets() {
    let directory = tempfile::tempdir().unwrap();
    let database = bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    ))
    .await
    .unwrap();
    let event_id = Uuid::now_v7();
    bounded(database.execute_write(move |pool| async move {
        sqlx::query("INSERT INTO competitions (id, created_at, event_submission) VALUES (?, datetime('now'), '{}')")
            .bind(event_id.to_string())
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'encrypted', 'hash')")
            .bind(Uuid::now_v7().to_string())
            .bind(event_id.to_string())
            .execute(&pool)
            .await?;
        sqlx::query("CREATE TRIGGER fail_competition_delete BEFORE DELETE ON competitions BEGIN SELECT RAISE(ABORT, 'simulated failure'); END")
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();
    let store = CompetitionStore::new(database.clone());
    assert!(bounded(store.delete_competition(event_id)).await.is_err());
    let count: i64 =
        bounded(sqlx::query_scalar("SELECT count(*) FROM tickets").fetch_one(database.read()))
            .await
            .unwrap();
    assert_eq!(count, 1);
    bounded(database.close()).await.unwrap();
}

#[tokio::test]
async fn taking_over_a_stale_reservation_rotates_the_ticket_and_reports_the_old_invoice() {
    let directory = tempfile::tempdir().unwrap();
    let database = bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    ))
    .await
    .unwrap();
    let store = CompetitionStore::new(database.clone());
    let event_id = Uuid::now_v7();
    let ticket_id = Uuid::now_v7();
    bounded(database.execute_write(move |pool| async move {
        sqlx::query("INSERT INTO competitions (id, created_at, event_submission) VALUES (?, datetime('now'), '{}')")
            .bind(event_id.to_string())
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'old-preimage', 'old-hash')")
            .bind(ticket_id.to_string())
            .bind(event_id.to_string())
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();

    // Alice reserves the ticket, receives an invoice, then goes quiet past the window.
    let alice = bounded(store.get_and_reserve_ticket(event_id, "alice"))
        .await
        .unwrap();
    assert!(alice.superseded_payment_hash.is_none());
    bounded(store.update_ticket_payment_request(
        &alice.ticket,
        "lnbc-alice",
        OffsetDateTime::now_utc() + time::Duration::hours(1),
    ))
    .await
    .unwrap();
    bounded(database.execute_write(move |pool| async move {
        sqlx::query("UPDATE tickets SET reserved_at = datetime('now', '-11 minutes') WHERE id = ?")
            .bind(ticket_id.to_string())
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();

    // Bob takes it over: fresh hash and preimage, no invoice, Alice's hash reported.
    let bob = bounded(store.get_and_reserve_ticket(event_id, "bob"))
        .await
        .unwrap();
    assert_eq!(bob.superseded_payment_hash.as_deref(), Some("old-hash"));
    assert_eq!(bob.ticket.id, ticket_id);
    assert_eq!(bob.ticket.reserved_by.as_deref(), Some("bob"));
    assert_ne!(bob.ticket.hash, "old-hash");
    assert_ne!(bob.ticket.encrypted_preimage, "old-preimage");
    assert!(bob.ticket.payment_request.is_none());

    // An in-flight request or watcher holding Alice's old snapshot must not
    // clear Bob's reservation or overwrite his invoice and escrow.
    assert!(!bounded(store.clear_ticket_reservation(&alice.ticket))
        .await
        .unwrap());
    assert!(!bounded(store.update_ticket_payment_request(
        &alice.ticket,
        "stale-invoice",
        OffsetDateTime::now_utc()
    ))
    .await
    .unwrap());
    assert!(!bounded(store.update_ticket_escrow(
        &alice.ticket,
        "alice-key".into(),
        "stale-escrow".into()
    ))
    .await
    .unwrap());
    let current = bounded(store.get_ticket(ticket_id)).await.unwrap();
    assert_eq!(current.reserved_by.as_deref(), Some("bob"));
    assert_eq!(current.hash, bob.ticket.hash);
    assert!(current.payment_request.is_none());
    assert!(current.escrow_transaction.is_none());

    // A late payment on Alice's invoice can never be credited to Bob's ticket.
    assert!(!bounded(store.mark_ticket_paid("old-hash", event_id))
        .await
        .unwrap());
    assert!(bounded(store.mark_ticket_paid(&bob.ticket.hash, event_id))
        .await
        .unwrap());

    // A paid ticket is never released, even if a later request fails.
    assert!(!bounded(store.clear_ticket_reservation(&bob.ticket))
        .await
        .unwrap());
    bounded(database.close()).await.unwrap();
}

async fn ticket_database() -> (TempDir, DBConnection, CompetitionStore, Uuid, Uuid) {
    let directory = tempfile::tempdir().unwrap();
    let database = bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    ))
    .await
    .unwrap();
    let event_id = Uuid::now_v7();
    let ticket_id = Uuid::now_v7();
    bounded(database.execute_write(move |pool| async move {
        sqlx::query("INSERT INTO competitions (id, created_at, event_submission) VALUES (?, datetime('now'), '{}')")
            .bind(event_id.to_string()).execute(&pool).await?;
        sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'old-preimage', 'old-hash')")
            .bind(ticket_id.to_string()).bind(event_id.to_string()).execute(&pool).await?;
        Ok(())
    })).await.unwrap();
    let store = CompetitionStore::new(database.clone());
    (directory, database, store, event_id, ticket_id)
}

#[tokio::test]
async fn releasing_an_invoice_rotates_the_hash_and_preserves_a_concurrently_issued_invoice() {
    let (_directory, database, store, event_id, ticket_id) = ticket_database().await;
    let unissued = bounded(store.get_and_reserve_ticket(event_id, "alice"))
        .await
        .unwrap()
        .ticket;
    assert!(bounded(store.update_ticket_payment_request(
        &unissued,
        "alice-invoice",
        OffsetDateTime::now_utc() + time::Duration::hours(1),
    ))
    .await
    .unwrap());
    // A duplicate request failed before seeing the invoice saved by its peer.
    assert!(!bounded(store.clear_ticket_reservation(&unissued))
        .await
        .unwrap());
    let issued = bounded(store.get_ticket(ticket_id)).await.unwrap();
    assert!(bounded(store.clear_ticket_reservation(&issued))
        .await
        .unwrap());
    let next = bounded(store.get_and_reserve_ticket(event_id, "bob"))
        .await
        .unwrap()
        .ticket;
    assert_ne!(next.hash, issued.hash);
    assert_ne!(next.encrypted_preimage, issued.encrypted_preimage);
    assert!(!bounded(store.mark_ticket_paid(&issued.hash, event_id))
        .await
        .unwrap());
    assert!(!bounded(store.clear_ticket_reservation(&issued))
        .await
        .unwrap());
    bounded(database.close()).await.unwrap();
}

#[tokio::test]
async fn renewing_an_expired_invoice_rotates_its_hash_even_for_the_same_owner() {
    let (_directory, database, store, event_id, ticket_id) = ticket_database().await;
    let original = bounded(store.get_and_reserve_ticket(event_id, "alice"))
        .await
        .unwrap()
        .ticket;
    bounded(store.update_ticket_payment_request(
        &original,
        "expired-invoice",
        OffsetDateTime::now_utc() - time::Duration::seconds(1),
    ))
    .await
    .unwrap();
    let renewed = bounded(store.get_and_reserve_ticket(event_id, "alice"))
        .await
        .unwrap();
    assert_eq!(renewed.ticket.id, ticket_id);
    assert_eq!(
        renewed.superseded_payment_hash.as_deref(),
        Some(original.hash.as_str())
    );
    assert_ne!(renewed.ticket.hash, original.hash);
    assert!(renewed.ticket.payment_request.is_none());
    bounded(database.close()).await.unwrap();
}

#[tokio::test]
async fn concurrent_payouts_are_exclusive_and_terminal_states_cannot_be_overwritten() {
    use crate::domain::PayoutError;
    let (_directory, database, store, event_id, ticket_id) = ticket_database().await;
    let entry_id = Uuid::now_v7();
    bounded(database.execute_write(move |pool| async move {
        sqlx::query("INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey, ephemeral_privatekey_encrypted, payout_preimage_encrypted, payout_hash, entry_submission) VALUES (?, ?, ?, 'owner', 'ephemeral', '', '', 'hash', '{}')")
            .bind(entry_id.to_string()).bind(event_id.to_string()).bind(ticket_id.to_string())
            .execute(&pool).await?;
        Ok(())
    })).await.unwrap();
    let start = || {
        store.store_payout_info_pending(
            entry_id,
            "preimage".into(),
            "private".into(),
            "invoice".into(),
            10,
        )
    };
    let (first, second) = bounded(async { tokio::join!(start(), start()) }).await;
    assert_ne!(first.is_ok(), second.is_ok());
    let first_id = first.or(second).unwrap();
    let pending = bounded(store.get_all_pending_payouts()).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, first_id);
    bounded(store.mark_payout_failed(
        first_id,
        OffsetDateTime::now_utc(),
        PayoutError::FailedToPayOut("confirmed failure".into()),
    ))
    .await
    .unwrap();
    bounded(store.mark_payout_succeeded(first_id, OffsetDateTime::now_utc()))
        .await
        .unwrap();
    assert!(bounded(store.get_payout(first_id))
        .await
        .unwrap()
        .unwrap()
        .succeed_at
        .is_none());
    let retry_id = bounded(start()).await.unwrap();
    bounded(store.mark_payout_succeeded(retry_id, OffsetDateTime::now_utc()))
        .await
        .unwrap();
    bounded(store.mark_payout_failed(
        retry_id,
        OffsetDateTime::now_utc(),
        PayoutError::FailedToPayOut("stale failure".into()),
    ))
    .await
    .unwrap();
    let succeeded = bounded(store.get_payout(retry_id)).await.unwrap().unwrap();
    assert!(succeeded.succeed_at.is_some());
    assert!(succeeded.failed_at.is_none());
    assert!(bounded(start()).await.is_err());
    bounded(database.close()).await.unwrap();
}
