//! Admission boundaries use a real migrated database, including its serialized writer.
//! No invoice, payment, or external service is involved.
use super::*;
use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
use axum::{http::StatusCode, response::IntoResponse};
use futures::poll;
use std::{future::Future, time::Duration as StdDuration};
use tempfile::TempDir;
use tokio::sync::oneshot;

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(StdDuration::from_secs(15), future)
        .await
        .expect("admission test timed out")
}

fn competition(start: OffsetDateTime, capacity: usize) -> Competition {
    Competition::new(&CreateEvent {
        id: Uuid::now_v7(),
        signing_date: start + Duration::hours(2),
        start_observation_date: start,
        end_observation_date: start + Duration::hours(1),
        locations: vec!["KDEN".into()],
        number_of_values_per_entry: 3,
        number_of_places_win: 1,
        total_allowed_entries: capacity,
        entry_fee: 1_000,
        coordinator_fee: crate::domain::CoordinatorFee::whole_percent(10),
        total_competition_pool: capacity * 1_000,
        relative_locktime_block_delta: None,
        unlisted: true,
        scoring_rules: None,
        scoring_fields: None,
        max_entries_per_player: 1,
        contract_options: None,
    })
}

fn entry(event_id: Uuid, ticket_id: Uuid) -> UserEntry {
    let id = Uuid::now_v7();
    AddEntry {
        id,
        ticket_id,
        event_id,
        ephemeral_pubkey: format!("key-{id}"),
        payout_hash: format!("hash-{id}"),
        expected_observations: vec![],
        encrypted_keymeld_private_key: None,
        keymeld_auth_pubkey: None,
        keymeld_registration_context: None,
        keymeld_escrow_policy: None,
    }
    .into_user_entry("alice".into())
}

async fn fixture(
    start: OffsetDateTime,
    capacity: usize,
) -> (TempDir, DBConnection, CompetitionStore, Competition, Uuid) {
    let directory = tempfile::tempdir().unwrap();
    let db = bounded(DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    ))
    .await
    .unwrap();
    let competition = competition(start, capacity);
    let id = competition.id;
    let event = serde_json::to_vec(&competition.event_submission).unwrap();
    let ticket_id = Uuid::now_v7();
    let created_at = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    bounded(db.execute_write(move |pool| async move {
        sqlx::query("INSERT INTO competitions(id, created_at, event_submission) VALUES (?, ?, ?)")
            .bind(id.to_string()).bind(created_at).bind(event).execute(&pool).await?;
        sqlx::query("INSERT INTO tickets(id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'preimage', 'hash')")
            .bind(ticket_id.to_string()).bind(id.to_string()).execute(&pool).await?;
        Ok(())
    })).await.unwrap();
    let store = CompetitionStore::new(db.clone());
    (directory, db, store, competition, ticket_id)
}

#[test]
fn admission_deadlines_are_exclusive_even_while_state_is_created() {
    let start = OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap();
    let competition = competition(start, 3);
    let ticket_cutoff = start - Duration::minutes(1);
    assert!(competition
        .require_ticket_admission(ticket_cutoff - Duration::nanoseconds(1))
        .is_ok());
    assert!(
        matches!(competition.require_ticket_admission(ticket_cutoff), Err(Error::BadRequest(message)) if message == admission::TICKETS_CLOSED)
    );
    assert!(competition
        .require_entry_admission(start - Duration::nanoseconds(1))
        .is_ok());
    assert!(
        matches!(competition.require_entry_admission(start), Err(Error::BadRequest(message)) if message == admission::ENTRIES_CLOSED)
    );
    assert!(competition.require_ticket_admission(start).is_err());
    assert!(competition
        .require_entry_admission(start + Duration::hours(1))
        .is_err());
}

