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
        let event = create_auth_event("GET", &url, None, keys).await.unwrap();
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
async fn entry_counts_are_private_and_count_only_the_authenticated_players_submissions() {
    let coordinator = Coordinator::start("http://127.0.0.1:9".into()).await;
    let competition = coordinator
        .competition(
            OffsetDateTime::now_utc() + time::Duration::days(1),
            &["KPWM"],
            10,
        )
        .await;
    let owner = Keys::generate();
    let other = Keys::generate();
    entries(
        &coordinator,
        competition.id,
        &owner.public_key().to_hex(),
        2,
    )
    .await;
    entries(
        &coordinator,
        competition.id,
        &other.public_key().to_hex(),
        1,
    )
    .await;

    let path = "/competitions/entry-counts";
    assert_eq!(
        get(&coordinator, path, false, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    for (keys, count) in [(&owner, 2), (&other, 1)] {
        let (status, caching, body) = get(&coordinator, path, false, Some(keys)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(caching, "private, no-store");
        let counts: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            counts,
            serde_json::json!({ competition.id.to_string(): count })
        );
    }
    let (_, _, body) = get(&coordinator, path, false, Some(&Keys::generate())).await;
    assert_eq!(body, "{}");

    // The form's Help link still exposes this competition's concrete payment terms.
    let path = format!("/help?open=advanced&competition={}", competition.id);
    let (status, _, body) = get(&coordinator, &path, false, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<details open>"));
    assert!(body.contains("This competition's payment terms"));
    assert!(body.contains("blocks (about"));
    assert!(body.contains("sat/vB"));
    coordinator.stop().await;
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

#[tokio::test]
async fn only_the_owner_sees_their_escrow_and_everyone_the_contract_funding() {
    use bitcoin::{
        absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Transaction, TxOut,
    };
    let coordinator = Coordinator::start_with("http://127.0.0.1:9".into(), |settings| {
        settings.bitcoin_settings.explorer_url = Some("https://mutinynet.com".into());
        settings.ark_settings.explorer_url = Some("https://ark.example/".into());
    })
    .await;
    let competition = coordinator
        .competition(
            OffsetDateTime::now_utc() - time::Duration::days(2),
            &["KPWM"],
            3,
        )
        .await;
    let owner = Keys::generate();
    let other = Keys::generate();
    entries(
        &coordinator,
        competition.id,
        &owner.public_key().to_hex(),
        1,
    )
    .await;
    entries(
        &coordinator,
        competition.id,
        &other.public_key().to_hex(),
        1,
    )
    .await;

    // The owner's entry fee, held in an Arkade escrow.
    let vtxo_txid = "e".repeat(64);
    let vtxo = format!("{vtxo_txid}:0");
    let (pubkey, outpoint) = (owner.public_key().to_hex(), vtxo.clone());
    coordinator.databases[0]
        .execute_write(move |pool| async move {
            sqlx::query(
                "INSERT INTO ticket_ark_escrows (ticket_id, ticket_hash, escrow_tap_tree,
                     escrow_address, vtxo_outpoint, vtxo_sats, funded_at)
                 SELECT t.id, t.hash, '', 'tark1escrow', ?, 6340, 1756720800
                 FROM tickets t JOIN entries e ON e.ticket_id = t.id WHERE e.pubkey = ?",
            )
            .bind(outpoint)
            .bind(pubkey)
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();

    let (status, _, body) = get(&coordinator, "/entries", true, Some(&owner)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("On-chain details"));
    assert!(body.contains(&format!(r#"data-copy="{vtxo}""#)));
    assert!(body.contains(&format!(r#"href="https://ark.example/tx/{vtxo_txid}""#)));

    // No one else sees it: not another player, not a visitor, not the public pages.
    let entry = coordinator
        .state
        .coordinator
        .player_ledger(&owner.public_key().to_hex())
        .await
        .unwrap()[0]
        .entry_id
        .clone();
    let leaderboard = format!("/competitions/{}/leaderboard", competition.id);
    for (path, keys) in [
        ("/entries".to_owned(), Some(&other)),
        ("/entries".to_owned(), None),
        ("/entries?from=1".to_owned(), Some(&other)),
        (format!("/entries/{entry}/detail"), Some(&other)),
        (format!("/entries/{entry}/detail"), None),
        (format!("/entries/{entry}/detail/mine"), Some(&other)),
        (leaderboard.clone(), None),
    ] {
        let (_, _, body) = get(&coordinator, &path, true, keys).await;
        assert!(!body.contains(&vtxo_txid), "{path} shows the escrow VTXO");
    }
    assert!(!get(&coordinator, &leaderboard, true, None)
        .await
        .2
        .contains("Contract funding"));

    // Kicked off: the contract's funding outpoint, public on the leaderboard and on the
    // entries page, linked to the chain's explorer; its amount isn't shown.
    let commitment = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![],
        output: vec![
            TxOut {
                value: Amount::from_sat(330),
                script_pubkey: ScriptBuf::new(),
            },
            TxOut {
                value: Amount::from_sat(12_680),
                script_pubkey: ScriptBuf::new(),
            },
        ],
    };
    let funding = OutPoint::new(commitment.compute_txid(), 1);
    let (event, outpoint, transaction) = (
        competition.id.to_string(),
        serde_json::to_string(&funding).unwrap(),
        serde_json::to_string(&commitment).unwrap(),
    );
    coordinator.databases[0]
        .execute_write(move |pool| async move {
            sqlx::query(
                "UPDATE competitions SET funding_outpoint = ?, funding_transaction = ?,
                     funding_broadcasted_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
            )
            .bind(outpoint)
            .bind(transaction)
            .bind(event)
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();
    let link = format!(
        r#"href="https://mutinynet.com/tx/{}""#,
        commitment.compute_txid()
    );
    let (_, _, body) = get(&coordinator, &leaderboard, true, None).await;
    assert!(body.contains("Contract funding"));
    assert!(body.contains(&format!(r#"data-copy="{funding}""#)));
    assert!(body.contains(&link));
    assert!(!body.contains("12,680"));
    for keys in [&owner, &other] {
        let (_, _, body) = get(&coordinator, "/entries", true, Some(keys)).await;
        assert!(body.contains("Contract funding") && body.contains(&link));
        assert!(!body.contains("12,680"));
    }

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
