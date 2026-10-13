//! The leaderboard pages answer from the weather cache: they neither wait for a slow oracle nor
//! ask it again while what they have is fresh. Nor do they decode a signed contract, which no
//! page shows.

use std::{
    str::FromStr,
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
use bitcoin::{Amount, FeeRate, OutPoint, Transaction};
use dlctix::{
    hashlock,
    musig2::{self, CompactSignature},
    secp::{MaybeScalar, Scalar},
    AnchorParams, ContractParameters, ContractSignatures, EventLockingConditions, MarketMaker,
    Outcome, PayoutWeights, Player, SignedContract,
};
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
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
pub(super) struct Coordinator {
    pub(super) state: Arc<AppState>,
    pub(super) router: Router,
    tasks: TaskTracker,
    cancel: CancellationToken,
    pub(super) databases: Vec<DBConnection>,
    _data: tempfile::TempDir,
}

impl Coordinator {
    pub(super) async fn start(oracle_url: String) -> Self {
        Self::start_with(oracle_url, |_| {}).await
    }

    /// [`Self::start`], with `configure` changing the settings first.
    pub(super) async fn start_with(
        oracle_url: String,
        configure: impl FnOnce(&mut Settings),
    ) -> Self {
        let data = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        configure(&mut settings);
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
        )
        .unwrap();
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
    pub(super) async fn competition(
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
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(5),
            total_competition_pool: 6_000 * entries,
            relative_locktime_block_delta: None,
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
            contract_options: None,
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
        // Each entry's own key and payout hash: the store keeps both unique.
        let key = Scalar::from_slice(&hashlock::sha256(id.as_bytes())).unwrap();
        let entry = UserEntry {
            id,
            event_id: competition.id,
            ticket_id,
            pubkey,
            ephemeral_pubkey: hex::encode(key.base_point_mul().serialize()),
            payout_hash: hex::encode(hashlock::sha256(&key.serialize())),
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
            sweep_uneconomic_at: None,
            split_output_spent_at: None,
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

    pub(super) async fn stop(self) {
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

fn scalar(index: u64) -> Scalar {
    Scalar::from_slice(&hashlock::sha256(&index.to_be_bytes())).unwrap()
}

/// A pool's contract between `players` players paying `places` places, signed: every ranking of
/// the paid places is an outcome, one more shares the pot back among every entry, and so does the
/// expiry. One key makes its signatures, not the players: decoding a contract doesn't check
/// them. Returns its parameters, the signed contract as stored, and the oracle's attestation
/// secret for each outcome.
fn pool_contract(players: usize, places: usize) -> (ContractParameters, Value, Vec<Scalar>) {
    let outcomes = coordinator_escrow::oracle_statement::ranking_outcomes(players, places);
    let attestations: Vec<Scalar> = (0..outcomes.len() as u64)
        .map(|index| scalar(10_000 + index))
        .collect();
    let equal: PayoutWeights = (0..players).map(|player| (player, 1)).collect();
    let weights = crate::domain::get_percentage_weights(places);
    let params = ContractParameters {
        market_maker: MarketMaker {
            pubkey: scalar(5_000).base_point_mul(),
        },
        players: (0..players)
            .map(|index| Player {
                pubkey: scalar(index as u64).base_point_mul(),
                ticket_hash: hashlock::sha256(&[index as u8, 1]),
                payout_hash: hashlock::sha256(&[index as u8, 2]),
            })
            .collect(),
        event: EventLockingConditions {
            locking_points: attestations
                .iter()
                .map(|key| key.base_point_mul().into())
                .collect(),
            expiry: Some(1_900_000_000),
        },
        outcome_payouts: outcomes
            .iter()
            .enumerate()
            .map(|(index, winners)| {
                let payout = if winners.len() == players {
                    equal.clone()
                } else {
                    winners
                        .iter()
                        .copied()
                        .zip(weights.iter().copied())
                        .collect()
                };
                (Outcome::Attestation(index), payout)
            })
            .chain(std::iter::once((Outcome::Expiry, equal.clone())))
            .collect(),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value: Amount::from_sat(5_000 * players as u64),
        relative_locktime_block_delta: 2_880,
        anchor: Some(AnchorParams::default()),
        // The outcome that shares the pot back shares winners with every other outcome.
        outcome_bound_splits: true,
    };
    let signer = scalar(6_000);
    let signatures = ContractSignatures {
        expiry_tx_signature: Some(musig2::deterministic::sign_solo::<CompactSignature>(
            signer, b"expiry",
        )),
        outcome_tx_signatures: params
            .event
            .locking_points
            .iter()
            .enumerate()
            .map(|(index, point)| {
                (
                    index,
                    musig2::deterministic::adaptor::sign_solo(signer, index.to_be_bytes(), *point),
                )
            })
            .collect(),
        split_tx_signatures: params
            .all_win_conditions()
            .into_iter()
            .map(|condition| {
                (
                    condition,
                    musig2::deterministic::sign_solo::<CompactSignature>(
                        signer,
                        condition.to_string(),
                    ),
                )
            })
            .collect(),
    };
    let funding = OutPoint::from_str(&format!("{}:0", "07".repeat(32))).unwrap();
    let signed = json!({
        "signatures": signatures,
        "dlc": { "params": params, "funding_outpoint": funding },
    });
    (params, signed, attestations)
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap()
}

/// Writes `columns` of competition `id` in one statement, so its runner never sees half of
/// them, including what the store itself would not write.
async fn set_columns(coordinator: &Coordinator, id: Uuid, columns: Vec<(&'static str, String)>) {
    let sql = format!(
        "UPDATE competitions SET {} WHERE id = ?",
        columns
            .iter()
            .map(|(column, _)| format!("{column} = ?"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    coordinator.databases[0]
        .execute_write(move |pool| async move {
            // The column names are this file's literals; the values are bound.
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
            for (_, value) in columns {
                query = query.bind(value);
            }
            query.bind(id.to_string()).execute(&pool).await?;
            Ok(())
        })
        .await
        .unwrap();
}

/// `competition` as kicked off with `params` and `signed_contract` (stored as given): seats for
/// its entries alone, the contract signed and funded, and the window closed an hour ago, so it
/// waits for the oracle, unless `more` columns move it on. Its entries go in before this, while
/// it is still upcoming and has seats to spare, so no runner kicks it off itself.
async fn kicked_off(
    coordinator: &Coordinator,
    competition: &mut Competition,
    params: &ContractParameters,
    places: usize,
    signed_contract: String,
    more: Vec<(&'static str, String)>,
) {
    let now = OffsetDateTime::now_utc();
    let event = &mut competition.event_submission;
    event.start_observation_date = now - time::Duration::hours(19);
    event.end_observation_date = now - time::Duration::hours(1);
    event.signing_date = now + time::Duration::hours(1);
    event.total_allowed_entries = params.players.len();
    event.number_of_places_win = places;
    let at = rfc3339(now - time::Duration::hours(18));
    let mut columns = vec![
        ("event_submission", serde_json::to_string(&*event).unwrap()),
        (
            "event_announcement",
            serde_json::to_string(&params.event).unwrap(),
        ),
        (
            "contract_parameters",
            serde_json::to_string(params).unwrap(),
        ),
        ("signed_contract", signed_contract),
        ("escrow_funds_confirmed_at", at.clone()),
        ("event_created_at", at.clone()),
        ("entries_submitted_at", at.clone()),
        ("contracted_at", at.clone()),
        ("keymeld_keygen_completed_at", at.clone()),
        ("signed_at", at.clone()),
        ("funding_broadcasted_at", at.clone()),
        ("funding_confirmed_at", at.clone()),
        ("funding_settled_at", at.clone()),
        ("awaiting_attestation_at", at),
    ];
    columns.extend(more);
    set_columns(coordinator, competition.id, columns).await;
}

/// A started competition's pages are the ones visitors open most, and its signed contract is the
/// largest thing stored for it: decoding the contract rebuilds every outcome's transactions. No
/// page shows the contract, so none reads it. With a contract that cannot be decoded, every page
/// of a competition waiting for its result, then scored, still answers, in full.
#[tokio::test]
async fn competition_pages_do_not_decode_the_signed_contract() {
    let stations = ["KPWM", "KBTV", "KBOS"];
    let readings: Vec<Value> = stations
        .iter()
        .map(|station| json!({"target": station, "metric": "temp_high", "baseline": 66}))
        .collect();
    let answers = Answers {
        event: json!({"readings": readings, "entries": [], "attestation": null}),
        observations: json!([]),
        stations: json!([]),
    };
    let (_open, gate) = watch::channel(true);
    let (oracle_url, oracle) = gated_oracle(answers, gate, Arc::new(AtomicUsize::new(0))).await;
    let coordinator = Coordinator::start(oracle_url).await;
    let mut competition = coordinator
        .competition(
            OffsetDateTime::now_utc() + time::Duration::days(1),
            &stations,
            25,
        )
        .await;
    let mut entries = vec![];
    for _ in 0..3 {
        entries.push(coordinator.paid_entry(&competition, &stations).await);
    }
    let (params, _, attestations) = pool_contract(3, 1);
    kicked_off(
        &coordinator,
        &mut competition,
        &params,
        1,
        "a graph that must not be decoded".into(),
        vec![],
    )
    .await;
    let id = competition.id;
    let state = &coordinator.state;
    assert!(
        state.coordinator.get_competition(id).await.is_err(),
        "a full read decodes the contract"
    );
    let read = state
        .coordinator
        .get_competition_for_page(id)
        .await
        .unwrap();
    assert!(read.signed_contract.is_none());
    assert_eq!(read.contract_parameters.as_ref(), Some(&params));
    let weather = state
        .leaderboards
        .weather(&competition, Duration::from_secs(30))
        .await;
    assert!(weather.value().is_some(), "the oracle answered");

    let rows = coordinator
        .get(&format!("/competitions/{id}/leaderboard/rows"))
        .await;
    for entry in &entries {
        assert!(rows.contains(&entry.to_string()), "{rows}");
    }
    for path in [
        format!("/competitions/{id}/leaderboard"),
        format!("/competitions/{id}/entry-form"),
        format!("/competitions/{id}/entry-forecasts"),
        format!("/entries/{}/detail", entries[0]),
        format!("/help?competition={id}"),
        format!("/api/v1/competitions/{id}/payout-terms"),
    ] {
        coordinator.get(&path).await;
    }

    // Scored with the outcome that shares the pot back, which the lists tell from the contract's
    // parameters.
    let shared_back = MaybeScalar::from(*attestations.last().unwrap());
    set_columns(
        &coordinator,
        id,
        vec![("attestation", serde_json::to_string(&shared_back).unwrap())],
    )
    .await;
    let finished = coordinator.get("/competitions?show=finished").await;
    assert!(finished.contains("pot shared back"), "{finished}");
    let leaderboard = coordinator
        .get(&format!("/competitions/{id}/leaderboard"))
        .await;
    assert!(leaderboard.contains("Each entry's share"), "{leaderboard}");

    coordinator.stop().await;
    oracle.abort();
}

/// The one contract a page reads: whether an attested competition's pot went back by expiry is
/// told by comparing its outcome transaction with the contract's expiry transaction.
#[tokio::test]
async fn a_page_reads_the_contract_of_a_competition_whose_expiry_was_broadcast() {
    let coordinator = Coordinator::start("http://127.0.0.1:9".into()).await;
    let mut competition = coordinator
        .competition(
            OffsetDateTime::now_utc() + time::Duration::days(1),
            &["KPWM"],
            25,
        )
        .await;
    for _ in 0..3 {
        coordinator.paid_entry(&competition, &["KPWM"]).await;
    }
    let (params, signed, attestations) = pool_contract(3, 1);
    let contract: SignedContract = serde_json::from_value(signed.clone()).unwrap();
    let expiry: Transaction = contract.expiry_tx().unwrap();
    // Attested with the first outcome, which pays the first player alone, after the expiry,
    // which pays everyone, settled it. Finished, so no runner steps it.
    let first_wins = MaybeScalar::from(attestations[0]);
    let at = rfc3339(OffsetDateTime::now_utc());
    kicked_off(
        &coordinator,
        &mut competition,
        &params,
        1,
        signed.to_string(),
        vec![
            ("attestation", serde_json::to_string(&first_wins).unwrap()),
            ("expiry_broadcasted_at", at.clone()),
            (
                "outcome_transaction",
                serde_json::to_string(&expiry).unwrap(),
            ),
            ("outcome_broadcasted_at", at.clone()),
            ("completed_at", at),
        ],
    )
    .await;
    let coordinator_state = &coordinator.state.coordinator;
    let read = coordinator_state
        .get_competition_for_page(competition.id)
        .await
        .unwrap();
    assert!(read.signed_contract.is_some());
    assert!(read.settled_by_expiry());
    // Without the contract, the attested outcome would look settled.
    assert!(!coordinator_state
        .get_competition_detail(competition.id, false)
        .await
        .unwrap()
        .settled_by_expiry());

    let leaderboard = coordinator
        .get(&format!("/competitions/{}/leaderboard", competition.id))
        .await;
    assert!(leaderboard.contains("Each entry's share"), "{leaderboard}");
    assert!(!leaderboard.contains("Winner takes all"), "{leaderboard}");

    coordinator.stop().await;
}

/// Not a check: prints how long a started pool's pages take, and the two ways of reading the
/// pool, for the PR's timings. A pool as the queues start them: 20 entries of 9 picks over three
/// stations, paying two places, so its contract has 381 attested outcomes and 800 win
/// conditions.
///
/// `cargo test --release -p coordinator pool_pages_timings -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "prints timings; run by hand"]
async fn pool_pages_timings() {
    const ENTRIES: usize = 20;
    const RUNS: usize = 20;
    let stations = ["KPWM", "KBTV", "KBOS"];
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
    let answers = Answers {
        event: json!({"readings": readings, "entries": [], "attestation": null}),
        observations: json!([]),
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
    let (_open, gate) = watch::channel(true);
    let (oracle_url, oracle) = gated_oracle(answers, gate, Arc::new(AtomicUsize::new(0))).await;
    let coordinator = Coordinator::start(oracle_url).await;
    let mut competition = coordinator
        .competition(
            OffsetDateTime::now_utc() + time::Duration::days(1),
            &stations,
            ENTRIES + 5,
        )
        .await;
    let mut entries = vec![];
    for _ in 0..ENTRIES {
        entries.push(coordinator.paid_entry(&competition, &stations).await);
    }
    let (params, signed, _) = pool_contract(ENTRIES, 2);
    assert_eq!(params.outcome_payouts.len(), 382);
    assert_eq!(params.all_win_conditions().len(), 800);
    kicked_off(
        &coordinator,
        &mut competition,
        &params,
        2,
        signed.to_string(),
        vec![],
    )
    .await;
    let id = competition.id;
    let state = &coordinator.state;
    let weather = state
        .leaderboards
        .weather(&competition, Duration::from_secs(30))
        .await;
    assert!(weather.value().is_some(), "the oracle answered");

    fn summary(name: &str, mut times: Vec<Duration>) {
        times.sort();
        let runs = times.len();
        println!(
            "{name}: median {:?}, p95 {:?}, max {:?} over {runs}",
            times[runs / 2],
            times[runs * 95 / 100],
            times[runs - 1],
        );
    }
    let mut full = vec![];
    let mut page = vec![];
    for _ in 0..RUNS {
        let started = Instant::now();
        let read = state.coordinator.get_competition(id).await.unwrap();
        full.push(started.elapsed());
        assert!(read.signed_contract.is_some());
        let started = Instant::now();
        state
            .coordinator
            .get_competition_for_page(id)
            .await
            .unwrap();
        page.push(started.elapsed());
    }
    summary("full read (before)", full);
    summary("page read (after)", page);

    let pages = [
        ("leaderboard", format!("/competitions/{id}/leaderboard")),
        (
            "leaderboard rows",
            format!("/competitions/{id}/leaderboard/rows"),
        ),
        ("entry picks", format!("/entries/{}/detail", entries[0])),
        ("help", format!("/help?competition={id}")),
        (
            "payout terms",
            format!("/api/v1/competitions/{id}/payout-terms"),
        ),
    ];
    for (name, path) in &pages {
        let mut times = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = Instant::now();
            coordinator.get(path).await;
            times.push(started.elapsed());
        }
        summary(name, times);
    }

    coordinator.stop().await;
    oracle.abort();
}
