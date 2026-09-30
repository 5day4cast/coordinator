//! The public list's lean read of every competition against the full read, on a real migrated
//! database: the same rows in the same order, the same phases, and the same pages once the
//! shown rows are completed with their contracts.
use super::*;
use crate::domain::leaderboard::Phase;
use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
use crate::templates::pages::competitions::{
    competitions_page, shown_ids, CompetitionView, ListOptions,
};
use dlctix::{
    bitcoin::{Amount, FeeRate},
    hashlock,
    secp::{MaybeScalar, Scalar},
    ContractParameters, EventLockingConditions, MarketMaker, Outcome, PayoutWeights, Player,
};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

fn competition(start: OffsetDateTime, unlisted: bool) -> Competition {
    Competition::new(&CreateEvent {
        id: Uuid::now_v7(),
        signing_date: start + Duration::days(2),
        start_observation_date: start,
        end_observation_date: start + Duration::hours(1),
        locations: vec!["KORD".into()],
        number_of_values_per_entry: 1,
        number_of_places_win: 1,
        total_allowed_entries: 3,
        entry_fee: 1_000,
        coordinator_fee: CoordinatorFee::whole_percent(10),
        total_competition_pool: 3_000,
        relative_locktime_block_delta: None,
        unlisted,
        scoring_rules: None,
        scoring_fields: None,
        max_entries_per_player: 1,
    })
}

/// A three-player contract whose only outcome pays every entry the same share, and the
/// attestation of that outcome.
fn pot_return_contract() -> (ContractParameters, EventLockingConditions, MaybeScalar) {
    let point = |byte: u8| Scalar::from_slice(&[byte; 32]).unwrap().base_point_mul();
    let attestation = Scalar::from_slice(&[20; 32]).unwrap();
    let event = EventLockingConditions {
        locking_points: vec![attestation.base_point_mul().into()],
        expiry: None,
    };
    let params = ContractParameters {
        market_maker: MarketMaker { pubkey: point(5) },
        players: (1u8..=3)
            .map(|key| Player {
                pubkey: point(key),
                ticket_hash: hashlock::sha256(&[key + 10; 32]),
                payout_hash: hashlock::sha256(&[key + 1; 32]),
            })
            .collect(),
        event: event.clone(),
        outcome_payouts: [(
            Outcome::Attestation(0),
            PayoutWeights::from([(0, 1), (1, 1), (2, 1)]),
        )]
        .into(),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value: Amount::from_sat(3_000),
        relative_locktime_block_delta: 72,
    };
    (params, event, attestation.into())
}

