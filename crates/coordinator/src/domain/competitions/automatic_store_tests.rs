//! Persistence boundaries for automatic payout recovery, using the real migrations.
use super::{CompetitionStore, PayoutError};
use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
use bitcoin::{
    hashes::{sha256, Hash},
    secp256k1::{Secp256k1, SecretKey},
};
use dlctix::{hashlock, secp::Scalar};
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use std::{future::Future, time::Duration};
use tempfile::TempDir;
use time::OffsetDateTime;
use uuid::Uuid;

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("automatic payout database operation timed out")
}

struct Fixture {
    directory: TempDir,
    database: DBConnection,
    store: CompetitionStore,
    event_id: Uuid,
    entry_id: Uuid,
    ticket_id: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = open(&directory).await;
        let event_id = Uuid::now_v7();
        bounded(database.execute_write(move |pool| async move {
            sqlx::query("INSERT INTO competitions (id, created_at, event_submission) VALUES (?, datetime('now'), '{}')")
                .bind(event_id.to_string()).execute(&pool).await?;
            Ok(())
        })).await.unwrap();
        let store = CompetitionStore::new(database.clone());
        let mut fixture = Self {
            directory,
            database,
            store,
            event_id,
            entry_id: Uuid::nil(),
            ticket_id: Uuid::nil(),
        };
        (fixture.entry_id, fixture.ticket_id) = fixture.add_entry(1).await;
        fixture
    }

    async fn add_entry(&self, key_byte: u8) -> (Uuid, Uuid) {
        let entry_id = Uuid::now_v7();
        let ticket_id = Uuid::now_v7();
        let event_id = self.event_id;
        let pubkey = Scalar::from_slice(&[key_byte; 32])
            .unwrap()
            .base_point_mul()
            .to_string();
        let payout_hash = hex::encode(hashlock::sha256(&[key_byte + 1; 32]));
        bounded(self.database.execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'encrypted', ?)")
                .bind(ticket_id.to_string()).bind(event_id.to_string()).bind(format!("ticket-{ticket_id}"))
                .execute(&mut *tx).await?;
            sqlx::query("INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey, payout_hash, entry_submission) VALUES (?, ?, ?, 'owner', ?, ?, '{}')")
                .bind(entry_id.to_string()).bind(event_id.to_string()).bind(ticket_id.to_string())
                .bind(pubkey).bind(payout_hash).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(())
        })).await.unwrap();
        (entry_id, ticket_id)
    }

    async fn assert_no_secrets(&self) {
        let secrets: (Option<String>, Option<String>) = bounded(
            sqlx::query_as(
                "SELECT ephemeral_privatekey, payout_preimage FROM entries WHERE id = ?",
            )
            .bind(self.entry_id.to_string())
            .fetch_one(self.database.read()),
        )
        .await
        .unwrap();
        assert_eq!(secrets, (None, None));
    }

    async fn prepare(&self, job: Uuid, proof: [u8; 32]) -> Uuid {
        bounded(
            self.store
                .store_prepared_payout(job, invoice(proof), 10, prepared(proof)),
        )
        .await
        .unwrap()
    }

    async fn count(&self, table: &str) -> i64 {
        assert!(matches!(
            table,
            "payout_jobs" | "payouts" | "payout_payment_hashes"
        ));
        bounded(
            sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(self.database.read()),
        )
        .await
        .unwrap()
    }
}

async fn open(directory: &TempDir) -> DBConnection {
    bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    ))
    .await
    .unwrap()
}

fn invoice(proof: [u8; 32]) -> String {
    InvoiceBuilder::new(Currency::Regtest)
        .description("automatic payout regression".into())
        .payment_hash(sha256::Hash::hash(&proof))
        .payment_secret(PaymentSecret([8; 32]))
        .amount_milli_satoshis(10_000)
        .duration_since_epoch(Duration::from_secs(1_800_000_000))
        .min_final_cltv_expiry_delta(18)
        .build_signed(|hash| {
            Secp256k1::new().sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[9; 32]).unwrap())
        })
        .unwrap()
        .to_string()
}

fn prepared(proof: [u8; 32]) -> String {
    serde_json::json!({"invoice": invoice(proof), "state_receipt": "sealed-claim"}).to_string()
}

