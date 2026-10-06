//! Admission boundaries for the default competition: one pool, 20 seats, at least 3 entries.

use super::queued_store::QueuedReservation;
use super::queued_tests::Queue;
use super::*;
use coordinator_escrow::pools::PoolRules;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

async fn default_queue(start: OffsetDateTime) -> Queue {
    Queue::paying(start, PoolRules::new(3, 20).unwrap(), 20, 2).await
}

#[tokio::test]
async fn twenty_paid_players_exclude_every_extra_player_even_after_their_holds_expire() {
    let queue = default_queue(OffsetDateTime::now_utc() + Duration::hours(3)).await;
    for index in 0..20 {
        queue.ticket(&format!("paid{index}"), true).await;
    }
    queue
        .db
        .execute_write(|pool| async move {
            sqlx::query("UPDATE tickets SET reserved_at = datetime('now', '-20 minutes')")
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();

    // These are distinct players, so per-player limits cannot mask an incorrect queue cap.
    for index in 20..25 {
        let result = queue
            .store()
            .reserve_queued_ticket(
                queue.competition.id,
                Uuid::now_v7(),
                &format!("extra{index}"),
                20,
                1,
                queue.competition.ticket_deadline(),
            )
            .await
            .unwrap();
        assert!(matches!(result, QueuedReservation::Full));
    }
    let summary = queue
        .coordinator
        .get_competition(queue.competition.id)
        .await
        .unwrap();
    let json = serde_json::to_value(summary).unwrap();
    assert_eq!(json["entries"], 20);
    assert_eq!(json["held"], 20);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tickets")
        .fetch_one(queue.db.read())
        .await
        .unwrap();
    assert_eq!(count, 20, "refusals must not create extra payable tickets");
}

#[tokio::test]
async fn the_default_queue_forms_one_pool_only_from_three_complete_entries() {
    for players in [0, 1, 2, 3, 20] {
        let queue = default_queue(OffsetDateTime::now_utc() - Duration::minutes(10)).await;
        queue.chain_closing_at(20, 30);
        let mut tickets = Vec::new();
        for index in 0..players {
            tickets.push(queue.ticket(&format!("player{index}"), true).await);
        }
        assert_eq!(queue.advance().await, Step::Finished);
        let parent = queue
            .coordinator
            .get_competition(queue.competition.id)
            .await
            .unwrap();
        let pools = queue
            .store()
            .competition_pools(queue.competition.id)
            .await
            .unwrap();
        if players < 3 {
            assert!(parent.is_cancelled(), "{players} entries cannot start");
            assert!(parent.pools_formed_at.is_none());
            assert!(pools.is_empty());
            for ticket in tickets {
                assert_eq!(queue.event_of(ticket).await.0, queue.competition.id);
            }
        } else {
            assert!(!parent.is_cancelled());
            assert!(parent.pools_formed_at.is_some());
            assert_eq!(pools.len(), 1, "{players} entries must remain one pool");
            let mut members = pools[0].members.clone();
            members.sort();
            tickets.sort();
            assert_eq!(
                members, tickets,
                "every completed entry enters exactly once"
            );
            let pool = queue
                .store()
                .get_competition(pools[0].competition_id)
                .await
                .unwrap();
            assert_eq!(pool.event_submission.total_allowed_entries, players);
            if players == 20 {
                assert_eq!(pool.event_submission.number_of_places_win, 2);
            }
        }
    }
}

#[tokio::test]
async fn a_paid_but_incomplete_third_entry_cannot_start_the_default_queue() {
    let queue = default_queue(OffsetDateTime::now_utc() - Duration::minutes(10)).await;
    queue.chain_closing_at(20, 30);
    let mut tickets = Vec::new();
    for (player, complete) in [("first", true), ("second", true), ("third", false)] {
        tickets.push(queue.ticket(player, complete).await);
    }
    assert_eq!(queue.advance().await, Step::Finished);
    let parent = queue
        .store()
        .get_competition(queue.competition.id)
        .await
        .unwrap();
    assert!(parent.is_cancelled());
    assert!(parent.pools_formed_at.is_none());
    assert!(queue
        .store()
        .competition_pools(parent.id)
        .await
        .unwrap()
        .is_empty());
    for ticket in tickets {
        assert_eq!(
            queue.event_of(ticket).await.0,
            parent.id,
            "refunds stay on the queue"
        );
    }
    assert!(queue
        .store()
        .get_competitions_pending_cleanup(false)
        .await
        .unwrap()
        .contains(&parent.id));
}

#[tokio::test]
async fn three_or_twenty_entries_do_not_start_before_registration_closes() {
    for players in [3, 20] {
        let start = OffsetDateTime::now_utc() + Duration::hours(3);
        let queue = default_queue(start).await;
        for index in 0..players {
            queue.ticket(&format!("player{index}"), true).await;
        }
        assert_eq!(queue.advance().await, Step::Next(Wait::Until(start)));
        assert!(queue
            .store()
            .competition_pools(queue.competition.id)
            .await
            .unwrap()
            .is_empty());
    }
}
