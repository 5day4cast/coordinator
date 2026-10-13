use super::*;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

/// Items spread over three enclaves out of order, as a pool's players are.
fn items() -> Vec<(EnclaveId, u32)> {
    [2, 1, 3, 2, 1, 3, 2, 2]
        .into_iter()
        .zip(0..)
        .map(|(enclave, item)| (EnclaveId::new(enclave), item))
        .collect()
}

/// What each enclave was asked for, in the order it was asked, and how many shares ran at once.
#[derive(Default)]
struct Seen {
    asked: BTreeMap<u32, Vec<u32>>,
    running: usize,
    most_running: usize,
}

fn record_running_share(seen: &StdMutex<Seen>) {
    let mut seen = seen.lock().unwrap();
    seen.running += 1;
    seen.most_running = seen.most_running.max(seen.running);
}

/// Work on one enclave's share: the first enclave's items take longest, so shares finish out of
/// order. `fails` names an item that fails, after the items before it.
async fn work(
    seen: &StdMutex<Seen>,
    lane: Vec<u32>,
    items: Vec<u32>,
    fails: &[u32],
) -> Result<Vec<String>, KeymeldError> {
    assert!(lane.is_empty(), "each share starts from its own copy");
    record_running_share(seen);
    let mut lane = lane;
    let mut results = Vec::new();
    let mut failed = None;
    for item in items {
        let enclave = items_enclave(item);
        tokio::time::sleep(Duration::from_millis(u64::from(4 - enclave) * 5)).await;
        seen.lock()
            .unwrap()
            .asked
            .entry(enclave)
            .or_default()
            .push(item);
        if fails.contains(&item) {
            failed = Some(invalid(format!("item {item} failed")));
            break;
        }
        lane.push(item);
        results.push(format!("signed {item}"));
    }
    seen.lock().unwrap().running -= 1;
    match failed {
        Some(error) => Err(error),
        None => Ok(results),
    }
}

fn items_enclave(item: u32) -> u32 {
    items()[item as usize].0.as_u32()
}

#[tokio::test]
async fn per_enclave_results_match_the_serial_order() {
    let seen = StdMutex::new(Seen::default());
    let lane: Vec<u32> = Vec::new();
    let results = per_enclave(items(), &lane, |lane, items| work(&seen, lane, items, &[]))
        .await
        .unwrap();

    // The serial path: every item in turn.
    let serial: Vec<_> = items()
        .into_iter()
        .map(|(_, item)| format!("signed {item}"))
        .collect();
    assert_eq!(results, serial);
    let seen = seen.into_inner().unwrap();
    // Each enclave was asked for its items in their order.
    assert_eq!(
        seen.asked,
        BTreeMap::from([(1, vec![1, 4]), (2, vec![0, 3, 6, 7]), (3, vec![2, 5])])
    );
    assert_eq!(seen.most_running, 3, "the shares ran side by side");
    assert!(lane.is_empty());
}

#[tokio::test]
async fn per_enclave_fails_whole_when_one_share_fails() {
    let seen = StdMutex::new(Seen::default());
    let result = per_enclave(items(), &Vec::new(), |lane, items| {
        work(&seen, lane, items, &[3])
    })
    .await;
    let Err(error) = result else {
        panic!("a failed share failed the call");
    };
    assert!(error.to_string().contains("item 3 failed"), "{error}");
    // The other shares ran to their end rather than being dropped mid-request.
    let seen = seen.into_inner().unwrap();
    assert_eq!(seen.asked[&1], vec![1, 4]);
    assert_eq!(seen.asked[&3], vec![2, 5]);
    assert_eq!(seen.asked[&2], vec![0, 3]);

    // With two failures, the first enclave's is returned.
    let seen = StdMutex::new(Seen::default());
    let Err(error) = per_enclave(items(), &Vec::new(), |lane, items| {
        work(&seen, lane, items, &[5, 4])
    })
    .await
    else {
        panic!("failed shares failed the call");
    };
    assert!(error.to_string().contains("item 4 failed"), "{error}");
}

#[tokio::test]
async fn per_enclave_refuses_a_share_with_missing_results() {
    let result = per_enclave(items(), &(), |_, items: Vec<u32>| async move {
        Ok::<_, KeymeldError>(items.into_iter().skip(1).collect::<Vec<_>>())
    })
    .await;
    assert!(result.is_err());
}

