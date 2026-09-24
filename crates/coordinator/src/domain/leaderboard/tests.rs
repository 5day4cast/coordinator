use super::*;
use crate::{
    domain::competitions::CreateEvent,
    infra::{
        oracle::{AddEventEntry, WeatherChoices},
        oracle_weather::Reading,
    },
};
use ValueOptions::{Over, Par, Under};

fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap()
}

fn competition() -> Competition {
    Competition::new(&CreateEvent {
        id: Uuid::now_v7(),
        signing_date: start() + time::Duration::days(2),
        start_observation_date: start(),
        end_observation_date: start() + time::Duration::hours(18),
        locations: vec!["KPWM".into(), "KBTV".into()],
        number_of_values_per_entry: 3,
        number_of_places_win: 1,
        total_allowed_entries: 3,
        entry_fee: 6_000,
        coordinator_fee_percentage: 5,
        total_competition_pool: 18_000,
        relative_locktime_block_delta: None,
    })
}

fn entry(event_id: Uuid, pubkey: &str, picks: Vec<WeatherChoices>) -> UserEntry {
    let id = Uuid::now_v7();
    UserEntry {
        id,
        event_id,
        ticket_id: Uuid::now_v7(),
        pubkey: pubkey.into(),
        ephemeral_pubkey: String::new(),
        payout_hash: String::new(),
        entry_submission: AddEventEntry {
            id,
            event_id,
            expected_observations: picks,
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
        paid_at: Some(start()),
        sellback_broadcasted_at: None,
        reclaimed_broadcasted_at: None,
        paid_out_at: None,
        payout_ln_invoice: None,
    }
}

fn picks(
    station: &str,
    temp_high: Option<ValueOptions>,
    temp_low: Option<ValueOptions>,
    wind_speed: Option<ValueOptions>,
) -> WeatherChoices {
    WeatherChoices {
        stations: station.into(),
        temp_high,
        temp_low,
        wind_speed,
    }
}

fn reading(target: &str, metric: &str, baseline: f64, observed: Option<f64>) -> Reading {
    Reading {
        target: target.into(),
        metric: metric.into(),
        baseline: Some(baseline),
        observed,
    }
}

/// KPWM forecast: high 66, low 42, wind 9 kt. Observed three hours in: high 60.4, low 41, wind 12.
fn weather(
    scores: HashMap<Uuid, u64>,
    entries: usize,
    attested: bool,
) -> Fetched<CompetitionWeather> {
    Fetched::new(CompetitionWeather {
        event: EventReadings {
            readings: vec![
                reading("KPWM", "temp_high", 66.0, Some(59.0)),
                reading("KPWM", "temp_low", 42.0, Some(41.0)),
                reading("KPWM", "wind_speed", 9.0, Some(12.0)),
            ],
            entry_count: entries,
            scores,
            attested,
        },
        observations: Some(vec![StationObservations {
            station_id: "KPWM".into(),
            first_report: Some(start() + time::Duration::minutes(45)),
            latest_report: Some(start() + time::Duration::hours(3)),
            temp_high: Some(60.4),
            temp_low: Some(41.0),
            wind_speed: Some(12.0),
        }]),
        early_forecasts: vec![],
    })
}

fn names() -> HashMap<String, String> {
    HashMap::from([("aa".repeat(32), "freya".to_owned())])
}

#[test]
fn a_live_board_ranks_by_what_would_score_now() {
    let competition = competition();
    let alice = entry(
        competition.id,
        &"aa".repeat(32),
        vec![picks("KPWM", Some(Under), Some(Under), Some(Over))],
    );
    let bob = entry(
        competition.id,
        &"bb".repeat(32),
        vec![picks("KPWM", Some(Over), Some(Par), Some(Under))],
    );
    let now = start() + time::Duration::hours(4);
    let board = build(
        &competition,
        &[bob.clone(), alice.clone()],
        Some(&weather(HashMap::new(), 2, false)),
        &names(),
        now,
    );

    assert_eq!(board.phase, Phase::Live);
    assert!(board.provisional);
    assert_eq!(board.rows.len(), 2);
    let first = &board.rows[0];
    assert_eq!(first.entry_id, alice.id);
    assert_eq!(first.player.name, "freya");
    assert_eq!(first.place, 1);
    // Under the high (60 < 66), under the low (41 < 42), over the wind (12 > 9).
    assert_eq!(first.standing.score, 30);
    assert_eq!(first.standing.rank, 1);
    assert!(first.final_standing.is_none());
    assert_eq!(first.prize_sats, Some(18_000));
    let states: Vec<PickState> = first.picks.iter().map(|pick| pick.state).collect();
    assert_eq!(
        states,
        vec![PickState::OnTrack, PickState::LockedIn, PickState::LockedIn]
    );
    let high = &first.picks[0];
    assert_eq!(high.forecast, Some(66.0));
    assert_eq!(
        high.observed,
        Some(60.4),
        "the latest observations, not the oracle's hourly reading"
    );
    assert_eq!(high.unit, "°F");
    assert_eq!(high.hours_covered, 3.0);
    assert_eq!(high.hours_total, 18.0);
    assert_eq!(high.updated_at, Some(start() + time::Duration::hours(3)));

    let second = &board.rows[1];
    assert_eq!(
        second.player.name, "bbbbbbbb",
        "no username: the key's start"
    );
    assert_eq!(second.standing.score, 0);
    assert_eq!(second.standing.rank, 2);
    assert_eq!(second.prize_sats, Some(0));
    let states: Vec<PickState> = second.picks.iter().map(|pick| pick.state).collect();
    assert_eq!(
        states,
        vec![PickState::OffTrack, PickState::Out, PickState::Out]
    );
    assert_eq!(
        board.observed_until,
        Some(start() + time::Duration::hours(3))
    );
}

#[test]
fn picks_stay_hidden_until_the_window_opens() {
    let competition = competition();
    let alice = entry(
        competition.id,
        &"aa".repeat(32),
        vec![picks("KPWM", Some(Over), None, None)],
    );
    let board = build(
        &competition,
        &[alice],
        Some(&weather(HashMap::new(), 0, false)),
        &names(),
        start() - time::Duration::minutes(5),
    );
    assert_eq!(board.phase, Phase::Upcoming);
    assert!(board.rows[0].picks.is_empty());
    assert_eq!(board.rows[0].prize_sats, None);
}

#[test]
fn equal_scores_share_a_rank_and_the_earlier_entry_is_paid() {
    let competition = competition();
    let same = vec![picks("KPWM", None, None, Some(Over))];
    let early = entry(competition.id, &"aa".repeat(32), same.clone());
    let late = entry(competition.id, &"bb".repeat(32), same);
    let board = build(
        &competition,
        &[late.clone(), early.clone()],
        Some(&weather(HashMap::new(), 2, false)),
        &names(),
        start() + time::Duration::hours(4),
    );
    let ids: Vec<Uuid> = board.rows.iter().map(|row| row.entry_id).collect();
    assert_eq!(ids, vec![early.id, late.id]);
    assert!(board
        .rows
        .iter()
        .all(|row| row.standing.rank == 1 && row.standing.tied));
    assert_eq!(board.rows[0].prize_sats, Some(18_000));
    assert_eq!(board.rows[1].prize_sats, Some(0));
}

#[test]
fn once_scored_the_oracle_decides_and_picks_are_final() {
    let competition = competition();
    let alice = entry(
        competition.id,
        &"aa".repeat(32),
        vec![picks("KPWM", Some(Under), None, None)],
    );
    let bob = entry(
        competition.id,
        &"bb".repeat(32),
        vec![picks("KPWM", Some(Over), None, None)],
    );
    // The oracle's scores stand even where the latest observations would say otherwise.
    let scores = HashMap::from([
        (alice.entry_submission.id, 10),
        (bob.entry_submission.id, 20),
    ]);
    let board = build(
        &competition,
        &[alice.clone(), bob.clone()],
        Some(&weather(scores, 2, true)),
        &names(),
        start() + time::Duration::days(1),
    );
    assert_eq!(board.phase, Phase::Scored);
    assert!(!board.provisional);
    assert_eq!(board.rows[0].entry_id, bob.id);
    assert_eq!(board.rows[0].final_standing.map(|s| s.score), Some(20));
    assert_eq!(board.rows[0].standing.score, 20);
    let pick = &board.rows[1].picks[0];
    assert_eq!(pick.state, PickState::Final);
    assert_eq!(
        pick.observed,
        Some(59.0),
        "the observation the oracle scored with"
    );
    assert!(pick.hit);
}

#[test]
fn without_weather_the_board_still_lists_entries() {
    let competition = competition();
    let alice = entry(
        competition.id,
        &"aa".repeat(32),
        vec![picks("KPWM", Some(Over), None, None)],
    );
    let board = build(
        &competition,
        &[alice],
        None,
        &names(),
        start() + time::Duration::hours(1),
    );
    assert_eq!(board.rows.len(), 1);
    assert_eq!(board.rows[0].picks[0].state, PickState::Pending);
    assert!(board.weather_fetched_at.is_none());
    assert!(board.pot.refund_all, "nobody has scored yet");
}

/// The oracle rescores entries on every run once the window opens; only its attestation is final.
#[test]
fn running_oracle_scores_are_not_final() {
    let competition = competition();
    let alice = entry(
        competition.id,
        &"aa".repeat(32),
        vec![picks("KPWM", Some(Over), None, None)],
    );
    let scores = HashMap::from([(alice.entry_submission.id, 10)]);
    let live = build(
        &competition,
        std::slice::from_ref(&alice),
        Some(&weather(scores.clone(), 1, false)),
        &names(),
        start() + time::Duration::hours(4),
    );
    assert_eq!(live.phase, Phase::Live);
    assert!(live.provisional);
    assert!(live.rows[0].final_standing.is_none());
    assert_eq!(
        live.rows[0].standing.score, 0,
        "the high is not over 66 yet"
    );
    assert_eq!(live.rows[0].picks[0].state, PickState::OffTrack);

    // Closed but not attested: picks are final, the ranking is still provisional.
    let closed = build(
        &competition,
        &[alice],
        Some(&weather(scores, 1, false)),
        &names(),
        start() + time::Duration::hours(19),
    );
    assert_eq!(closed.phase, Phase::Closed);
    assert!(closed.provisional);
    assert_eq!(closed.rows[0].picks[0].state, PickState::Final);
    assert!(closed.rows[0].final_standing.is_none());
}

#[test]
fn a_missing_oracle_baseline_does_not_reuse_an_early_forecast() {
    let mut weather = weather(HashMap::new(), 1, false).value;
    weather.early_forecasts = vec![reading("KPWM", "temp_high", 66.0, None)];
    weather.event.readings[0].baseline = None;
    assert_eq!(weather.forecast("KPWM", Metric::TempHigh), None);

    // Only an event without an authoritative reading uses the early forecast.
    weather.event.readings.clear();
    assert_eq!(weather.forecast("KPWM", Metric::TempHigh), Some(66.0));
}

#[tokio::test]
async fn public_weather_pages_stay_under_budget_while_the_oracle_is_slow() {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        routing::get,
        Json, Router,
    };
    use std::time::Instant;
    use tower::ServiceExt;

    use crate::{
        config::{APISettings, RateLimitSettings, Settings},
        startup::{app, build_app},
    };

    let slow_oracle = Router::new()
        .route(
            "/stations",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Json(serde_json::json!([]))
            }),
        )
        .fallback(|| async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Json(serde_json::json!({
                "readings": [{"target": "KPWM", "metric": "temp_high", "baseline": 66}],
                "entries": [], "attestation": null
            }))
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oracle_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, slow_oracle).await.unwrap() });

    let data = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.db_settings.data_folder = data.path().display().to_string();
    settings.bitcoin_settings.mock_enabled = true;
    settings.ln_settings.mock_enabled = true;
    settings.coordinator_settings.mock_oracle = true;
    settings.coordinator_settings.oracle_url = oracle_url;
    let (state, tasks, cancel, databases) = build_app(settings).await.unwrap();
    let competition = state
        .coordinator
        .competition_store
        .add_competition_with_tickets(competition(), vec![])
        .await
        .unwrap();
    let state = Arc::new(state);
    let router = app(
        state.clone(),
        &APISettings {
            rate_limit: RateLimitSettings::disabled(),
            ..APISettings::default()
        },
    );

    for cache in ["cold", "warm"] {
        for suffix in ["leaderboard", "leaderboard/rows", "entry-form"] {
            let path = format!("/competitions/{}/{suffix}", competition.id);
            let started = Instant::now();
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&path)
                        .header("HX-Request", "true")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            to_bytes(response.into_body(), 1 << 20).await.unwrap();
            let elapsed = started.elapsed();
            println!("{cache} {suffix}: {elapsed:?}");
            assert!(
                elapsed < Duration::from_millis(400),
                "{cache} {path}: {elapsed:?}"
            );
        }
        // Both background fetches have finished by the second pass.
        if cache == "cold" {
            tokio::time::sleep(Duration::from_millis(2100)).await;
        }
    }

    cancel.cancel();
    for handle in state.background_threads.values() {
        handle.abort();
    }
    tasks.wait().await;
    for database in databases {
        database.close().await.unwrap();
    }
    server.abort();
}