#[test]
fn observation_start_expires_only_unfilled_created_competitions() {
    let start = OffsetDateTime::now_utc();
    let mut competition = competition(start, 3);
    assert!(competition.event_announcement.is_none());
    assert!(!competition.is_expired_at(start - Duration::nanoseconds(1)));
    assert!(competition.is_expired_at(start));
    competition.total_entries = 3;
    competition.total_paid_entries = 3;
    assert!(matches!(
        competition.get_state(),
        CompetitionState::EntriesCollected
    ));
    assert!(!competition.is_expired_at(start));
    assert!(competition
        .require_entry_admission(start - Duration::minutes(2))
        .is_err());
    // A later state must not be cancelled from an incomplete legacy count either.
    competition.total_entries = 1;
    competition.entries_submitted_at = Some(start);
    assert!(!competition.is_expired_at(start));
    competition.keymeld_keygen_completed_at = Some(start);
    assert!(!competition.is_expired_at(start));
}

#[tokio::test]
async fn a_players_side_by_side_reservations_share_one_ticket() {
    let (_dir, db, store, competition, ticket_id) =
        fixture(OffsetDateTime::now_utc() + Duration::hours(1), 3).await;
    let other = Uuid::now_v7();
    let event_id = competition.id;
    bounded(db.execute_write(move |pool| async move {
        sqlx::query("INSERT INTO tickets(id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'other', 'other')")
            .bind(other.to_string()).bind(event_id.to_string()).execute(&pool).await?;
        Ok(())
    }))
    .await
    .unwrap();
    // A retry sent while the first request is still reserving: one ticket, with nothing rotated.
    let deadline = competition.ticket_deadline();
    let (first, retry) = bounded(async {
        tokio::join!(
            store.get_and_reserve_ticket_before(competition.id, "alice", deadline, 1),
            store.get_and_reserve_ticket_before(competition.id, "alice", deadline, 1),
        )
    })
    .await;
    let reserved = |reservation| match reservation {
        Ok(super::store::TicketReservation::Reserved(reserved)) => *reserved,
        _ => panic!("no ticket reserved"),
    };
    let (first, retry) = (reserved(first), reserved(retry));
    assert_eq!(retry.ticket.id, first.ticket.id);
    assert_eq!(retry.ticket.hash, first.ticket.hash);
    assert!(first.superseded_payment_hash.is_none() && retry.superseded_payment_hash.is_none());
    let untouched = if first.ticket.id == ticket_id {
        other
    } else {
        ticket_id
    };
    assert!(bounded(store.get_ticket(untouched))
        .await
        .unwrap()
        .reserved_by
        .is_none());
    bounded(db.close()).await.unwrap();
}

#[tokio::test]
async fn closed_ticket_request_does_not_reserve_or_rotate_a_ticket() {
    let (_dir, db, store, competition, ticket_id) = fixture(OffsetDateTime::now_utc(), 3).await;
    let denied = bounded(store.get_and_reserve_ticket_before(
        competition.id,
        "alice",
        competition.ticket_deadline(),
        1,
    ))
    .await
    .unwrap();
    assert!(matches!(denied, super::store::TicketReservation::Closed));
    let ticket = bounded(store.get_ticket(ticket_id)).await.unwrap();
    assert!(ticket.reserved_at.is_none());
    assert!(ticket.reserved_by.is_none());
    assert_eq!(ticket.hash, "hash");
    bounded(db.close()).await.unwrap();
}

