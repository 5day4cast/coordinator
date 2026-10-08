//! The public list's lean read of every competition against the full read, on a real migrated
//! database: the same rows in the same order, the same phases, and the same pages once the
//! shown rows are completed with their contracts.
use super::*;
use crate::domain::leaderboard::Phase;
use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
use crate::templates::pages::competitions::{
    competitions_page, shown_ids, CompetitionView, ListOptions, Tab,
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
        contract_options: None,
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
        anchor: None,
        outcome_bound_splits: false,
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
    rows.push((contracted.clone(), 1, 1));
    // Contract state takes precedence over all earlier lifecycle timestamps.
    for earlier in 0..3 {
        let mut c = contracted.clone();
        c.id = Uuid::now_v7();
        match earlier {
            0 => c.escrow_funds_confirmed_at = Some(now),
            1 => c.event_created_at = Some(now),
            _ => c.entries_submitted_at = Some(now),
        }
        rows.push((c, 1, 1));
    }
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
    let mut expected_counts = std::collections::BTreeMap::new();
    for competition in &full {
        *expected_counts
            .entry(states::CompetitionStatus::from(competition.clone()).state_name())
            .or_insert(0_i64) += 1;
    }
    let counts = store.competition_state_counts().await.unwrap();
    assert_eq!(
        counts
            .states
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
        expected_counts
    );
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
    for tab in [Tab::Overview, Tab::Live, Tab::Finished] {
        for page in 0..4 {
            for show_cancelled in [false, true] {
                let options = ListOptions {
                    tab,
                    page,
                    show_cancelled,
                    ..Default::default()
                };
                // As the handler does: complete the shown rows with their contracts.
                let mut lean_views: Vec<_> = lean
                    .iter()
                    .map(|competition| CompetitionView::new(competition, now))
                    .collect();
                let shown = shown_ids(&lean_views, &options);
                assert_eq!(shown, shown_ids(&full_views, &options));
                for view in lean_views
                    .iter_mut()
                    .filter(|view| shown.contains(&view.id))
                {
                    let competition = full.iter().find(|c| c.id.to_string() == view.id).unwrap();
                    view.add_contract(competition);
                }
                assert_eq!(
                    competitions_page(&lean_views, &options, now).into_string(),
                    competitions_page(&full_views, &options, now).into_string(),
                    "{tab:?}, page {page}, cancelled {show_cancelled}"
                );
            }
        }
    }
    let finished = ListOptions {
        tab: Tab::Finished,
        ..Default::default()
    };
    let first = competitions_page(&full_views, &finished, now).into_string();
    assert!(first.contains("Page 1 of 3"), "{first}");
    assert!(first.contains("no winner · pot shared back"));
    database.close().await.unwrap();
}

