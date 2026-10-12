//! The queue details every list attaches, read for all queues at once, against the reads a list
//! made for each queue before, on a migrated database shaped like a busy one: dozens of queues,
//! most of them formed into pools, with paid, lapsed, reserved, invoiced and expired tickets
//! left on them, and single competitions beside them.

use super::queued::CompetitionKind;
use super::queued_tests::{entry_id_at, Queue};
use super::*;
use coordinator_escrow::pools::PoolRules;
use std::collections::HashMap;
use std::time::Instant;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use uuid::Uuid;

/// Queues, as many as production held when the operator inventory was over its budget; the
/// first `FORMED` formed their pools, one or two each.
const QUEUES: usize = 85;
const FORMED: usize = 66;
/// Paid entries in each pool, and in each queue still open.
const PLAYERS: usize = 12;
/// Single competitions, whose entries count for no queue.
const SINGLES: usize = 20;

/// A ticket left on a queue without an entry.
#[derive(Clone, Copy)]
enum Left {
    /// Paid within its entry's hour: it holds a place.
    Paid,
    /// Paid, but its entry's hour has passed: it is refunded and holds none.
    Lapsed,
    /// Reserved a moment ago, before its invoice: it holds a place.
    Reserved,
    /// Its invoice can still be paid: it holds a place.
    Invoiced,
    /// Its invoice expired unpaid: it holds none.
    Expired,
}

/// A paid ticket of `event` and its entry, under the ticket's id; returns the id.
async fn paid_entry(
    connection: &mut sqlx::SqliteConnection,
    event: &str,
) -> Result<String, sqlx::Error> {
    let ticket = Uuid::now_v7();
    left_ticket(&mut *connection, event, ticket, Left::Paid).await?;
    sqlx::query(
        "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey, payout_hash,
            entry_submission)
         VALUES (?1, ?2, ?1, 'player', 'key-' || ?1, 'payout-' || ?1, '{}')",
    )
    .bind(ticket.to_string())
    .bind(event)
    .execute(&mut *connection)
    .await?;
    Ok(ticket.to_string())
}

async fn left_ticket(
    connection: &mut sqlx::SqliteConnection,
    event: &str,
    ticket: Uuid,
    left: Left,
) -> Result<(), sqlx::Error> {
    let (reserved, paid, invoice) = match left {
        Left::Paid | Left::Lapsed => ("+0 minutes", true, None),
        Left::Reserved => ("+0 minutes", false, None),
        Left::Invoiced => ("-20 minutes", false, Some("+20 minutes")),
        Left::Expired => ("-20 minutes", false, Some("-5 minutes")),
    };
    sqlx::query(
        "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_by, reserved_at,
            paid_at, settled_at, payment_request, invoice_expires_at)
         VALUES (?1, ?2, 'preimage', ?3, 'player', datetime('now', ?4),
            CASE WHEN ?5 THEN datetime('now') END, CASE WHEN ?5 THEN datetime('now') END,
            CASE WHEN ?6 IS NOT NULL THEN 'lnbcrt1invoice' END,
            CASE WHEN ?6 IS NOT NULL THEN datetime('now', ?6) END)",
    )
    .bind(ticket.to_string())
    .bind(event)
    .bind(format!("{:064x}", ticket.as_u128()))
    .bind(reserved)
    .bind(paid)
    .bind(invoice)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// [`QUEUES`] queues, the first of them `queue.competition`, with their pools and tickets.
