//! The leaderboard pages answer from the weather cache: they neither wait for a slow oracle nor
//! ask it again while what they have is fresh.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower::ServiceExt;
use uuid::Uuid;

use crate::{
    config::{APISettings, RateLimitSettings, Settings},
    domain::{leaderboard::Metric, Competition, CreateEvent, UserEntry},
    infra::{
        db::DBConnection,
        oracle::{AddEventEntry, ValueOptions, WeatherChoices},
    },
    startup::{app, build_app, AppState},
};

/// What the stand-in oracle answers.
struct Answers {
    event: Value,
    observations: Value,
    stations: Value,
}

/// An oracle whose event requests wait until the test opens `gate`, and are counted.
async fn gated_oracle(
    answers: Answers,
    gate: watch::Receiver<bool>,
    event_requests: Arc<AtomicUsize>,
) -> (String, tokio::task::JoinHandle<()>) {
    let Answers {
        event,
        observations,
        stations,
    } = answers;
    let oracle = Router::new()
        .route("/stations", get(move || async move { Json(stations) }))
        .route(
            "/stations/observations",
            get(move || async move { Json(observations) }),
        )
        .route(
            "/oracle/events/{id}",
            get(move || {
                let mut gate = gate.clone();
                let event_requests = event_requests.clone();
                let event = event.clone();
                async move {
                    event_requests.fetch_add(1, Ordering::SeqCst);
                    let _ = gate.wait_for(|open| *open).await;
                    Json(event)
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, oracle).await.unwrap() });
    (url, server)
}

/// The coordinator's app, with mock Bitcoin, Lightning and oracle contract calls, reading
/// weather from `oracle_url`.
struct Coordinator {
    state: Arc<AppState>,
    router: Router,
    tasks: TaskTracker,
    cancel: CancellationToken,
    databases: Vec<DBConnection>,
    _data: tempfile::TempDir,
}

impl Coordinator {
    async fn start(oracle_url: String) -> Self {
        let data = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.db_settings.data_folder = data.path().display().to_string();
        settings.bitcoin_settings.mock_enabled = true;
        settings.ln_settings.mock_enabled = true;
        settings.coordinator_settings.mock_oracle = true;
        settings.coordinator_settings.oracle_url = oracle_url;
        let (state, tasks, cancel, databases) = build_app(settings).await.unwrap();
        let state = Arc::new(state);
        let router = app(
            state.clone(),
            &APISettings {
                rate_limit: RateLimitSettings::disabled(),
                ..APISettings::default()
            },
        );
        Self {
            state,
            router,
            tasks,
            cancel,
            databases,
            _data: data,
        }
    }

    /// A competition over `stations` whose window opens at `start`.
    async fn competition(
        &self,
        start: OffsetDateTime,
        stations: &[&str],
        entries: usize,
    ) -> Competition {
        let competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + time::Duration::days(2),
            start_observation_date: start,
            end_observation_date: start + time::Duration::hours(18),
            locations: stations.iter().map(|station| station.to_string()).collect(),
            number_of_values_per_entry: stations.len() * 3,
            number_of_places_win: 1,
            total_allowed_entries: entries,
            entry_fee: 6_000,
            coordinator_fee_percentage: 5,
            total_competition_pool: 6_000 * entries,
            relative_locktime_block_delta: None,
        });
        self.state
            .coordinator
            .competition_store
            .add_competition_with_tickets(competition, vec![])
            .await
            .unwrap()
    }

    /// A paid entry picking every metric at every one of `stations`.
    async fn paid_entry(&self, competition: &Competition, stations: &[&str]) -> Uuid {
        let (id, ticket_id) = (Uuid::now_v7(), Uuid::now_v7());
        let (event_id, ticket) = (competition.id.to_string(), ticket_id.to_string());
        self.databases[0]
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_at, \
                     paid_at, settled_at) VALUES (?, ?, 'preimage', ?, datetime('now'), \
                     datetime('now'), datetime('now'))",
                )
                .bind(&ticket)
                .bind(event_id)
                .bind(format!("hash-{ticket}"))
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
        let picks = [ValueOptions::Over, ValueOptions::Par, ValueOptions::Under];
        let choices = stations
            .iter()
            .enumerate()
            .map(|(index, station)| WeatherChoices {
                stations: station.to_string(),
                temp_high: Some(picks[index % 3].clone()),
                temp_low: Some(picks[(index + 1) % 3].clone()),
                wind_speed: Some(picks[(index + 2) % 3].clone()),
            })
            .collect();
        let pubkey = nostr::Keys::generate().public_key().to_hex();
        let entry = UserEntry {
            id,
            event_id: competition.id,
            ticket_id,
            pubkey,
            ephemeral_pubkey: String::new(),
            payout_hash: String::new(),
            entry_submission: AddEventEntry {
                id,
                event_id: competition.id,
                expected_observations: choices,
            },
            ephemeral_privatekey: None,
            payout_preimage: None,
            encrypted_keymeld_private_key: None,
            keymeld_auth_pubkey: None,
            keymeld_registration_context: None,
            keymeld_escrow_policy: None,
            public_nonces: None,
            funding_psbt_base64: None,
            partial_signatures: None,
            signed_at: None,
            paid_at: None,
            sellback_broadcasted_at: None,
            reclaimed_broadcasted_at: None,
            paid_out_at: None,
            payout_ln_invoice: None,
        };
        self.state
            .coordinator
            .competition_store
            .add_entry(entry, ticket_id)
            .await
            .unwrap();
        id
    }

    async fn get(&self, path: &str) -> String {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("HX-Request", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    async fn stop(self) {
        self.cancel.cancel();
        for handle in self.state.background_threads.values() {
            handle.abort();
        }
        self.tasks.wait().await;
        for database in self.databases {
            database.close().await.unwrap();
        }
    }
}

/// Wait until the oracle has had `count` requests, for at most 30 s.
async fn requests_reach(requests: &AtomicUsize, count: usize) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while requests.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the oracle was asked");
}

