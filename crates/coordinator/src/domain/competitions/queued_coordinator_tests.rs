//! A pool's oracle event, signed statement and payout table, against the mock oracle.

use super::*;
use crate::domain::competitions::queued_tests::Queue;
use crate::infra::oracle::Oracle;
use coordinator_escrow::pools::PoolRules;
use time::Duration;

/// A queue of five complete tickets, formed into pools, and its first pool.
async fn formed() -> (Queue, Competition, Vec<Uuid>) {
    let start = OffsetDateTime::now_utc() - Duration::minutes(10);
    let queue = Queue::new(start, PoolRules::new(2, 3).unwrap(), 100).await;
    formed_from(queue, 5).await
}

/// The default competition, one pool of up to 20 paying two places from ten players, formed
/// from `players` complete tickets.
async fn formed_paying_two(players: usize) -> (Queue, Competition, Vec<Uuid>) {
    let start = OffsetDateTime::now_utc() - Duration::minutes(10);
    let queue = Queue::paying(start, PoolRules::new(2, 20).unwrap(), 20, 2).await;
    formed_from(queue, players).await
}

async fn formed_from(queue: Queue, players: usize) -> (Queue, Competition, Vec<Uuid>) {
    queue.chain_closing_at(20, 30);
    for player in 0..players {
        queue.ticket(&format!("player{player}"), true).await;
    }
    queue.advance().await;
    let record = queue
        .store()
        .competition_pools(queue.competition.id)
        .await
        .unwrap()
        .remove(0);
    let pool = queue
        .coordinator
        .get_competition(record.competition_id)
        .await
        .unwrap();
    (queue, pool, record.members)
}

#[tokio::test]
async fn a_pool_event_copies_the_reference_lines_and_its_statement_is_checked() {
    let (queue, mut pool, members) = formed().await;
    let coordinator = &queue.coordinator;
    let (settings, record) = coordinator.pool_of(&pool).await.unwrap().unwrap();
    assert_eq!(settings, queue.settings);
    assert_eq!(record.members, members);

    coordinator.submit_event_to_oracle(&mut pool).await.unwrap();
    assert!(pool.event_created_at.is_some());
    let event = queue.oracle.get_event_terms(&pool.id).await.unwrap();
    assert_eq!(event.total_allowed_entries, members.len());
    assert!(event
        .observation()
        .unwrap()
        .same_as(&settings.terms.observation));
    assert!(
        coordinator
            .pool_statement(&pool, &settings, &members)
            .await
            .is_err(),
        "no statement before every entry is in"
    );

    coordinator
        .submit_entries_to_oracle(&mut pool)
        .await
        .unwrap();
    let statement = coordinator
        .pool_statement(&pool, &settings, &members)
        .await
        .unwrap();
    assert_eq!(statement.statement.event_id, pool.id);
    // Every player's contract terms follow from their consent and the statement.
    let entry = coordinator_escrow::queued::QueuedEntryTerms {
        terms: settings.terms.clone(),
        entry_id: members[0],
        ticket_hash: [1; 32],
        payout_hash: [2; 32],
    };
    let derived =
        coordinator_escrow::queued::pool_authorization(&entry, &members, &statement).unwrap();
    assert_eq!(derived.competition_id, pool.id);
    assert_eq!(derived.player_count, members.len());
    assert_eq!(
        derived.funding_value.to_sat() as usize,
        pool.event_submission.total_competition_pool
    );
    assert_eq!(
        Some(&derived.event),
        pool.event_announcement.as_ref(),
        "the statement's locking points are the ones the pool's event announced"
    );

    // Another pool's members, or an announcement that is not the statement's, are refused.
    let mut others = members.clone();
    others[0] = Uuid::now_v7();
    assert!(check_pool_statement(&pool, &settings, &others, &statement).is_err());
    let mut changed = pool.clone();
    changed
        .event_announcement
        .as_mut()
        .unwrap()
        .locking_points
        .reverse();
    assert!(check_pool_statement(&changed, &settings, &members, &statement).is_err());
    let mut forged = statement.clone();
    forged.statement.signing_date += 1;
    assert!(check_pool_statement(&pool, &settings, &members, &forged).is_err());
}

#[tokio::test]
async fn a_pool_event_with_other_lines_or_terms_is_refused() {
    let (queue, pool, _) = formed().await;
    let settings = &queue.settings;
    let created = queue
        .oracle
        .create_event_from_lines(pool.event_submission.clone(), settings.competition_id)
        .await
        .unwrap();
    let event = queue.oracle.get_event_terms(&pool.id).await.unwrap();
    check_pool_event(&pool, settings, &created, &event).unwrap();

    // An oracle that ignored lines_from_event froze lines of its own.
    let mut refit = event.clone();
    refit.lines[0].upper += 0.25;
    assert!(check_pool_event(&pool, settings, &created, &refit).is_err());
    let mut seats = event.clone();
    seats.total_allowed_entries += 1;
    assert!(check_pool_event(&pool, settings, &created, &seats).is_err());
    let mut signing = event.clone();
    signing.signing_date += Duration::hours(1);
    assert!(check_pool_event(&pool, settings, &created, &signing).is_err());
    let mut other = created.clone();
    other.nonce_point = dlctix::secp::Scalar::from_slice(&[5; 32])
        .unwrap()
        .base_point_mul();
    assert!(check_pool_event(&pool, settings, &other, &event).is_err());
}