/// A service with a database holding one session's checkpoint row, `parts` parts and nodes, and
/// a refund invoice authorization. Deletion reads none of them, so they are raw rows.
async fn service_with_state(parts: i64) -> (tempfile::TempDir, KeymeldService, SessionId) {
    let directory = tempfile::tempdir().unwrap();
    let db = DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        crate::infra::db::DatabasePoolConfig::default(),
        crate::infra::db::DatabaseType::Competitions,
    )
    .await
    .unwrap();
    let mut service =
        KeymeldService::new(KeymeldSettings::default(), Uuid::now_v7(), &[1; 32]).unwrap();
    service.db = Some(db.clone());
    let session = SessionId::new_v7();
    let id = session.to_string();
    db.execute_write(move |pool| async move {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO keymeld_protocol_state (session_id, version, encrypted_state, format)
             VALUES (?, 7, 'sealed root', 3)",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?)
             INSERT INTO keymeld_protocol_parts (session_id, digest, body)
             SELECT ?, randomblob(32), zeroblob(1000) FROM n",
        )
        .bind(parts)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO refund_invoice_authorizations
                 (keygen_session_id, user_id, invoice, authorization)
             VALUES (?, 'player', 'lnbcrt1refund', 'authenticated receipt')",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    })
    .await
    .unwrap();
    (directory, service, session)
}

async fn stored_rows(service: &KeymeldService, session: &SessionId) -> (i64, i64, i64) {
    let db = service.database().unwrap();
    let count = |table: &'static str| {
        let sql = format!(
            "SELECT COUNT(*) FROM {table} WHERE {}",
            match table {
                "refund_invoice_authorizations" => "keygen_session_id = ?",
                _ => "session_id = ?",
            }
        );
        let id = session.to_string();
        async move {
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
                .bind(id)
                .fetch_one(db.read())
                .await
                .unwrap()
        }
    };
    (
        count("keymeld_protocol_state").await,
        count("keymeld_protocol_parts").await,
        count("refund_invoice_authorizations").await,
    )
}

/// Signing state is deleted only under a hold on its session, which an operation in progress
/// prevents, and which keeps any operation from starting until the deletion is done. The
/// operation that runs after it finds no checkpoint, and fails closed.
#[tokio::test]
async fn signing_state_is_deleted_only_while_no_operation_holds_its_session() {
    let (_directory, service, session) = service_with_state(600).await;

    // An operation holds the session for as long as it runs, as every one does.
    let operation = service.lock_session(&session).await;
    assert!(service.try_hold_session(&session).await.is_none());
    assert_eq!(stored_rows(&service, &session).await, (1, 600, 1));
    drop(operation);

    let hold = service
        .try_hold_session(&session)
        .await
        .expect("no operation holds it");
    assert_eq!(hold.session(), &session);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), service.lock_session(&session))
            .await
            .is_err(),
        "no operation starts while the deletion holds its session"
    );
    assert!(service.try_hold_session(&session).await.is_none());
    assert!(
        service
            .try_hold_session(&SessionId::new_v7())
            .await
            .is_some(),
        "other sessions are not held"
    );

    let deleted = service.delete_signing_state(&hold, u64::MAX).await.unwrap();
    assert_eq!(
        (
            deleted.rows,
            deleted.parts,
            deleted.authorizations,
            deleted.finished
        ),
        (1, 600, 1, true)
    );
    assert_eq!(deleted.part_bytes, 600 * 1000);
    assert_eq!(deleted.row_bytes, "sealed root".len() as u64);
    assert_eq!(stored_rows(&service, &session).await, (0, 0, 0));
    drop(hold);

    let _operation = tokio::time::timeout(Duration::from_secs(5), service.lock_session(&session))
        .await
        .expect("operations run again once the hold is dropped");
    assert!(
        confidential_store::load(service.database().unwrap(), &service.store_key, &session)
            .await
            .unwrap()
            .is_none(),
        "and find no checkpoint"
    );
}

/// A hold that does not hold the session's lock, as a Keymeld that keeps no state makes, cannot
/// delete this service's state.
#[tokio::test]
async fn signing_state_is_never_deleted_without_the_sessions_lock() {
    let (_directory, service, session) = service_with_state(3).await;
    let unheld = SessionHold {
        session: session.clone(),
        guard: None,
    };
    assert!(service
        .delete_signing_state(&unheld, u64::MAX)
        .await
        .is_err());
    assert_eq!(stored_rows(&service, &session).await, (1, 3, 1));
}