async fn busy_queues() -> (Queue, Vec<Uuid>) {
    let now = OffsetDateTime::now_utc();
    let queue = Queue::new(
        now + Duration::hours(3),
        PoolRules::new(3, 20).unwrap(),
        500,
    )
    .await;
    let mut ids = vec![queue.competition.id];
    for _ in 1..QUEUES {
        let mut competition = queue.competition.clone();
        competition.id = Uuid::now_v7();
        competition.event_submission.id = competition.id;
        let mut settings = queue.settings.clone();
        settings.competition_id = competition.id;
        settings.terms.competition_id = competition.id;
        settings.terms_digest = settings.terms.digest().unwrap();
        queue
            .store()
            .add_queued_competition(&competition, &settings)
            .await
            .unwrap();
        ids.push(competition.id);
    }

    let submission = serde_json::to_string(&queue.competition.event_submission).unwrap();
    let created_at = now.format(&Rfc3339).unwrap();
    let queues: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    queue
        .db
        .execute_write(move |pool| async move {
            let mut tx = pool.begin().await?;
            for (index, queue) in queues.iter().enumerate() {
                if index >= FORMED {
                    for _ in 0..PLAYERS {
                        paid_entry(&mut tx, queue).await?;
                    }
                    for left in [
                        Left::Paid,
                        Left::Lapsed,
                        Left::Reserved,
                        Left::Invoiced,
                        Left::Expired,
                    ] {
                        let ticket = match left {
                            Left::Lapsed => entry_id_at(now - Duration::hours(2)),
                            _ => Uuid::now_v7(),
                        };
                        left_ticket(&mut tx, queue, ticket, left).await?;
                    }
                    continue;
                }
                for pool_index in 0..1 + index % 2 {
                    let pool_id = Uuid::now_v7().to_string();
                    sqlx::query(
                        "INSERT INTO competitions (id, created_at, event_submission, kind,
                            parent_id, pool_index)
                         VALUES (?, ?, ?, 'pool', ?, ?)",
                    )
                    .bind(&pool_id)
                    .bind(&created_at)
                    .bind(&submission)
                    .bind(queue)
                    .bind(pool_index as i64)
                    .execute(&mut *tx)
                    .await?;
                    let mut members = Vec::with_capacity(PLAYERS);
                    for _ in 0..PLAYERS {
                        members.push(paid_entry(&mut tx, &pool_id).await?);
                    }
                    members.sort();
                    let members = serde_json::to_string(&members).unwrap();
                    sqlx::query(
                        "INSERT INTO competition_pools (competition_id, parent_id, pool_index,
                            close_height, block_hash, tickets_json, members_json, formed_at)
                         VALUES (?, ?, ?, 100, ?, ?, ?, ?)",
                    )
                    .bind(&pool_id)
                    .bind(queue)
                    .bind(pool_index as i64)
                    .bind("00".repeat(32))
                    .bind(&members)
                    .bind(&members)
                    .bind(now.unix_timestamp())
                    .execute(&mut *tx)
                    .await?;
                }
                sqlx::query("UPDATE competitions SET pools_formed_at = ? WHERE id = ?")
                    .bind(&created_at)
                    .bind(queue)
                    .execute(&mut *tx)
                    .await?;
                // What the kickoff left on the queue: a paid ticket whose entry's hour passed,
                // and an unpaid one whose invoice expired.
                let lapsed = entry_id_at(now - Duration::hours(2));
                left_ticket(&mut tx, queue, lapsed, Left::Lapsed).await?;
                left_ticket(&mut tx, queue, Uuid::now_v7(), Left::Expired).await?;
            }
            for _ in 0..SINGLES {
                let single = Uuid::now_v7().to_string();
                sqlx::query(
                    "INSERT INTO competitions (id, created_at, event_submission) VALUES (?, ?, ?)",
                )
                .bind(&single)
                .bind(&created_at)
                .bind(&submission)
                .execute(&mut *tx)
                .await?;
                for _ in 0..5 {
                    paid_entry(&mut tx, &single).await?;
                }
            }
            tx.commit().await?;
            Ok(())
        })
        .await
        .unwrap();
    (queue, ids)
}

/// The reads a list made before, one queue at a time: its settings, paid entries, held tickets
/// and pools.
async fn read_each(store: &CompetitionStore, ids: &[Uuid]) -> HashMap<Uuid, QueueDetails> {
    let mut details = HashMap::new();
    for id in ids {
        let Some(settings) = store.queue_settings(*id).await.unwrap() else {
            continue;
        };
        details.insert(
            *id,
            QueueDetails {
                settings,
                entries: store.queued_entry_count(*id).await.unwrap(),
                held: store.queued_held_count(*id).await.unwrap(),
                pools: store.competition_pools(*id).await.unwrap(),
            },
        );
    }
    details
}