#[tokio::test]
async fn a_pool_pays_one_winner_from_its_players_consent() {
    let (queue, pool, members) = formed().await;
    let coordinator = &queue.coordinator;
    let mut entries = coordinator
        .competition_store
        .get_competition_entries(pool.id, vec![EntryStatus::Paid])
        .await
        .unwrap();
    entries.sort_by_key(|entry| entry.ticket_id);
    assert_eq!(
        entries.iter().map(|e| e.ticket_id).collect::<Vec<_>>(),
        members
    );
    let payouts = coordinator
        .accepted_pool_payouts(&pool, &queue.settings, &members, &entries)
        .await
        .unwrap();
    assert_eq!(
        payouts,
        coordinator_escrow::queued::pool_payouts(members.len(), 1).unwrap()
    );
    // Missing a player, or with another pool's, the roster is not the pool's.
    assert!(coordinator
        .accepted_pool_payouts(&pool, &queue.settings, &members, &entries[1..])
        .await
        .is_err());
    let mut others = members.clone();
    others[0] = Uuid::now_v7();
    assert!(coordinator
        .accepted_pool_payouts(&pool, &queue.settings, &others, &entries)
        .await
        .is_err());
    // Terms other than the queue's are refused.
    let mut other = queue.settings.clone();
    other.terms.stake_sats += 1;
    other.terms_digest = other.terms.digest().unwrap();
    assert!(coordinator
        .accepted_pool_payouts(&pool, &other, &members, &entries)
        .await
        .is_err());
}

#[tokio::test]
async fn a_pool_event_made_before_a_restart_is_found_not_made_again() {
    let (queue, pool, _) = formed().await;
    let coordinator = &queue.coordinator;
    let first = coordinator
        .create_pool_event(&pool, &queue.settings)
        .await
        .unwrap();
    let again = coordinator
        .create_pool_event(&pool, &queue.settings)
        .await
        .unwrap();
    assert_eq!(first.id, pool.id);
    assert_eq!(
        (again.nonce_point, again.event_announcement),
        (first.nonce_point, first.event_announcement)
    );
}

/// A pool of ten pays its first two places 70% and 30%; a pool of five pays its winner the pot.
/// The pool's oracle event, its statement and its payout table all carry the places its size
/// gives, as every player's consent states.
#[tokio::test]
async fn a_pool_pays_the_places_its_size_gives() {
    for (players, places) in [(20, 2), (10, 2), (5, 1), (3, 1)] {
        let (queue, mut pool, members) = formed_paying_two(players).await;
        let coordinator = &queue.coordinator;
        assert_eq!(members.len(), players, "one pool of everyone");
        assert_eq!(pool.event_submission.number_of_places_win, places);
        let (settings, _) = coordinator.pool_of(&pool).await.unwrap().unwrap();
        assert_eq!(settings.terms.number_of_places_win, 2);
        assert_eq!(settings.terms.pool_places(players), places as u32);

        coordinator.submit_event_to_oracle(&mut pool).await.unwrap();
        let event = queue.oracle.get_event_terms(&pool.id).await.unwrap();
        assert_eq!(event.number_of_places_win as usize, places);
        coordinator
            .submit_entries_to_oracle(&mut pool)
            .await
            .unwrap();
        let statement = coordinator
            .pool_statement(&pool, &settings, &members)
            .await
            .unwrap();
        let coordinator_escrow::oracle_statement::Outcomes::Ranking(ranking) =
            &statement.statement.outcomes;
        assert_eq!(ranking.number_of_places_win as usize, places);

        let mut entries = coordinator
            .competition_store
            .get_competition_entries(pool.id, vec![EntryStatus::Paid])
            .await
            .unwrap();
        entries.sort_by_key(|entry| entry.ticket_id);
        let payouts = coordinator
            .accepted_pool_payouts(&pool, &settings, &members, &entries)
            .await
            .unwrap();
        assert_eq!(
            payouts,
            coordinator_escrow::queued::pool_payouts(players, places).unwrap()
        );
        let first = &payouts[&dlctix::Outcome::Attestation(0)];
        let shares: Vec<u64> = first.values().copied().collect();
        if places == 2 {
            assert_eq!(shares, vec![70, 30]);
        } else {
            assert_eq!(shares, vec![100]);
        }
    }
}