#[tokio::test]
async fn an_observer_read_preserves_settlement_evidence_without_decoding_the_signed_graph() {
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
    let (params, event, attestation) = pot_return_contract();
    let mut c = competition(now - Duration::DAY, false);
    c.contract_parameters = Some(params);
    c.event_announcement = Some(event);
    c.signed_at = Some(now);
    c.funding_confirmed_at = Some(now);
    c.awaiting_attestation_at = Some(now);
    c.attestation = Some(attestation);
    let tx = bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![],
        output: vec![bitcoin::TxOut {
            value: Amount::from_sat(3_000),
            script_pubkey: bitcoin::ScriptBuf::new(),
        }],
    };
    c.funding_outpoint = Some(bitcoin::OutPoint {
        txid: tx.compute_txid(),
        vout: 0,
    });
    c.funding_transaction = Some(tx.clone());
    c.outcome_transaction = Some(tx);
    c.errors.push(CompetitionError::InvalidStateTransition(
        "retained evidence".into(),
    ));
    store
        .add_competition_with_tickets(c.clone(), vec![])
        .await
        .unwrap();
    store.update_competitions(vec![c.clone()]).await.unwrap();
    enter(&database, c.id, 3, 3).await;
    let expected = serde_json::to_value(store.get_competition(c.id).await.unwrap()).unwrap();

    let id = c.id;
    database
        .execute_write(move |pool| async move {
            sqlx::query("UPDATE competitions SET signed_contract = ? WHERE id = ?")
                .bind(b"a graph that must not be decoded".to_vec())
                .bind(id.to_string())
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();

    let observed = store.get_competition_detail(c.id, false).await.unwrap();
    assert_eq!(serde_json::to_value(observed).unwrap(), expected);
    assert!(
        store.get_competition(c.id).await.is_err(),
        "full reads still decode the graph"
    );
    assert!(matches!(
        store.get_competition_detail(Uuid::now_v7(), false).await,
        Err(sqlx::Error::RowNotFound)
    ));
    database.close().await.unwrap();
}

#[tokio::test]
async fn operator_summaries_keep_signing_errors_without_loading_contracts() {
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
    let mut c = competition(now + Duration::DAY, true);
    c.contract_parameters = Some(pot_return_contract().0);
    c.contracted_at = Some(now);
    c.cancelled_at = Some(now);
    c.errors.push(CompetitionError::InvalidStateTransition(
        "Keymeld enclave signing failed".into(),
    ));
    store
        .add_competition_with_tickets(c.clone(), vec![])
        .await
        .unwrap();
    store.update_competitions(vec![c.clone()]).await.unwrap();

    let public = store.list_competitions().await.unwrap();
    let operator = store.list_operator_competitions().await.unwrap();
    assert_eq!(public.len(), 1);
    assert_eq!(operator.len(), 1);
    assert_eq!(operator[0].id, c.id);
    assert_eq!(operator[0].get_state(), public[0].get_state());
    assert!(public[0].errors.is_empty());
    assert_eq!(
        serde_json::to_value(&operator[0].errors).unwrap(),
        serde_json::to_value(&c.errors).unwrap()
    );
    assert!(operator[0].contract_parameters.is_none());
    assert!(operator[0].funding_transaction.is_none());
    assert!(operator[0].signed_contract.is_none());
    let full = store.get_competition(c.id).await.unwrap();
    let as_api = |competition: &Competition| {
        serde_json::to_value(crate::api::routes::OperatorCompetition::new(
            competition,
            None,
            vec![],
        ))
        .unwrap()
    };
    assert_eq!(
        as_api(&operator[0]),
        as_api(&full),
        "lean inventory preserves every operator field"
    );
    database.close().await.unwrap();
}

#[tokio::test]
async fn operator_payment_coverage_reads_only_successful_outcome_recipients() {
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
    let (mut params, mut event, attestation) = pot_return_contract();
    event.expiry = Some((now + Duration::DAY).unix_timestamp() as u32);
    params.event = event.clone();
    params.outcome_payouts.insert(
        Outcome::Expiry,
        PayoutWeights::from([(0, 1), (1, 1), (2, 1)]),
    );
    let mut c = competition(now - Duration::DAY, false);
    c.event_announcement = Some(event);
    c.contract_parameters = Some(params.clone());
    c.attestation = Some(attestation);
    c.completed_at = Some(now);
    store
        .add_competition_with_tickets(c.clone(), vec![])
        .await
        .unwrap();
    store.update_competitions(vec![c.clone()]).await.unwrap();
    enter(&database, c.id, 3, 3).await;
    let entries: Vec<String> =
        sqlx::query_scalar("SELECT id FROM entries WHERE event_id=? ORDER BY id")
            .bind(c.id.to_string())
            .fetch_all(database.read())
            .await
            .unwrap();
    let assign = entries.clone();
    database
        .execute_write(move |pool| async move {
            for (entry, player) in assign.iter().zip(params.players) {
                sqlx::query("UPDATE entries SET ephemeral_pubkey=? WHERE id=?")
                    .bind(player.pubkey.to_string())
                    .bind(entry)
                    .execute(&pool)
                    .await?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let read = |c: Competition| {
        let store = store.clone();
        async move { store.operator_payout_progress(&[c]).await.unwrap() }
    };
    let before = read(c.clone()).await;
    assert_eq!(
        before.get(&c.id),
        Some(&OperatorPayoutProgress {
            paid: 0,
            expected: 3
        })
    );

    let payments = entries.clone();
    database.execute_write(move |pool| async move {
        // Pending and failed attempts cannot imply all paid. The schema permits only one
        // live attempt per entry; duplicate and wrong-amount evidence is tested separately.
        for (entry, success, failed, amount) in [(&payments[0], true, false, 1000), (&payments[1], false, false, 1000), (&payments[2], false, true, 1000)] {
            sqlx::query("INSERT INTO payouts(id,entry_id,payout_payment_request,payout_amount_sats,initiated_at,succeed_at,failed_at) VALUES (?,?,'invoice',?,datetime('now'),CASE WHEN ? THEN datetime('now') END,CASE WHEN ? THEN datetime('now') END)")
                .bind(Uuid::now_v7().to_string()).bind(entry).bind(amount).bind(success).bind(failed).execute(&pool).await?;
        }
        Ok(())
    }).await.unwrap();
    let partial = read(c.clone()).await;
    assert_eq!(
        partial.get(&c.id),
        Some(&OperatorPayoutProgress {
            paid: 1,
            expected: 3
        })
    );
    assert!(!partial[&c.id].all_paid());

    database.execute_write(move |pool| async move {
        sqlx::query("UPDATE payouts SET failed_at=datetime('now') WHERE entry_id=? AND succeed_at IS NULL").bind(&entries[1]).execute(&pool).await?;
        for entry in &entries[1..] {
            sqlx::query("INSERT INTO payouts(id,entry_id,payout_payment_request,payout_amount_sats,initiated_at,succeed_at) VALUES (?,?,'invoice',1000,datetime('now'),datetime('now'))").bind(Uuid::now_v7().to_string()).bind(entry).execute(&pool).await?;
        }
        Ok(())
    }).await.unwrap();
    assert!(read(c.clone()).await[&c.id].all_paid());

    c.attestation = None;
    c.expiry_broadcasted_at = Some(now);
    store.update_competitions(vec![c.clone()]).await.unwrap();
    assert_eq!(
        read(c.clone()).await[&c.id],
        OperatorPayoutProgress {
            paid: 3,
            expected: 3
        }
    );
    // A retained expiry marker alongside a later attestation cannot choose the expiry weights.
    c.attestation = Some(attestation);
    store.update_competitions(vec![c.clone()]).await.unwrap();
    assert!(!read(c.clone()).await.contains_key(&c.id));
    c.expiry_broadcasted_at = None;
    store.update_competitions(vec![c.clone()]).await.unwrap();
    let id = c.id.to_string();
    database
        .execute_write(move |pool| async move {
            sqlx::query("UPDATE competitions SET contract_parameters='invalid json' WHERE id=?")
                .bind(id)
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!read(c.clone()).await.contains_key(&c.id));
    database.close().await.unwrap();
}

#[tokio::test]
async fn api_pages_bound_history_preserve_fields_and_keep_entry_owners_separate() {
    let directory = tempfile::tempdir().unwrap();
    let database = DBConnection::new(
        directory.path().to_str().unwrap(),
        "pages",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap();
    let store = CompetitionStore::new(database.clone());
    let now = OffsetDateTime::now_utc();
    let mut ids = Vec::new();
    for _ in 0..4 {
        let row = competition(now + Duration::HOUR, false);
        ids.push(row.id);
        store
            .add_competition_with_tickets(row, vec![])
            .await
            .unwrap();
    }
    ids.sort_by_key(|id| std::cmp::Reverse(*id));
    let page = ListPage {
        limit: 2,
        history: true,
        ..Default::default()
    };
    assert_eq!(store.competition_page_ids(&page).await.unwrap(), ids[..3]);
    let next = ListPage {
        before: Some(ids[1]),
        ..page.clone()
    };
    assert_eq!(store.competition_page_ids(&next).await.unwrap(), ids[2..]);
    let selected = store
        .get_competitions_selected(false, Some(&ids[..2]))
        .await
        .unwrap();
    assert_eq!(selected.len(), 2);
    for row in selected {
        let full = store
            .get_competitions(false)
            .await
            .unwrap()
            .into_iter()
            .find(|full| full.id == row.id)
            .unwrap();
        assert_eq!(
            serde_json::to_value(row).unwrap(),
            serde_json::to_value(full).unwrap()
        );
    }
    enter(&database, ids[0], 3, 1).await;
    assert_eq!(
        store
            .entry_page_ids("owner", &[ids[0]], &page)
            .await
            .unwrap()
            .len(),
        3
    );
    assert!(store
        .entry_page_ids("someone else", &[ids[0]], &page)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .entry_page_ids("owner", &[ids[1]], &page)
        .await
        .unwrap()
        .is_empty());
    let old = ids[0].to_string();
    database.execute_write(move |pool| async move {
        sqlx::query("UPDATE competitions SET cancelled_at = '2000-01-01T00:00:00Z' WHERE id = ?").bind(&old).execute(&pool).await?;
        sqlx::query("UPDATE list_updates SET updated_at = '2000-01-01T00:00:00Z' WHERE kind='competition' AND id = ?").bind(&old).execute(&pool).await?;
        Ok(())
    }).await.unwrap();
    let current = ListPage {
        limit: 100,
        ..Default::default()
    };
    assert!(!store
        .competition_page_ids(&current)
        .await
        .unwrap()
        .contains(&ids[0]));
    let explicit = ListPage {
        ids: vec![ids[0]],
        ..current.clone()
    };
    assert_eq!(
        store.competition_page_ids(&explicit).await.unwrap(),
        vec![ids[0]]
    );
    let cancelled = ListPage {
        status: Some("cancelled".into()),
        ..current.clone()
    };
    assert_eq!(
        store.competition_page_ids(&cancelled).await.unwrap(),
        vec![ids[0]]
    );
    let changed = ListPage {
        since: Some(now - Duration::MINUTE),
        ..current
    };
    assert_eq!(store.competition_page_ids(&changed).await.unwrap().len(), 3);
    database.close().await.unwrap();
}

#[tokio::test]
async fn payout_transition_waits_for_an_independent_sqlite_writer() {
    let directory = tempfile::tempdir().unwrap();
    let database = DBConnection::new(
        directory.path().to_str().unwrap(),
        "busy",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap();
    let store = CompetitionStore::new(database.clone());
    let row = competition(OffsetDateTime::now_utc() + Duration::HOUR, false);
    let id = row.id;
    store
        .add_competition_with_tickets(row, vec![])
        .await
        .unwrap();
    let other = sqlx::SqlitePool::connect(&format!("sqlite:{}", database.database_path))
        .await
        .unwrap();
    let mut held = other.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("UPDATE competitions SET entries_submitted_at = datetime('now') WHERE id = ?")
        .bind(id.to_string())
        .execute(&mut *held)
        .await
        .unwrap();
    let transition = tokio::spawn(async move { store.close_payout_window(id).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!transition.is_finished(), "a busy writer is waited for");
    held.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), transition)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    other.close().await;
    database.close().await.unwrap();
}