#[tokio::test]
async fn queued_reservation_registration_and_entry_cannot_cross_the_deadline() {
    let (_dir, db, store, competition, ticket_id) =
        fixture(OffsetDateTime::now_utc() + Duration::hours(1), 3).await;
    let reserved = bounded(store.get_and_reserve_ticket(competition.id, "alice"))
        .await
        .unwrap()
        .ticket;
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut blocker = Box::pin(db.execute_write(move |_pool| async move {
        started.send(()).unwrap();
        released.await.unwrap();
        Ok(())
    }));
    assert!(poll!(blocker.as_mut()).is_pending());
    bounded(start).await.unwrap();
    let deadline = OffsetDateTime::now_utc() + Duration::milliseconds(100);
    let mut reserve =
        Box::pin(store.get_and_reserve_ticket_before(competition.id, "alice", deadline, 1));
    let mut register = Box::pin(store.store_ticket_registration_before(
        ticket_id,
        reserved.hash,
        "alice".into(),
        "registration".into(),
        deadline,
    ));
    let mut enter = Box::pin(store.add_entry_with_policy_before(
        entry(competition.id, ticket_id),
        ticket_id,
        Some("policy".into()),
        deadline,
        None,
        1,
    ));
    assert!(poll!(reserve.as_mut()).is_pending());
    assert!(poll!(register.as_mut()).is_pending());
    assert!(poll!(enter.as_mut()).is_pending());
    tokio::time::sleep(StdDuration::from_millis(110)).await;
    assert!(OffsetDateTime::now_utc() >= deadline);
    release.send(()).unwrap();
    bounded(blocker).await.unwrap();
    assert!(bounded(reserve).await.unwrap().reserved().is_none());
    assert!(bounded(register).await.unwrap().is_none());
    assert!(bounded(enter).await.unwrap().added().is_none());
    for table in [
        "entries",
        "entry_payout_policies",
        "ticket_keymeld_registrations",
    ] {
        let count: i64 = bounded(
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(db.read()),
        )
        .await
        .unwrap();
        assert_eq!(count, 0, "late write changed {table}");
    }
    let current = bounded(store.get_ticket(ticket_id)).await.unwrap();
    assert_eq!(current.reserved_at, reserved.reserved_at);
    assert_eq!(current.reserved_by.as_deref(), Some("alice"));
    bounded(db.close()).await.unwrap();
}

#[tokio::test]
async fn admission_before_close_preserves_registration_policy_and_one_entry_per_ticket() {
    let (_dir, db, store, competition, ticket_id) =
        fixture(OffsetDateTime::now_utc() + Duration::hours(1), 3).await;
    let deadline = competition.event_submission.start_observation_date;
    let ticket = bounded(store.get_and_reserve_ticket_before(
        competition.id,
        "alice",
        competition.ticket_deadline(),
        1,
    ))
    .await
    .unwrap()
    .reserved()
    .unwrap()
    .ticket;
    assert_eq!(
        bounded(store.store_ticket_registration_before(
            ticket_id,
            ticket.hash.clone(),
            "alice".into(),
            "registration".into(),
            deadline
        ))
        .await
        .unwrap(),
        Some(RegistrationStored::Stored)
    );
    let first = entry(competition.id, ticket_id);
    let id = first.id;
    let second = entry(competition.id, ticket_id);
    let (first, second) = bounded(async {
        tokio::join!(
            store.add_entry_with_policy_before(
                first,
                ticket_id,
                Some("policy".into()),
                deadline,
                None,
                1
            ),
            store.add_entry_with_policy_before(
                second,
                ticket_id,
                Some("policy".into()),
                deadline,
                None,
                1
            ),
        )
    })
    .await;
    assert!(first.is_ok());
    assert!(first.unwrap().added().is_some());
    let duplicate_error = second.unwrap_err();
    assert!(matches!(&duplicate_error,
        crate::infra::db::DatabaseWriteError::Sqlx(sqlx::Error::Database(error)) if error.is_unique_violation()));
    let ticket_has_entry = bounded(store.get_ticket(ticket_id))
        .await
        .unwrap()
        .entry_id
        .is_some();
    let rejection = admission::entry_write_error(duplicate_error, ticket_has_entry);
    assert!(
        matches!(&rejection, Error::BadRequest(message) if message == "Ticket has already been used")
    );
    let response = rejection.into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": "Ticket has already been used"})
    );
    let stored_policy: String = bounded(
        sqlx::query_scalar("SELECT policy_json FROM entry_payout_policies WHERE entry_id = ?")
            .bind(id.to_string())
            .fetch_one(db.read()),
    )
    .await
    .unwrap();
    assert_eq!(stored_policy, "policy");
    assert_eq!(
        bounded(store.ticket_registration(ticket_id, &ticket.hash))
            .await
            .unwrap()
            .as_deref(),
        Some("registration")
    );
    let count: i64 =
        bounded(sqlx::query_scalar("SELECT count(*) FROM entries").fetch_one(db.read()))
            .await
            .unwrap();
    assert_eq!(count, 1);
    bounded(db.close()).await.unwrap();
}

