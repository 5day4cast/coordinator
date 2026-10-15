//! Compare the SDK dispatch counter with requests received by a local gRPC server.

use ark_core::server::GetVtxosRequest;
use axum::{routing::post, Router};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[tokio::test]
async fn actual_rpc_traffic_is_counted_paced_and_blocked_after_failure() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let received = Arc::new(AtomicUsize::new(0));
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let failing = Arc::new(AtomicBool::new(false));
    let counted = received.clone();
    let recorded = arrivals.clone();
    let reject = failing.clone();
    let app = Router::new().route(
        "/ark.v1.IndexerService/GetVtxos",
        post(move || {
            let counted = counted.clone();
            let recorded = recorded.clone();
            let reject = reject.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                recorded.lock().unwrap().push(Instant::now());
                let status = if reject.load(Ordering::SeqCst) {
                    "14"
                } else {
                    "0"
                };
                (
                    [
                        ("content-type", "application/grpc"),
                        ("grpc-status", status),
                    ],
                    vec![0u8; 5],
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut first = ark_grpc::Client::new(format!("http://{address}"));
    first.connect().await.unwrap();
    let mut second = ark_grpc::Client::new(format!("http://{address}"));
    second.connect().await.unwrap();
    let before = ark_grpc::vtxo_requests_started();
    first
        .list_vtxos(GetVtxosRequest::new_for_outpoints(&[]))
        .await
        .unwrap();
    assert_eq!(ark_grpc::vtxo_requests_started(), before);
    for client in [&first, &second, &first] {
        client
            .list_vtxos(GetVtxosRequest::new_for_outpoints(&[
                bitcoin::OutPoint::null(),
            ]))
            .await
            .unwrap();
    }
    assert_eq!(received.load(Ordering::SeqCst), 3);
    assert_eq!(ark_grpc::vtxo_requests_started(), before + 3);
    for pair in arrivals.lock().unwrap().windows(2) {
        assert!(pair[1] - pair[0] >= Duration::from_secs(2));
    }

    // Exercise the same persisted reservation used by both worker paths. Even
    // after reconnecting to SQLite, one-second ticks cannot repeat a failed RPC.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swaps.sqlite");
    let key = || crate::preimages::PreimageKey::from_wallet_secret(&[1; 32]);
    let store = crate::store::Store::open(&path, key()).await.unwrap();
    let id = uuid::Uuid::now_v7();
    let now = 1_800_000_000;
    assert!(store
        .reserve_indexer_lookup("payment", id, now)
        .await
        .unwrap());
    failing.store(true, Ordering::SeqCst);
    assert!(first
        .list_vtxos(GetVtxosRequest::new_for_outpoints(&[
            bitcoin::OutPoint::null()
        ]))
        .await
        .is_err());
    drop(store);
    let reopened = crate::store::Store::open(&path, key()).await.unwrap();
    for tick in now..now + 10 {
        if reopened
            .reserve_indexer_lookup("payment", id, tick)
            .await
            .unwrap()
        {
            first
                .list_vtxos(GetVtxosRequest::new_for_outpoints(&[
                    bitcoin::OutPoint::null(),
                ]))
                .await
                .unwrap();
            panic!("a one-second worker tick repeated the failed request");
        }
    }
    assert_eq!(received.load(Ordering::SeqCst), 4);

    // Different swaps and independently constructed SDK clients must share the
    // transport budget. A hundred callers after an Unavailable response must
    // all time out waiting locally, without hitting the fake server.
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..100 {
        let client = if n % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        tasks.spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(1),
                client.list_vtxos(GetVtxosRequest::new_for_outpoints(&[
                    bitcoin::OutPoint::null(),
                ])),
            )
            .await
            .is_err()
        });
    }
    while let Some(result) = tasks.join_next().await {
        assert!(
            result.unwrap(),
            "call must expire locally during global backoff"
        );
    }
    assert_eq!(received.load(Ordering::SeqCst), 4);
    assert_eq!(ark_grpc::vtxo_requests_started(), before + 4);
    server.abort();
}
