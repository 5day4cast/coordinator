//! Compare the SDK dispatch counter with requests received by a local gRPC server.

use ark_core::server::GetVtxosRequest;
use axum::{routing::post, Router};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

/// The SDK's dispatch counter is process-wide, so the tests that send GetVtxos
/// run one at a time.
static SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(Default::default);

// These tests compare a process-wide dispatch counter with local servers.
static INDEXER_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn actual_rpc_traffic_is_counted_paced_and_blocked_after_failure() {
    let _isolated_counter = INDEXER_TEST.lock().await;
    let _serial = SERIAL.lock().await;
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

    shared_quota_bounds_actual_requests().await;
}

/// Three distinct transport limiters represent the three production processes.
/// One reserves directly in SQLite; the others use the production HTTP client.
async fn shared_quota_bounds_actual_requests() {
    use crate::indexer_budget::{BACKGROUND_REQUESTS, REQUESTS};
    let background = BACKGROUND_REQUESTS as usize;
    let total = REQUESTS as usize;
    let directory = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(
        &directory.path().join("quota.sqlite"),
        crate::preimages::PreimageKey::from_wallet_secret(&[1; 32]),
    )
    .await
    .unwrap();
    let shared = store.clone();
    let received = Arc::new(AtomicUsize::new(0));
    let counted = received.clone();
    let app = Router::new()
        .route(
            "/v1/indexer/permit",
            post(
                move |axum::extract::Query(request): axum::extract::Query<
                    crate::api::IndexerPermit,
                >| {
                    let store = shared.clone();
                    async move { crate::api::reserve_indexer(&store, request).await }
                },
            ),
        )
        .route(
            "/ark.v1.IndexerService/GetVtxos",
            post(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        [("content-type", "application/grpc"), ("grpc-status", "0")],
                        vec![0u8; 5],
                    )
                }
            }),
        );
    let mut servers = Vec::new();
    let mut urls = Vec::new();
    for _ in 0..3 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://{}", listener.local_addr().unwrap()));
        let app = app.clone();
        servers.push(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
    }
    ark_grpc::set_vtxo_request_budget(&urls[0], Arc::new(store)).await;
    let mut clients = Vec::new();
    for (index, url) in urls.iter().enumerate() {
        if index > 0 {
            coordinator_ark::indexer_budget::configure(url, &urls[0], "test-token")
                .await
                .unwrap();
        }
        let mut client = ark_grpc::Client::new(url.clone());
        client.connect().await.unwrap();
        clients.push(client);
    }
    let before = ark_grpc::vtxo_requests_started();
    for index in 0..background {
        clients[index % clients.len()]
            .list_vtxos(GetVtxosRequest::new_for_outpoints(&[
                bitcoin::OutPoint::null(),
            ]))
            .await
            .unwrap();
    }
    assert!(clients[1]
        .list_vtxos(GetVtxosRequest::new_for_outpoints(&[
            bitcoin::OutPoint::null(),
        ]))
        .await
        .is_err());
    assert_eq!(
        received.load(Ordering::SeqCst),
        background,
        "background work must leave the payment reserve untouched"
    );
    for index in 0..total - background {
        coordinator_ark::indexer_budget::payment(clients[index % clients.len()].list_vtxos(
            GetVtxosRequest::new_for_outpoints(&[bitcoin::OutPoint::null()]),
        ))
        .await
        .unwrap();
    }
    assert_eq!(received.load(Ordering::SeqCst), total);
    let mut burst = tokio::task::JoinSet::new();
    for index in 0..100 {
        let client = clients[index % clients.len()].clone();
        burst.spawn(coordinator_ark::indexer_budget::payment(async move {
            !matches!(
                tokio::time::timeout(
                    Duration::from_secs(3),
                    client.list_vtxos(GetVtxosRequest::new_for_outpoints(&[
                        bitcoin::OutPoint::null()
                    ]),)
                )
                .await,
                Ok(Ok(_))
            )
        }));
    }
    while let Some(result) = burst.join_next().await {
        assert!(
            result.unwrap(),
            "exhausted shared quota must block every caller"
        );
    }
    assert_eq!(received.load(Ordering::SeqCst), total);
    assert_eq!(ark_grpc::vtxo_requests_started(), before + total as u64);
    for server in servers {
        server.abort();
    }
}

/// A refusal of the request, or an answer that fails to decode, keeps the usual
/// spacing. Only trouble at the server (here Unavailable) starts the backoff.
#[tokio::test]
async fn only_trouble_at_the_server_starts_the_backoff() {
    let _isolated_counter = INDEXER_TEST.lock().await;
    let _serial = SERIAL.lock().await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let received = Arc::new(AtomicUsize::new(0));
    let counted = received.clone();
    let app = Router::new().route(
        "/ark.v1.IndexerService/GetVtxos",
        post(move || {
            let n = counted.fetch_add(1, Ordering::SeqCst);
            async move {
                // InvalidArgument; then one VTXO without an outpoint, which
                // fails to decode; then Unavailable.
                let (status, body) = match n {
                    0 => ("3", vec![0u8; 5]),
                    1 => ("0", vec![0, 0, 0, 0, 2, 0x0a, 0x00]),
                    _ => ("14", vec![0u8; 5]),
                };
                (
                    [
                        ("content-type", "application/grpc"),
                        ("grpc-status", status),
                    ],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut client = ark_grpc::Client::new(format!("http://{address}"));
    client.connect().await.unwrap();
    let request = || GetVtxosRequest::new_for_outpoints(&[bitcoin::OutPoint::null()]);
    for expected in 1..=3 {
        let started = Instant::now();
        assert!(client.list_vtxos(request()).await.is_err());
        assert_eq!(received.load(Ordering::SeqCst), expected);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an answer must not start a backoff"
        );
    }
    // The Unavailable answer did: the next call expires locally.
    assert!(
        tokio::time::timeout(Duration::from_secs(5), client.list_vtxos(request()))
            .await
            .is_err()
    );
    assert_eq!(received.load(Ordering::SeqCst), 3);
    server.abort();
}