#[tokio::test]
async fn concurrent_workers_persist_one_intent_before_preparing_an_invoice() {
    let f = Fixture::new().await;
    let create = || {
        f.store
            .create_payout_job(f.entry_id, "\"Automatic\"".into())
    };
    let (a, b) = bounded(async { tokio::join!(create(), create()) }).await;
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(f.count("payout_jobs").await, 1);
    assert_eq!(f.count("payouts").await, 0);
    f.assert_no_secrets().await;
    let jobs = bounded(f.store.due_payout_jobs()).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].prepared_json.is_none());
    assert!(jobs[0].payout_id.is_none());
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn pending_payment_batch_rotates_before_new_claims_and_paid_recovery() {
    let f = Fixture::new().await;
    let mut stalled = Vec::new();
    for index in 1..=100u8 {
        let entry = if index == 1 {
            f.entry_id
        } else {
            f.add_entry(index).await.0
        };
        let job = bounded(f.store.create_payout_job(entry, "automatic".into()))
            .await
            .unwrap();
        f.prepare(job, [index; 32]).await;
        stalled.push(job);
    }
    let new_entry = f.add_entry(101).await.0;
    let new_claim = bounded(f.store.create_payout_job(new_entry, "automatic".into()))
        .await
        .unwrap();
    let paid_entry = f.add_entry(102).await.0;
    let paid_claim = bounded(f.store.create_payout_job(paid_entry, "automatic".into()))
        .await
        .unwrap();
    let paid_outbox = f.prepare(paid_claim, [102; 32]).await;
    bounded(f.store.mark_payout_succeeded(
        paid_outbox,
        OffsetDateTime::now_utc(),
        Some(hex::encode([102; 32])),
    ))
    .await
    .unwrap();
    bounded(f.database.execute_write(|pool| async move {
        sqlx::query("UPDATE payout_jobs SET retry_at = 0")
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();

    let first_batch = bounded(f.store.due_payout_jobs()).await.unwrap();
    assert_eq!(first_batch.len(), 100);
    assert!(first_batch.iter().all(|job| stalled.contains(&job.id)));
    for job in first_batch {
        bounded(f.store.schedule_payout_poll(job.id)).await.unwrap();
    }
    let next_batch = bounded(f.store.due_payout_jobs()).await.unwrap();
    assert_eq!(
        next_batch.iter().map(|job| job.id).collect::<Vec<_>>(),
        vec![new_claim, paid_claim]
    );

    // Even if processing one batch takes longer than the polling interval,
    // newly due polls must remain behind the work that has never been polled.
    bounded(f.database.execute_write(|pool| async move {
        sqlx::query("UPDATE payout_jobs SET retry_at = ? WHERE retry_at > 0")
            .bind(OffsetDateTime::now_utc().unix_timestamp() - 1)
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();
    let next_batch = bounded(f.store.due_payout_jobs()).await.unwrap();
    assert_eq!(next_batch.len(), 100);
    assert_eq!(next_batch[0].id, new_claim);
    assert_eq!(next_batch[1].id, paid_claim);
    assert!(next_batch.iter().all(|job| job.attempts == 0));
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn prepared_invoice_retry_reuses_outbox_and_rejects_changed_receipt() {
    let f = Fixture::new().await;
    let job = bounded(f.store.create_payout_job(f.entry_id, "automatic".into()))
        .await
        .unwrap();
    let first = f.prepare(job, [7; 32]).await;
    assert_eq!(first, f.prepare(job, [7; 32]).await);
    assert!(bounded(f.store.store_prepared_payout(
        job,
        invoice([7; 32]),
        10,
        "changed-receipt".into()
    ))
    .await
    .is_err());
    assert_eq!(f.count("payouts").await, 1);
    assert_eq!(f.count("payout_payment_hashes").await, 1);
    let jobs = bounded(f.store.due_payout_jobs()).await.unwrap();
    assert_eq!(jobs[0].payout_id, Some(first));
    assert_eq!(
        jobs[0].prepared_json.as_deref(),
        Some(prepared([7; 32]).as_str())
    );
    f.assert_no_secrets().await;
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn one_payment_hash_cannot_pay_two_entries_and_rejection_rolls_back_outbox() {
    let f = Fixture::new().await;
    let (second_entry, _) = f.add_entry(3).await;
    let first = bounded(f.store.create_payout_job(f.entry_id, "automatic".into()))
        .await
        .unwrap();
    let second = bounded(f.store.create_payout_job(second_entry, "automatic".into()))
        .await
        .unwrap();
    f.prepare(first, [7; 32]).await;
    assert!(bounded(f.store.store_prepared_payout(
        second,
        invoice([7; 32]),
        10,
        prepared([7; 32])
    ))
    .await
    .is_err());
    assert_eq!(f.count("payouts").await, 1);
    assert_eq!(f.count("payout_payment_hashes").await, 1);
    let jobs = bounded(f.store.due_payout_jobs()).await.unwrap();
    let rejected = jobs.iter().find(|job| job.id == second).unwrap();
    assert!(rejected.payout_id.is_none());
    assert!(rejected.prepared_json.is_none());
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn entry_secrets_require_matching_payment_proof_and_matching_escrow_material() {
    let f = Fixture::new().await;
    let job = bounded(f.store.create_payout_job(f.entry_id, "automatic".into()))
        .await
        .unwrap();
    let payout = f.prepare(job, [7; 32]).await;
    let complete = || {
        f.store
            .complete_payout_job(job, hex::encode([1; 32]), hex::encode([2; 32]))
    };
    assert!(bounded(complete()).await.is_err());
    assert!(bounded(f.store.mark_payout_succeeded(
        payout,
        OffsetDateTime::now_utc(),
        Some(hex::encode([8; 32]))
    ))
    .await
    .is_err());
    let row = bounded(f.store.get_payout(payout)).await.unwrap().unwrap();
    assert!(row.succeed_at.is_none());
    assert!(row.payment_preimage.is_none());
    // A success notification without the proof is not enough to release secrets.
    bounded(
        f.store
            .mark_payout_succeeded(payout, OffsetDateTime::now_utc(), None),
    )
    .await
    .unwrap();
    assert!(bounded(complete()).await.is_err());
    bounded(f.store.mark_payout_succeeded(
        payout,
        OffsetDateTime::now_utc(),
        Some(hex::encode([7; 32])),
    ))
    .await
    .unwrap();
    assert!(bounded(
        f.store
            .complete_payout_job(job, hex::encode([3; 32]), hex::encode([2; 32]))
    )
    .await
    .is_err());
    assert!(bounded(
        f.store
            .complete_payout_job(job, hex::encode([1; 32]), hex::encode([4; 32]))
    )
    .await
    .is_err());
    f.assert_no_secrets().await;
    bounded(complete()).await.unwrap();
    bounded(complete()).await.unwrap();
    assert!(bounded(f.store.due_payout_jobs()).await.unwrap().is_empty());
    assert_eq!(
        bounded(f.store.payout_job_status(f.entry_id))
            .await
            .unwrap()
            .as_deref(),
        Some("Paid")
    );
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn restart_after_payment_recovers_same_claim_and_proof_without_new_payment() {
    let mut f = Fixture::new().await;
    let job = bounded(f.store.create_payout_job(f.entry_id, "automatic".into()))
        .await
        .unwrap();
    let payout = f.prepare(job, [7; 32]).await;
    bounded(f.store.mark_payout_succeeded(
        payout,
        OffsetDateTime::now_utc(),
        Some(hex::encode([7; 32])),
    ))
    .await
    .unwrap();
    f.assert_no_secrets().await;
    bounded(f.database.close()).await.unwrap();
    f.database = open(&f.directory).await;
    f.store = CompetitionStore::new(f.database.clone());
    let jobs = bounded(f.store.due_payout_jobs()).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, job);
    assert_eq!(jobs[0].payout_id, Some(payout));
    assert_eq!(
        jobs[0].prepared_json.as_deref(),
        Some(prepared([7; 32]).as_str())
    );
    let paid = bounded(f.store.get_payout(payout)).await.unwrap().unwrap();
    assert_eq!(
        paid.payment_preimage.as_deref(),
        Some(hex::encode([7; 32]).as_str())
    );
    assert!(paid.succeed_at.is_some());
    assert!(
        bounded(f.store.create_payout_job(f.entry_id, "replacement".into()))
            .await
            .is_err()
    );
    bounded(
        f.store
            .complete_payout_job(job, hex::encode([1; 32]), hex::encode([2; 32])),
    )
    .await
    .unwrap();
    assert_eq!(f.count("payouts").await, 1);
    assert!(bounded(f.store.due_payout_jobs()).await.unwrap().is_empty());
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn invoice_fallback_replaces_only_unpaid_intent_and_is_idempotent() {
    let f = Fixture::new().await;
    let original = bounded(f.store.create_payout_job(f.entry_id, "automatic".into()))
        .await
        .unwrap();
    let fallback = Uuid::now_v7();
    bounded(
        f.store
            .queue_invoice_fallback(fallback, f.entry_id, "signed-invoice".into()),
    )
    .await
    .unwrap();
    assert_eq!(
        bounded(
            f.store
                .queue_invoice_fallback(fallback, f.entry_id, "signed-invoice".into())
        )
        .await
        .unwrap(),
        fallback
    );
    assert!(bounded(f.store.queue_invoice_fallback(
        fallback,
        f.entry_id,
        "different-invoice".into()
    ))
    .await
    .is_err());
    assert!(bounded(f.store.store_prepared_payout(
        original,
        invoice([7; 32]),
        10,
        prepared([7; 32])
    ))
    .await
    .is_err());
    let payout = f.prepare(fallback, [7; 32]).await;
    assert!(bounded(f.store.queue_invoice_fallback(
        Uuid::now_v7(),
        f.entry_id,
        "replacement".into()
    ))
    .await
    .is_err());
    // A retry/timeout is ambiguous: it must not release the entry for a new payment.
    bounded(
        f.store
            .retry_payout_job(fallback, 0, "LND unavailable".into()),
    )
    .await
    .unwrap();
    bounded(f.store.fail_payout_job(fallback)).await.unwrap();
    assert!(bounded(f.store.queue_invoice_fallback(
        Uuid::now_v7(),
        f.entry_id,
        "replacement".into()
    ))
    .await
    .is_err());
    bounded(f.store.mark_payout_failed(
        payout,
        OffsetDateTime::now_utc(),
        PayoutError::FailedToPayOut("conclusive failure".into()),
    ))
    .await
    .unwrap();
    let replacement = Uuid::now_v7();
    bounded(
        f.store
            .queue_invoice_fallback(replacement, f.entry_id, "replacement".into()),
    )
    .await
    .unwrap();
    let jobs = bounded(f.store.due_payout_jobs()).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, replacement);
    f.assert_no_secrets().await;
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn entry_authorization_is_immutable_but_unpaid_ticket_recycling_can_rotate_it() {
    let f = Fixture::new().await;
    bounded(
        f.store
            .store_entry_payout_policy(f.entry_id, "alice@example.org".into()),
    )
    .await
    .unwrap();
    bounded(
        f.store
            .store_entry_payout_policy(f.entry_id, "alice@example.org".into()),
    )
    .await
    .unwrap();
    assert!(bounded(
        f.store
            .store_entry_payout_policy(f.entry_id, "mallory@example.org".into())
    )
    .await
    .is_err());
    assert_eq!(
        bounded(f.store.entry_payout_policy(f.entry_id))
            .await
            .unwrap()
            .as_deref(),
        Some("alice@example.org")
    );
    bounded(f.store.store_ticket_payout_policy(
        f.ticket_id,
        "old-hash".into(),
        "alice-key".into(),
        "alice-policy".into(),
    ))
    .await
    .unwrap();
    assert!(bounded(f.store.store_ticket_payout_policy(
        f.ticket_id,
        "old-hash".into(),
        "mallory-key".into(),
        "mallory-policy".into()
    ))
    .await
    .is_err());
    bounded(f.store.store_ticket_payout_policy(
        f.ticket_id,
        "new-hash".into(),
        "bob-key".into(),
        "bob-policy".into(),
    ))
    .await
    .unwrap();
    assert!(
        bounded(f.store.ticket_payout_policy(f.ticket_id, "old-hash"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        bounded(f.store.ticket_payout_policy(f.ticket_id, "new-hash"))
            .await
            .unwrap()
            .as_deref(),
        Some("bob-policy")
    );
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn closing_payout_window_stops_new_claims_and_sends_but_keeps_paid_release_recoverable() {
    let f = Fixture::new().await;
    let (unprepared_entry, _) = f.add_entry(3).await;
    let (new_entry, _) = f.add_entry(5).await;
    bounded(f.store.enable_automatic_payouts(f.event_id))
        .await
        .unwrap();
    let paid_job = bounded(f.store.create_payout_job(f.entry_id, "automatic".into()))
        .await
        .unwrap();
    let unprepared_job = bounded(
        f.store
            .create_payout_job(unprepared_entry, "automatic".into()),
    )
    .await
    .unwrap();
    let payout = f.prepare(paid_job, [7; 32]).await;
    assert!(bounded(f.store.payout_send_allowed(payout)).await.unwrap());
    bounded(f.store.close_payout_window(f.event_id))
        .await
        .unwrap();
    assert!(bounded(f.store.payout_window_is_closed(f.event_id))
        .await
        .unwrap());
    assert!(!bounded(f.store.payout_send_allowed(payout)).await.unwrap());
    assert!(
        bounded(f.store.create_payout_job(new_entry, "automatic".into()))
            .await
            .is_err()
    );
    assert!(bounded(f.store.queue_invoice_fallback(
        Uuid::now_v7(),
        new_entry,
        "signed-invoice".into()
    ))
    .await
    .is_err());
    assert!(bounded(f.store.store_prepared_payout(
        unprepared_job,
        invoice([8; 32]),
        10,
        prepared([8; 32])
    ))
    .await
    .is_err());
    // An already dispatched payment can settle after the window closes.
    bounded(f.store.mark_payout_succeeded(
        payout,
        OffsetDateTime::now_utc(),
        Some(hex::encode([7; 32])),
    ))
    .await
    .unwrap();
    assert!(bounded(f.store.has_unsettled_payout_jobs(f.event_id))
        .await
        .unwrap());
    f.assert_no_secrets().await;
    bounded(
        f.store
            .complete_payout_job(paid_job, hex::encode([1; 32]), hex::encode([2; 32])),
    )
    .await
    .unwrap();
    assert!(!bounded(f.store.has_unsettled_payout_jobs(f.event_id))
        .await
        .unwrap());
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn cancelled_competitions_keep_cleanup_work_until_each_invoice_and_escrow_is_done() {
    let f = Fixture::new().await;
    let (_, settled_ticket) = f.add_entry(3).await;
    let (_, escrow_ticket) = f.add_entry(5).await;
    let event = f.event_id.to_string();
    let held = f.ticket_id.to_string();
    bounded(f.database.execute_write(move |pool| async move {
        sqlx::query("UPDATE competitions SET failed_at = datetime('now', '-8 days'), cancelled_at = datetime('now', '-7 days') WHERE id = ?")
            .bind(event).execute(&pool).await?;
        sqlx::query("UPDATE tickets SET paid_at = datetime('now') WHERE id = ?")
            .bind(held).execute(&pool).await?;
        sqlx::query("UPDATE tickets SET paid_at = datetime('now'), settled_at = datetime('now') WHERE id = ?")
            .bind(settled_ticket.to_string()).execute(&pool).await?;
        sqlx::query("UPDATE tickets SET escrow_transaction = 'durably-stored-transaction' WHERE id = ?")
            .bind(escrow_ticket.to_string()).execute(&pool).await?;
        Ok(())
    })).await.unwrap();
    assert!(bounded(f.store.get_competitions(true))
        .await
        .unwrap()
        .is_empty());
    for _retry_after_outage in 0..2 {
        assert_eq!(
            bounded(f.store.get_competitions_pending_cleanup(false))
                .await
                .unwrap(),
            vec![f.event_id]
        );
        let pending = bounded(f.store.get_held_tickets_for_competition(f.event_id))
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, f.ticket_id);
    }
    assert!(bounded(f.store.mark_ticket_invoice_cancelled(f.ticket_id))
        .await
        .unwrap());
    assert!(bounded(f.store.get_competitions_pending_cleanup(false))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        bounded(f.store.get_competitions_pending_cleanup(true))
            .await
            .unwrap(),
        vec![f.event_id]
    );
    let escrows = bounded(f.store.get_escrowed_tickets_for_competition(f.event_id))
        .await
        .unwrap();
    assert_eq!(escrows.len(), 1);
    assert_eq!(escrows[0].id, escrow_ticket);
    assert!(bounded(f.store.mark_ticket_escrow_reclaimed(escrow_ticket))
        .await
        .unwrap());
    assert!(bounded(f.store.get_competitions_pending_cleanup(true))
        .await
        .unwrap()
        .is_empty());
    bounded(f.database.close()).await.unwrap();
}

#[tokio::test]
async fn escrow_reservation_key_update_rejects_a_recycled_ticket() {
    let f = Fixture::new().await;
    let id = f.ticket_id.to_string();
    bounded(f.database.execute_write(move |pool| async move {
        sqlx::query(
            "UPDATE tickets SET reserved_by = 'owner', reserved_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(&pool)
        .await?;
        Ok(())
    }))
    .await
    .unwrap();
    let old = bounded(f.store.get_ticket(f.ticket_id)).await.unwrap();
    assert!(bounded(
        f.store
            .update_ticket_pubkey(&old, "original-bitcoin-key".into())
    )
    .await
    .unwrap());
    let id = f.ticket_id.to_string();
    bounded(f.database.execute_write(move |pool| async move {
        sqlx::query("UPDATE tickets SET hash = 'new-reservation-hash', ephemeral_pubkey = 'replacement-key' WHERE id = ?")
            .bind(id).execute(&pool).await?;
        Ok(())
    })).await.unwrap();
    assert!(
        !bounded(f.store.update_ticket_pubkey(&old, "stale-key".into()))
            .await
            .unwrap()
    );
    let current = bounded(f.store.get_ticket(f.ticket_id)).await.unwrap();
    assert_eq!(current.ephemeral_pubkey.as_deref(), Some("replacement-key"));
    assert!(current.escrow_transaction.is_none());
    bounded(f.database.close()).await.unwrap();
}