#[tokio::test]
async fn leaderboard_pages_answer_from_the_cache() {
    let (open_gate, gate) = watch::channel(false);
    let event_requests = Arc::new(AtomicUsize::new(0));
    let answers = Answers {
        event: json!({
            "readings": [{"target": "KPWM", "metric": "temp_high", "baseline": 66}],
            "entries": [],
            "attestation": null
        }),
        observations: json!([]),
        stations: json!([]),
    };
    let (oracle_url, oracle) = gated_oracle(answers, gate, event_requests.clone()).await;
    let coordinator = Coordinator::start(oracle_url).await;
    let start = OffsetDateTime::now_utc() + time::Duration::hours(2);
    let competition = coordinator.competition(start, &["KPWM"], 3).await;
    let pages = [
        format!("/competitions/{}/leaderboard", competition.id),
        format!("/competitions/{}/leaderboard/rows", competition.id),
    ];

    // Cold: the pages answer while the oracle still holds its answer back, and the fetch they
    // started is shared rather than repeated.
    for path in &pages {
        coordinator.get(path).await;
    }
    assert!(!*open_gate.borrow(), "answered without the oracle");
    // However slow the machine, the one fetch reaches the oracle; no second one follows.
    requests_reach(&event_requests, 1).await;
    for path in &pages {
        coordinator.get(path).await;
    }
    assert_eq!(event_requests.load(Ordering::SeqCst), 1);
    let leaderboards = &coordinator.state.leaderboards;
    let loading = leaderboards.weather(&competition, Duration::ZERO).await;
    assert!(loading.latest.is_none() && loading.refreshing);

    // Once the oracle answers, the cache holds its weather. The wait ends when the fetch does;
    // its bound is generous only for a loaded machine.
    open_gate.send_replace(true);
    let filled = leaderboards
        .weather(&competition, Duration::from_secs(30))
        .await;
    let weather = filled.value().expect("the fetch finished");
    assert_eq!(weather.forecast("KPWM", Metric::TempHigh), Some(66.0));

    // Warm: the pages are served from the cache; the oracle is not asked again.
    for _ in 0..3 {
        for path in &pages {
            coordinator.get(path).await;
        }
    }
    assert_eq!(event_requests.load(Ordering::SeqCst), 1);

    coordinator.stop().await;
    oracle.abort();
}