#[tokio::test]
async fn stale_unfilled_snapshot_cannot_cancel_the_last_entry_committed_before_close() {
    let (_dir, db, store, snapshot, ticket_id) =
        fixture(OffsetDateTime::now_utc() + Duration::minutes(1), 1).await;
    let lease = bounded(store.acquire_lease(
        &Lease::competition_resource(snapshot.id),
        "worker",
        StdDuration::from_secs(30),
    ))
    .await
    .unwrap()
    .unwrap();
    // Set a short real admission window after the DB and lease are ready.
    let mut snapshot = snapshot;
    let deadline = OffsetDateTime::now_utc() + Duration::seconds(2);
    snapshot.event_submission.start_observation_date = deadline;
    let event = serde_json::to_vec(&snapshot.event_submission).unwrap();
    let id = snapshot.id.to_string();
    bounded(db.execute_write(move |pool| async move {
        sqlx::query("UPDATE competitions SET event_submission = ? WHERE id = ?")
            .bind(event)
            .bind(id)
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();
    // Snapshot is still empty when the final valid entry commits.
    assert_eq!(snapshot.total_entries, 0);
    assert!(bounded(store.add_entry_with_policy_before(
        entry(snapshot.id, ticket_id),
        ticket_id,
        None,
        snapshot.event_submission.start_observation_date,
        None,
        1
    ))
    .await
    .unwrap()
    .added()
    .is_some());
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut blocker = Box::pin(db.execute_write(move |_pool| async move {
        started.send(()).unwrap();
        released.await.unwrap();
        Ok(())
    }));
    assert!(poll!(blocker.as_mut()).is_pending());
    bounded(start).await.unwrap();
    let mut cancel = Box::pin(store.cancel_unfilled_at_deadline(&snapshot, &lease));
    assert!(poll!(cancel.as_mut()).is_pending());
    // The cancellation reaches the writer after close, while the worker still
    // believes the roster is empty. Its SQL must inspect the committed entry.
    let until_close = (deadline - OffsetDateTime::now_utc()).max(Duration::ZERO);
    tokio::time::sleep(until_close.unsigned_abs() + StdDuration::from_millis(10)).await;
    assert!(snapshot.unfilled_admission_expired(OffsetDateTime::now_utc()));
    release.send(()).unwrap();
    bounded(blocker).await.unwrap();
    assert!(!bounded(cancel).await.unwrap());
    let current = bounded(store.get_competition(snapshot.id)).await.unwrap();
    assert!(current.has_full_entries());
    assert!(current.cancelled_at.is_none());
    bounded(db.close()).await.unwrap();
}

#[tokio::test]
async fn unfilled_cancellation_is_due_lease_fenced_and_does_not_touch_advanced_states() {
    let (_dir, db, store, mut competition, _ticket) =
        fixture(OffsetDateTime::now_utc() + Duration::hours(1), 3).await;
    let lease = bounded(store.acquire_lease(
        &Lease::competition_resource(competition.id),
        "worker",
        StdDuration::from_secs(30),
    ))
    .await
    .unwrap()
    .unwrap();
    assert!(
        !bounded(store.cancel_unfilled_at_deadline(&competition, &lease))
            .await
            .unwrap()
    );
    competition.event_submission.start_observation_date =
        OffsetDateTime::now_utc() - Duration::seconds(1);
    let invalid_lease = Lease {
        token: lease.token + 1,
        ..lease.clone()
    };
    assert!(
        !bounded(store.cancel_unfilled_at_deadline(&competition, &invalid_lease))
            .await
            .unwrap()
    );
    let id = competition.id.to_string();
    bounded(db.execute_write(move |pool| async move {
        sqlx::query("UPDATE competitions SET entries_submitted_at = datetime('now') WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();
    assert!(
        !bounded(store.cancel_unfilled_at_deadline(&competition, &lease))
            .await
            .unwrap()
    );
    let id = competition.id.to_string();
    bounded(db.execute_write(move |pool| async move {
        sqlx::query("UPDATE competitions SET entries_submitted_at = NULL WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await?;
        Ok(())
    }))
    .await
    .unwrap();
    assert!(
        bounded(store.cancel_unfilled_at_deadline(&competition, &lease))
            .await
            .unwrap()
    );
    assert!(bounded(store.get_competition(competition.id))
        .await
        .unwrap()
        .cancelled_at
        .is_some());
    assert!(
        !bounded(store.cancel_unfilled_at_deadline(&competition, &lease))
            .await
            .unwrap()
    );
    bounded(db.close()).await.unwrap();
}

#[test]
fn unrelated_entry_write_failures_remain_server_errors() {
    use crate::infra::db::DatabaseWriteError;
    let error =
        admission::entry_write_error(DatabaseWriteError::Sqlx(sqlx::Error::PoolClosed), true);
    assert!(matches!(error, Error::DatabaseWrite(_)));
    assert_eq!(
        error.into_response().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

/// One entry per player unless the competition says otherwise: a player whose paid tickets all
/// have entries gets no new ticket, and a second paid ticket makes no second entry either.
#[tokio::test]
async fn a_player_enters_only_as_often_as_the_competition_allows() {
    use super::store::{EntryAdmission, TicketReservation};
    let (_dir, db, store, competition, _) =
        fixture(OffsetDateTime::now_utc() + Duration::hours(1), 3).await;
    let id = competition.id;
    let second_ticket = Uuid::now_v7();
    bounded(db.execute_write(move |pool| async move {
        sqlx::query("INSERT INTO tickets(id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'preimage', 'hash2')")
            .bind(second_ticket.to_string()).bind(id.to_string()).execute(&pool).await?;
        Ok(())
    }))
    .await
    .unwrap();
    let tickets = competition.ticket_deadline();
    let entries = competition.event_submission.start_observation_date;
    let reserve = |max| {
        let store = store.clone();
        async move {
            bounded(store.get_and_reserve_ticket_before(id, "alice", tickets, max))
                .await
                .unwrap()
        }
    };
    let enter = |ticket: Uuid, max| {
        let store = store.clone();
        async move {
            bounded(store.add_entry_with_policy_before(
                entry(id, ticket),
                ticket,
                None,
                entries,
                None,
                max,
            ))
            .await
        }
    };

    let first = reserve(1).await.reserved().unwrap().ticket;
    // Unpaid, or paid without an entry yet, the ticket is still the player's to use.
    assert_eq!(reserve(1).await.reserved().unwrap().ticket.id, first.id);
    assert!(bounded(store.mark_ticket_paid(&first.hash, id))
        .await
        .unwrap());
    assert_eq!(reserve(1).await.reserved().unwrap().ticket.id, first.id);
    assert!(matches!(
        enter(first.id, 1).await.unwrap(),
        EntryAdmission::Added(_)
    ));

    // Sending the first entry again still reads as a retry of that ticket, not as the limit.
    let retry = enter(first.id, 1).await.unwrap_err();
    assert!(matches!(&retry,
        crate::infra::db::DatabaseWriteError::Sqlx(sqlx::Error::Database(error)) if error.is_unique_violation()));

    // Entered once: no second ticket, unless the competition allows two.
    assert!(matches!(reserve(1).await, TicketReservation::EntryLimit));
    let second = reserve(2).await.reserved().unwrap().ticket;
    assert_eq!(second.id, second_ticket);

    // A second ticket paid anyway (two paid at once, say) makes no second entry under a limit
    // of one; cleanup refunds a paid ticket without an entry.
    assert!(bounded(store.mark_ticket_paid(&second.hash, id))
        .await
        .unwrap());
    assert!(matches!(
        enter(second.id, 1).await.unwrap(),
        EntryAdmission::EntryLimit
    ));
    assert!(matches!(
        enter(second.id, 2).await.unwrap(),
        EntryAdmission::Added(_)
    ));

    bounded(db.close()).await.unwrap();
}

#[test]
fn the_entry_limit_says_so_in_plain_words() {
    let one = super::coordinator::entry_limit_error(1);
    assert!(
        matches!(one, Error::BadRequest(message) if message == "You've already entered this competition")
    );
    let two = super::coordinator::entry_limit_error(2);
    assert!(matches!(two, Error::BadRequest(message) if message.contains("the 2 entries")));
}

/// The contract terms a player accepts before paying for a seat of single competition
/// `competition_id`, for the entry `entry_id`, as its payout authorization carries them: one seat
/// of three.
pub(super) fn contract_terms(competition_id: Uuid, entry_id: Uuid) -> String {
    let point = |byte: u8| {
        dlctix::secp::Scalar::from_slice(&[byte; 32])
            .unwrap()
            .base_point_mul()
    };
    serde_json::to_string(&coordinator_escrow::payout::ContractAuthorization {
        competition_id,
        entry_id,
        network: dlctix::bitcoin::Network::Regtest,
        player_index: 0,
        player_count: 3,
        ticket_hash: [1; 32],
        payout_hash: [2; 32],
        market_maker: dlctix::MarketMaker { pubkey: point(9) },
        event: dlctix::EventLockingConditions {
            locking_points: vec![point(10).into()],
            expiry: None,
        },
        outcome_payouts: std::collections::BTreeMap::from([(
            dlctix::Outcome::Attestation(0),
            dlctix::PayoutWeights::from([(0, 1)]),
        )]),
        funding_value: dlctix::bitcoin::Amount::from_sat(3_000),
        relative_locktime_block_delta: 72,
        max_fee_rate: dlctix::bitcoin::FeeRate::from_sat_per_vb_u32(1),
    })
    .unwrap()
}

/// A paid ticket whose entry was not made within the hour its entry id allows has lapsed. In a
/// single competition it keeps its seat, as every seat's ticket is named in the competition's
/// payout terms and Keymeld session, so it still counts as its player's entry. It is no longer
/// handed back for them to enter, and a player who may make one entry gets no new ticket, which
/// would only pay for a seat in a competition that can no longer fill: they are told why. One who
/// may make two can still take a second seat.
#[tokio::test]
async fn a_lapsed_paid_ticket_still_counts_as_its_players_entry_in_a_single_competition() {
    use super::store::TicketReservation;
    let (_dir, db, store, competition, _) =
        fixture(OffsetDateTime::now_utc() + Duration::hours(1), 3).await;
    let id = competition.id;
    let more = [(Uuid::now_v7(), "hash2"), (Uuid::now_v7(), "hash3")];
    bounded(db.execute_write(move |pool| async move {
        for (ticket, hash) in more {
            sqlx::query("INSERT INTO tickets(id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'preimage', ?)")
                .bind(ticket.to_string()).bind(id.to_string()).bind(hash).execute(&pool).await?;
        }
        Ok(())
    }))
    .await
    .unwrap();
    let deadline = competition.ticket_deadline();
    let reserve = |player: &'static str, max: u32| {
        let store = store.clone();
        async move { bounded(store.get_and_reserve_ticket_before(id, player, deadline, max)).await }
    };

    // Alice pays for a seat, having accepted the payout terms for the entry she started.
    let paid = reserve("alice", 1)
        .await
        .unwrap()
        .reserved()
        .unwrap()
        .ticket;
    assert!(bounded(store.mark_ticket_paid(&paid.hash, id))
        .await
        .unwrap());
    let started = |minutes_ago: i64| {
        let started = OffsetDateTime::now_utc() - Duration::minutes(minutes_ago);
        let policy = serde_json::to_string(&coordinator_escrow::authorization::PayoutPolicy {
            queued_entry: None,
            automatic_lightning_address: Some("alice@example.org".into()),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            contract_terms: contract_terms(id, super::queued_tests::entry_id_at(started)),
            ark_escrow: None,
        })
        .unwrap();
        let (ticket, hash, db) = (paid.id.to_string(), paid.hash.clone(), db.clone());
        async move {
            bounded(db.execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO ticket_payout_policies(ticket_id, ticket_hash, entry_pubkey, policy_json)
                     VALUES (?, ?, 'key', ?)
                     ON CONFLICT(ticket_id) DO UPDATE SET policy_json = excluded.policy_json",
                )
                .bind(ticket)
                .bind(hash)
                .bind(policy)
                .execute(&pool)
                .await?;
                Ok(())
            }))
            .await
            .unwrap();
        }
    };
    started(55).await;
    // Within the hour the paid ticket is hers to enter, and her one entry.
    assert_eq!(
        reserve("alice", 1)
            .await
            .unwrap()
            .reserved()
            .unwrap()
            .ticket
            .id,
        paid.id
    );

    // The hour passes without the entry: the ticket is not handed back, and is still her entry.
    started(61).await;
    assert!(matches!(
        reserve("alice", 1).await.unwrap(),
        TicketReservation::Lapsed
    ));
    let held: i64 = bounded(
        sqlx::query_scalar("SELECT count(*) FROM tickets WHERE reserved_by = 'alice'")
            .fetch_one(db.read()),
    )
    .await
    .unwrap();
    assert_eq!(held, 1, "no other seat was reserved for her");

    // Where one player may make two entries, the lapsed ticket is one of them.
    let again = reserve("alice", 2)
        .await
        .unwrap()
        .reserved()
        .unwrap()
        .ticket;
    assert_ne!(again.id, paid.id, "a second seat, not the lapsed ticket");
    // The lapsed ticket keeps its seat: with bob's, every seat is taken.
    assert!(reserve("bob", 1).await.unwrap().reserved().is_some());
    assert!(matches!(
        reserve("carol", 1).await,
        Err(crate::infra::db::DatabaseWriteError::Sqlx(
            sqlx::Error::RowNotFound
        ))
    ));
    // Paid and entered, the second seat is her other entry.
    assert!(bounded(store.mark_ticket_paid(&again.hash, id))
        .await
        .unwrap());
    assert!(bounded(store.add_entry_with_policy_before(
        entry(id, again.id),
        again.id,
        None,
        competition.event_submission.start_observation_date,
        None,
        2,
    ))
    .await
    .unwrap()
    .added()
    .is_some());
    assert!(matches!(
        reserve("alice", 2).await.unwrap(),
        TicketReservation::Lapsed
    ));
    bounded(db.close()).await.unwrap();
}

/// An entry checked within the hour its id allows can reach the writer after it, behind other
/// writes. By then its ticket has lapsed and stopped counting as its player's entry, so the write
/// refuses it: a ticket is never both refunded and entered.
#[tokio::test]
async fn an_entry_that_reaches_the_writer_after_its_hour_is_refused() {
    let (_dir, db, store, competition, ticket_id) =
        fixture(OffsetDateTime::now_utc() + Duration::hours(1), 3).await;
    let deadline = competition.event_submission.start_observation_date;
    let (started, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut blocker = Box::pin(db.execute_write(move |_pool| async move {
        started.send(()).unwrap();
        released.await.unwrap();
        Ok(())
    }));
    assert!(poll!(blocker.as_mut()).is_pending());
    bounded(start).await.unwrap();
    let finish_by = OffsetDateTime::now_utc() + Duration::milliseconds(100);
    let mut enter = Box::pin(store.add_entry_with_policy_before(
        entry(competition.id, ticket_id),
        ticket_id,
        Some("policy".into()),
        deadline,
        Some(finish_by),
        1,
    ));
    assert!(poll!(enter.as_mut()).is_pending());
    tokio::time::sleep(StdDuration::from_millis(110)).await;
    assert!(OffsetDateTime::now_utc() > finish_by);
    release.send(()).unwrap();
    bounded(blocker).await.unwrap();
    assert!(matches!(
        bounded(enter).await.unwrap(),
        super::store::EntryAdmission::Lapsed
    ));
    for table in ["entries", "entry_payout_policies"] {
        let count: i64 = bounded(
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(db.read()),
        )
        .await
        .unwrap();
        assert_eq!(count, 0, "a lapsed entry changed {table}");
    }

    // Within its hour the entry goes in.
    let finish_by = OffsetDateTime::now_utc() + Duration::minutes(1);
    assert!(bounded(store.add_entry_with_policy_before(
        entry(competition.id, ticket_id),
        ticket_id,
        Some("policy".into()),
        deadline,
        Some(finish_by),
        1,
    ))
    .await
    .unwrap()
    .added()
    .is_some());
    bounded(db.close()).await.unwrap();
}
