use super::*;
use coordinator_escrow::payout_witness::Claim;

async fn fresh_ledger(directory: &Path) -> Ledger {
    let id = Uuid::now_v7();
    let inventory = directory.join("inventory.jsonl");
    std::fs::write(
        &inventory,
        serde_json::json!({
            "ledger_id": id,
            "complete_history_checkpoint": "test-only genesis: no prior claims"
        })
        .to_string(),
    )
    .unwrap();
    let database = directory.join("witness.sqlite");
    initialize(&database, &inventory).await.unwrap();
    Ledger::open(&database).await.unwrap()
}

fn reservation(sequence: u32) -> Reservation {
    let mut payment_hash = [0; 32];
    payment_hash[..4].copy_from_slice(&sequence.to_be_bytes());
    Reservation {
        claim: Claim {
            session_id: Uuid::now_v7(),
            user_id: Uuid::now_v7(),
            claim_id: Uuid::now_v7(),
        },
        payment_hash,
        executing: false,
    }
}

#[tokio::test]
async fn history_survives_capacity_boundary_restart_and_lost_responses() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = fresh_ledger(directory.path()).await;
    let mut first = reservation(0);
    first.executing = true;
    ledger.reserve(ledger.id, &first).await.unwrap();
    for sequence in 1..=4096 {
        ledger
            .reserve(ledger.id, &reservation(sequence))
            .await
            .unwrap();
    }
    assert_eq!(ledger.occupancy().await.unwrap().payment_hashes, 4097);
    let id = ledger.id;
    ledger.close().await;
    let reopened = Ledger::open(&directory.path().join("witness.sqlite"))
        .await
        .unwrap();
    // The original success reply might have been lost; replay does not add rows.
    assert_eq!(
        reopened.reserve(id, &first).await.unwrap().payment_hashes,
        4097
    );
    let mut hash_reuse = reservation(0);
    assert_eq!(
        reopened.reserve(id, &hash_reuse).await,
        Err(WitnessError::Conflict)
    );
    hash_reuse.payment_hash = [255; 32];
    hash_reuse.claim.session_id = first.claim.session_id;
    hash_reuse.claim.user_id = first.claim.user_id;
    assert_eq!(
        reopened.reserve(id, &hash_reuse).await,
        Err(WitnessError::Conflict)
    );
    assert_eq!(
        reopened.reserve(Uuid::now_v7(), &first).await,
        Err(WitnessError::WrongLedger)
    );
    assert_eq!(reopened.occupancy().await.unwrap().payment_hashes, 4097);
    reopened.close().await;
}

#[tokio::test]
async fn concurrent_conflicting_owners_cannot_both_commit() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = fresh_ledger(directory.path()).await;
    let first = reservation(7);
    let second = reservation(7);
    let (a, b) = tokio::join!(
        ledger.reserve(ledger.id, &first),
        ledger.reserve(ledger.id, &second)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(ledger.occupancy().await.unwrap().payment_hashes, 1);
    ledger.close().await;
}

#[tokio::test]
async fn missing_or_partial_database_never_becomes_a_fresh_ledger() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing.sqlite");
    assert!(Ledger::open(&path).await.is_err());
    assert!(!path.exists());
    std::fs::write(&path, []).unwrap();
    assert!(Ledger::open(&path).await.is_err());
}