#[tokio::test]
async fn one_read_gives_every_queue_what_its_own_reads_give() {
    let (queue, ids) = busy_queues().await;
    let store = queue.store();

    let batched = store.queue_details(&ids).await.unwrap();
    assert_eq!(batched.len(), QUEUES);
    assert_eq!(batched, read_each(store, &ids).await);

    // Not vacuous: a queue that formed one pool, one that formed two, and one still open.
    let one_pool = &batched[&ids[0]];
    assert_eq!(one_pool.settings, queue.settings);
    assert_eq!(one_pool.entries, PLAYERS as u64);
    assert_eq!(one_pool.pools.len(), 1);
    assert_eq!(one_pool.pools[0].members.len(), PLAYERS);
    assert_eq!(
        one_pool.held, 0,
        "its pools hold its entries; the lapsed and expired tickets left on it hold nothing"
    );
    let two_pools = &batched[&ids[1]];
    assert_eq!(two_pools.entries, 2 * PLAYERS as u64);
    assert_eq!(
        two_pools
            .pools
            .iter()
            .map(|pool| pool.pool_index)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    let open = &batched[&ids[QUEUES - 1]];
    assert_eq!(open.entries, PLAYERS as u64);
    assert!(open.pools.is_empty());
    assert_eq!(
        open.held,
        PLAYERS as u64 + 3,
        "its entries, and the paid, reserved and invoiced tickets; not the lapsed or expired"
    );

    // A competition that is not queued, or does not exist, is left out; no ids, no read.
    let pool = one_pool.pools[0].competition_id;
    let some = store
        .queue_details(&[ids[0], pool, Uuid::now_v7()])
        .await
        .unwrap();
    assert_eq!(some.keys().collect::<Vec<_>>(), [&ids[0]]);
    assert_eq!(some[&ids[0]], *one_pool);
    assert!(store.queue_details(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_list_attaches_each_queue_as_its_own_read_does() {
    let (queue, ids) = busy_queues().await;
    let listed = queue
        .coordinator
        .list_operator_competitions()
        .await
        .unwrap();
    let pools = FORMED + FORMED / 2;
    assert_eq!(listed.len(), QUEUES + pools + SINGLES);
    for competition in &listed {
        if competition.kind != CompetitionKind::Queued {
            assert!(competition.queue.is_none());
            continue;
        }
        let own = queue
            .coordinator
            .get_competition(competition.id)
            .await
            .unwrap();
        assert!(competition.queue.is_some());
        assert_eq!(competition.queue, own.queue);
    }
    assert_eq!(
        listed
            .iter()
            .filter(|competition| competition.queue.is_some())
            .count(),
        ids.len()
    );
}

/// The before and after of the operator inventory's queue stage: run with `--nocapture` to see
/// the times.
#[tokio::test]
async fn one_read_for_every_queue_is_faster_than_a_read_per_queue() {
    const RUNS: usize = 15;
    let (queue, ids) = busy_queues().await;
    let store = queue.store();
    let (mut summary, mut each, mut batched) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..RUNS {
        let started = Instant::now();
        store.list_operator_competitions().await.unwrap();
        summary.push(started.elapsed());
        let started = Instant::now();
        read_each(store, &ids).await;
        each.push(started.elapsed());
        let started = Instant::now();
        store.queue_details(&ids).await.unwrap();
        batched.push(started.elapsed());
    }
    for times in [&mut summary, &mut each, &mut batched] {
        times.sort();
    }
    fn median(times: &[std::time::Duration]) -> std::time::Duration {
        times[times.len() / 2]
    }
    println!(
        "{QUEUES} queues, median of {RUNS}: summary query {:?}; queue details read per queue \
         (before) {:?}, in one batch (after) {:?}",
        median(&summary),
        median(&each),
        median(&batched),
    );
    assert!(
        median(&batched) < median(&each),
        "one batch {:?} is not faster than a read per queue {:?}",
        median(&batched),
        median(&each),
    );
}