/// `entries` entries, the first `paid` of them paid for.
async fn enter(database: &DBConnection, event_id: Uuid, entries: usize, paid: usize) {
    database
        .execute_write(move |pool| async move {
            let event = event_id.to_string();
            for index in 0..entries {
                let ticket = Uuid::now_v7().to_string();
                let entry = Uuid::now_v7().to_string();
                sqlx::query(
                    "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_at, paid_at)
                     VALUES (?, ?, 'encrypted', ?, datetime('now'), CASE WHEN ? THEN datetime('now') END)",
                )
                .bind(&ticket)
                .bind(&event)
                .bind(format!("hash-{ticket}"))
                .bind(index < paid)
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey,
                         payout_hash, entry_submission)
                     VALUES (?, ?, ?, 'owner', ?, ?, '{}')",
                )
                .bind(&entry)
                .bind(&event)
                .bind(&ticket)
                .bind(format!("pubkey-{entry}"))
                .bind(format!("hash-{entry}"))
                .execute(&pool)
                .await?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn the_list_reads_the_same_rows_order_and_pages_as_the_full_read() {
    let directory = tempfile::tempdir().unwrap();
    let database = DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap();
    let store = CompetitionStore::new(database.clone());
    let now = OffsetDateTime::now_utc();
    let hours = Duration::hours;

    // (competition, entries, paid): every phase the list groups, enough finished ones for three
    // pages, and one of each that only the full read could tell apart.
    let mut rows: Vec<(Competition, usize, usize)> = Vec::new();
    for index in 0..3 {
        rows.push((competition(now + hours(2 + index), false), 1, 1));
    }
    rows.push((competition(now - Duration::minutes(10), false), 3, 3));
    rows.push((competition(now - hours(3), false), 3, 3));
    for index in 0..14 {
        let mut scored = competition(now - hours(100 + index), index == 5);
        scored.attestation = Some(Scalar::from_slice(&[7; 32]).unwrap().into());
        rows.push((scored, 3, 3));
    }
    for index in 0..4 {
        let mut cancelled = competition(now - hours(50 + index), false);
        cancelled.cancelled_at = Some(now - hours(50));
        rows.push((cancelled, if index % 2 == 0 { 1 } else { 3 }, 1));
    }
    for index in 0..2 {
        let mut failed = competition(now - hours(60 + index), false);
        failed.failed_at = Some(now - hours(59));
        rows.push((failed, 3, 3));
    }
    for index in 0..3 {
        rows.push((competition(now - hours(20 + index), false), 1, 1));
    }
    let mut expired = competition(now - hours(40), false);
    expired.expiry_broadcasted_at = Some(now - hours(30));
    rows.push((expired, 3, 3));
    let mut completed = competition(now - hours(41), false);
    completed.completed_at = Some(now - hours(30));
    rows.push((completed, 3, 3));
    // A contract and nothing after it: without the contract it would read as just created,
    // and so as unfilled.
    let (params, event, attestation) = pot_return_contract();
    let mut contracted = competition(now - hours(4), false);
    contracted.contract_parameters = Some(params.clone());
    rows.push((contracted, 1, 1));
    // The newest finished: its pot went back to every entry, which only its contract says.
    let mut returned = competition(now - hours(5), false);
    returned.event_announcement = Some(event);
    returned.contract_parameters = Some(params);
    returned.attestation = Some(attestation);
    rows.push((returned, 3, 3));

    for (competition, entries, paid) in &rows {
        store
            .add_competition_with_tickets(competition.clone(), vec![])
            .await
            .unwrap();
        store
            .update_competitions(vec![competition.clone()])
            .await
            .unwrap();
        enter(&database, competition.id, *entries, *paid).await;
    }

    let full = store.get_competitions(false).await.unwrap();
    let lean = store.list_competitions().await.unwrap();
    let ids = |competitions: &[Competition]| -> Vec<Uuid> {
        competitions
            .iter()
            .map(|competition| competition.id)
            .collect()
    };
    assert_eq!(ids(&lean), ids(&full));
    assert_eq!(lean.len(), rows.len());
    for (lean, full) in lean.iter().zip(&full) {
        assert_eq!(lean.get_state(), full.get_state(), "{}", full.id);
        assert_eq!(Phase::of(lean, now), Phase::of(full, now), "{}", full.id);
        assert_eq!(lean.total_entries, full.total_entries);
        assert_eq!(lean.total_paid_entries, full.total_paid_entries);
        assert_eq!(lean.total_signed_entries, full.total_signed_entries);
        assert_eq!(lean.total_entry_nonces, full.total_entry_nonces);
        assert_eq!(lean.total_paid_out_entries, full.total_paid_out_entries);
        assert_eq!(lean.is_listed(), full.is_listed());
    }
    let returned_id = rows.last().unwrap().0.id;
    let lean_returned = lean.iter().find(|c| c.id == returned_id).unwrap();
    assert!(lean_returned.contract_parameters.is_none() && lean_returned.signed_contract.is_none());

    let full_views: Vec<_> = full
        .iter()
        .map(|competition| CompetitionView::new(competition, now))
        .collect();
    assert!(full_views.iter().any(|view| view.pot_refunded));
    for page in 0..4 {
        for show_cancelled in [false, true] {
            let options = ListOptions {
                page,
                show_cancelled,
            };
            // As the handler does: complete the shown rows with their contracts.
            let mut lean_views: Vec<_> = lean
                .iter()
                .map(|competition| CompetitionView::new(competition, now))
                .collect();
            let shown = shown_ids(&lean_views, options);
            assert_eq!(shown, shown_ids(&full_views, options));
            for view in lean_views
                .iter_mut()
                .filter(|view| shown.contains(&view.id))
            {
                let competition = full.iter().find(|c| c.id.to_string() == view.id).unwrap();
                view.add_contract(competition);
            }
            assert_eq!(
                competitions_page(&lean_views, options, now).into_string(),
                competitions_page(&full_views, options, now).into_string(),
                "page {page}, cancelled {show_cancelled}"
            );
        }
    }
    let first = competitions_page(&full_views, ListOptions::default(), now).into_string();
    assert!(first.contains("Page 1 of 3"), "{first}");
    assert!(first.contains("no winner · pot shared back"));
    database.close().await.unwrap();
}
