//! The entries page is the signed-in player's own: their money goes to them alone, and no
//! cache keeps it.

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr::Keys;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

use super::leaderboard_tests::Coordinator;
use crate::api::extractors::create_auth_event;

/// A GET of `path` as htmx sends it, signed by `keys` when given, and the answer.
async fn get(
    coordinator: &Coordinator,
    path: &str,
    htmx: bool,
    keys: Option<&Keys>,
) -> (StatusCode, String, String) {
    let mut request = Request::builder().uri(path);
    if htmx {
        request = request.header("HX-Request", "true");
    }
    if let Some(keys) = keys {
        // The default origins name the UI served on port 9990.
        let url = format!("http://localhost:9990{path}");
        let event = create_auth_event("GET", &url, None, keys).await;
        request = request.header(
            header::AUTHORIZATION,
            format!(
                "Nostr {}",
                BASE64.encode(serde_json::to_vec(&event).unwrap())
            ),
        );
    }
    let response = coordinator
        .router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let caching = response
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, caching, String::from_utf8(body.to_vec()).unwrap())
}

/// Paid entries by `pubkey` in `competition`, each settled and, every other one, paid out.
async fn entries(coordinator: &Coordinator, competition: Uuid, pubkey: &str, count: usize) {
    let (event, pubkey) = (competition.to_string(), pubkey.to_owned());
    coordinator.databases[0]
        .execute_write(move |pool| async move {
            for index in 0..count {
                let (entry, ticket) = (Uuid::now_v7().to_string(), Uuid::now_v7().to_string());
                sqlx::query(
                    "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_at,
                         paid_at, settled_at, network_fee_sats)
                     VALUES (?, ?, 'preimage', ?, datetime('now'), datetime('now'),
                         datetime('now'), 40)",
                )
                .bind(&ticket)
                .bind(&event)
                .bind(format!("hash-{ticket}"))
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey,
                         payout_hash, entry_submission)
                     VALUES (?, ?, ?, ?, ?, ?, '{}')",
                )
                .bind(&entry)
                .bind(&event)
                .bind(&ticket)
                .bind(&pubkey)
                .bind(format!("pubkey-{entry}"))
                .bind(format!("hash-{entry}"))
                .execute(&pool)
                .await?;
                if index % 2 == 0 {
                    sqlx::query(
                        "INSERT INTO payouts (id, entry_id, payout_payment_request,
                             payout_amount_sats, initiated_at, succeed_at)
                         VALUES (?, ?, 'lnbc1', 9000, '2026-09-03T10:00:00Z',
                             '2026-09-03T10:00:05Z')",
                    )
                    .bind(Uuid::now_v7().to_string())
                    .bind(&entry)
                    .execute(&pool)
                    .await?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn only_the_signed_in_owner_gets_their_ledger() {
    let coordinator = Coordinator::start("http://127.0.0.1:9".into()).await;
    let competition = coordinator
        .competition(
            OffsetDateTime::now_utc() - time::Duration::days(2),
            &["KPWM"],
            3,
        )
        .await;
    let owner = Keys::generate();
    entries(
        &coordinator,
        competition.id,
        &owner.public_key().to_hex(),
        1,
    )
    .await;
    let ledger = coordinator
        .state
        .coordinator
        .player_ledger(&owner.public_key().to_hex())
        .await
        .unwrap();
    let entry = &ledger[0].entry_id;
    let short = &entry[entry.len() - 8..];

    // Signed out: the log-in prompt, with no ledger and no entries, whether htmx asks (401,
    // which it doesn't swap) or the address is opened; and never cached.
    for (path, htmx, status) in [
        ("/entries", true, StatusCode::UNAUTHORIZED),
        ("/entries", false, StatusCode::OK),
        ("/entries?from=25", true, StatusCode::UNAUTHORIZED),
    ] {
        let (answered, caching, body) = get(&coordinator, path, htmx, None).await;
        assert_eq!(answered, status, "{path}");
        assert_eq!(caching, "private, no-store, no-transform", "{path}");
        assert!(body.contains("Log in to see your entries"), "{path}");
        assert!(
            !body.contains("ledgerSummary") && !body.contains(short),
            "{path}"
        );
    }

    // The owner, signed in: their ledger. 6,000 sats, the 5% service fee and the network fee,
    // all in; the payout settled.
    let (status, caching, body) = get(&coordinator, "/entries", true, Some(&owner)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(caching, "private, no-store, no-transform");
    assert!(body.contains(r#"id="ledgerSummary""#));
    assert!(body.contains("Paid <strong>6,340 sats</strong> across 1 entry"));
    assert!(body.contains("Received <strong>9,000 sats</strong> (won 9,000 sats"));
    assert!(body.contains(short));

    // Anyone else, signed in, sees their own entries: none.
    let (status, _, body) = get(&coordinator, "/entries", true, Some(&Keys::generate())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("ledgerSummary") && !body.contains(short));
    assert!(body.contains("You haven't entered a competition yet."));

    coordinator.stop().await;
}

/// Not a check: prints how long the entries page takes, warm, for a player with 200 entries
/// over 50 competitions, and the next page of rows.
///
/// `cargo test -p coordinator entries_timings -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "prints timings; run by hand"]
async fn entries_timings() {
    const RUNS: usize = 50;
    let coordinator = Coordinator::start("http://127.0.0.1:9".into()).await;
    let player = Keys::generate();
    let pubkey = player.public_key().to_hex();
    for days in 0..50 {
        let start = OffsetDateTime::now_utc() - time::Duration::days(days + 1);
        let competition = coordinator.competition(start, &["KPWM"], 4).await;
        entries(&coordinator, competition.id, &pubkey, 4).await;
    }
    let (_, _, body) = get(&coordinator, "/entries", true, Some(&player)).await;
    assert!(body.contains("across 200 entries"), "{body}");
    assert!(body.contains("Show older entries (175 more)"));

    for path in ["/entries", "/entries?from=25"] {
        let mut times = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = std::time::Instant::now();
            let (status, _, _) = get(&coordinator, path, true, Some(&player)).await;
            times.push(started.elapsed());
            assert_eq!(status, StatusCode::OK);
        }
        times.sort();
        println!(
            "warm {path}: median {:?}, p95 {:?}, max {:?} over {RUNS}",
            times[RUNS / 2],
            times[RUNS * 95 / 100],
            times[RUNS - 1],
        );
    }
    coordinator.stop().await;
}