/// Not a check: prints how long the leaderboard pages take, cold and warm, for the PR's
/// timings. 50 paid entries with three stations each, in an open window.
///
/// `cargo test -p coordinator leaderboard_timings -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "prints timings; run by hand"]
async fn leaderboard_timings() {
    const ENTRIES: usize = 50;
    const RUNS: usize = 50;
    let stations = ["KPWM", "KBTV", "KBOS"];
    let now = OffsetDateTime::now_utc();
    let readings: Vec<Value> = stations
        .iter()
        .flat_map(|station| {
            [
                ("temp_high", 66.0, 64.0),
                ("temp_low", 42.0, 43.0),
                ("wind_speed", 9.0, 12.0),
            ]
            .map(|(metric, baseline, observed)| {
                json!({"target": station, "metric": metric, "baseline": baseline, "observed": observed})
            })
        })
        .collect();
    let latest = (now - time::Duration::minutes(20))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let answers = Answers {
        event: json!({"readings": readings, "entries": [], "attestation": null}),
        observations: Value::Array(
            stations
                .iter()
                .map(|station| {
                    json!({"station_id": station, "end_time": latest, "temp_high": 64.4,
                           "temp_low": 42.6, "wind_speed": 12.0})
                })
                .collect(),
        ),
        stations: Value::Array(
            stations
                .iter()
                .map(|station| {
                    json!({"station_id": station, "station_name": station,
                           "latitude": 43.6, "longitude": -70.3})
                })
                .collect(),
        ),
    };
    let (open_gate, gate) = watch::channel(false);
    let event_requests = Arc::new(AtomicUsize::new(0));
    let (oracle_url, oracle) = gated_oracle(answers, gate, event_requests.clone()).await;
    let coordinator = Coordinator::start(oracle_url).await;
    let competition = coordinator
        .competition(now - time::Duration::hours(3), &stations, ENTRIES)
        .await;
    let mut entries = vec![];
    for _ in 0..ENTRIES {
        entries.push(coordinator.paid_entry(&competition, &stations).await);
    }
    let pages = [
        (
            "leaderboard",
            format!("/competitions/{}/leaderboard", competition.id),
        ),
        (
            "leaderboard rows",
            format!("/competitions/{}/leaderboard/rows", competition.id),
        ),
        ("entry picks", format!("/entries/{}/detail", entries[0])),
    ];

    // Cold: nothing cached and the oracle holds its answer back.
    for (name, path) in &pages {
        let started = Instant::now();
        coordinator.get(path).await;
        println!("cold {name}: {:?}", started.elapsed());
    }

    open_gate.send_replace(true);
    let board = coordinator
        .state
        .leaderboards
        .get(&competition, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(board.rows.len(), ENTRIES);
    assert_eq!(board.rows[0].picks.len(), stations.len() * 3);
    assert!(board.weather_fetched_at.is_some());

    // Warm: served from the cache.
    for (name, path) in &pages {
        let mut times: Vec<Duration> = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = Instant::now();
            coordinator.get(path).await;
            times.push(started.elapsed());
        }
        times.sort();
        println!(
            "warm {name}: median {:?}, p95 {:?}, max {:?} over {RUNS}",
            times[RUNS / 2],
            times[RUNS * 95 / 100],
            times[RUNS - 1],
        );
    }
    assert_eq!(event_requests.load(Ordering::SeqCst), 1);

    coordinator.stop().await;
    oracle.abort();
}
