use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::{
    domain::CreateEvent,
    infra::{
        oracle::{AddEventEntry, WeatherChoices},
        refresh_cache::Fetched,
    },
};
use ValueOptions::{Over, Par, Under};

fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap()
}

/// Three paid entries, so its window runs rather than waiting to be cancelled.
fn competition() -> Competition {
    let mut competition = Competition::new(&CreateEvent {
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
        unlisted: false,
    });
    competition.total_entries = 3;
    competition.total_paid_entries = 3;
    competition
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

fn cached(weather: CompetitionWeather) -> Cached<CompetitionWeather> {
    Cached {
        latest: Some(Arc::new(Fetched::new(weather))),
        refreshing: false,
    }
}

/// KPWM forecast: high 66, low 42, wind 9 kt. Observed three hours in: high 60.4, low 41, wind 12.
fn weather(scores: HashMap<Uuid, u64>, entries: usize, attested: bool) -> CompetitionWeather {
    CompetitionWeather {
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
            latest_report: Some(start() + time::Duration::hours(3)),
            temp_high: Some(60.4),
            temp_low: Some(41.0),
            wind_speed: Some(12.0),
        }]),
        early_forecasts: None,
    }
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
        &cached(weather(HashMap::new(), 2, false)),
        &names(),
        now,
    );

    assert_eq!(board.phase, Phase::Live);
    assert!(board.provisional);
    assert!(!board.refreshing);
    assert_eq!(board.rows.len(), 2);
    let first = &board.rows[0];
    assert_eq!(first.entry_id, alice.id);
    assert_eq!(first.player.name, "freya");
    // Under the high (60 < 66), under the low (41 < 42), over the wind (12 > 9).
    assert_eq!(first.standing.score, 30);
    assert_eq!(first.standing.rank, 1);
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
    assert_eq!(high.hours_covered, 3.0);
    assert_eq!(high.hours_total, 18.0);

    let second = &board.rows[1];
    assert_eq!(
        second.player.name, "bbbbbbbb",
        "no username: the key's start"
    );
    assert_eq!(second.standing.score, 0);
    assert_eq!(second.standing.rank, 2);
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
        &cached(weather(HashMap::new(), 0, false)),
        &names(),
        start() - time::Duration::minutes(5),
    );
    assert_eq!(board.phase, Phase::Upcoming);
    assert!(board.rows[0].picks.is_empty());
}

#[test]
fn equal_scores_share_a_rank_and_the_earlier_entry_comes_first() {
    let competition = competition();
    let same = vec![picks("KPWM", None, None, Some(Over))];
    let early = entry(competition.id, &"aa".repeat(32), same.clone());
    let late = entry(competition.id, &"bb".repeat(32), same);
    let board = build(
        &competition,
        &[late.clone(), early.clone()],
        &cached(weather(HashMap::new(), 2, false)),
        &names(),
        start() + time::Duration::hours(4),
    );
    let ids: Vec<Uuid> = board.rows.iter().map(|row| row.entry_id).collect();
    assert_eq!(ids, vec![early.id, late.id]);
    assert!(board.rows.iter().all(|row| row.standing.rank == 1));
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
        &cached(weather(scores, 2, true)),
        &names(),
        start() + time::Duration::days(1),
    );
    assert_eq!(board.phase, Phase::Scored);
    assert!(!board.provisional);
    assert_eq!(board.rows[0].entry_id, bob.id);
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
    let loading = Cached {
        latest: None,
        refreshing: true,
    };
    let board = build(
        &competition,
        &[alice],
        &loading,
        &names(),
        start() + time::Duration::hours(1),
    );
    assert_eq!(board.rows.len(), 1);
    assert_eq!(board.rows[0].picks[0].state, PickState::Pending);
    assert!(board.weather_fetched_at.is_none());
    assert!(board.refreshing, "still loading, not unavailable");
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
        &cached(weather(scores.clone(), 1, false)),
        &names(),
        start() + time::Duration::hours(4),
    );
    assert_eq!(live.phase, Phase::Live);
    assert!(live.provisional);
    assert_eq!(
        live.rows[0].standing.score, 0,
        "the high is not over 66 yet"
    );
    assert_eq!(live.rows[0].picks[0].state, PickState::OffTrack);

    // Closed but not attested: the ranking is still provisional, and so are the picks.
    let closed = build(
        &competition,
        &[alice],
        &cached(weather(scores, 1, false)),
        &names(),
        start() + time::Duration::hours(19),
    );
    assert_eq!(closed.phase, Phase::AwaitingResult);
    assert!(closed.provisional);
    assert_eq!(closed.rows[0].picks[0].state, PickState::AwaitingResult);
}

/// The coordinator can hold the attestation before the cached weather has the oracle's final
/// scores. Until it does, the rows are provisional, and the phase says so rather than Scored.
#[test]
fn scored_waits_for_the_attested_scores() {
    let mut competition = competition();
    competition.attestation = Some(dlctix::secp::MaybeScalar::Valid(dlctix::secp::Scalar::one()));
    let alice = entry(
        competition.id,
        &"aa".repeat(32),
        vec![picks("KPWM", Some(Under), None, None)],
    );
    let after = start() + time::Duration::days(1);
    let stale = build(
        &competition,
        std::slice::from_ref(&alice),
        &cached(weather(HashMap::new(), 1, false)),
        &names(),
        after,
    );
    assert_eq!(stale.phase, Phase::AwaitingResult);
    assert!(stale.provisional);
    assert_eq!(stale.rows[0].picks[0].state, PickState::AwaitingResult);

    let scores = HashMap::from([(alice.entry_submission.id, 10)]);
    let attested = build(
        &competition,
        &[alice],
        &cached(weather(scores, 1, true)),
        &names(),
        after,
    );
    assert_eq!(attested.phase, Phase::Scored);
    assert!(!attested.provisional);
    assert_eq!(attested.rows[0].picks[0].state, PickState::Final);
}

/// A competition cancelled mid-window says so, whatever the clock says.
#[test]
fn a_cancelled_competition_is_not_live() {
    let mut competition = competition();
    competition.cancelled_at = Some(start() + time::Duration::hours(1));
    let board = build(
        &competition,
        &[],
        &cached(weather(HashMap::new(), 0, false)),
        &names(),
        start() + time::Duration::hours(2),
    );
    assert_eq!(board.phase, Phase::Cancelled);
}

#[test]
fn a_missing_oracle_baseline_does_not_reuse_an_early_forecast() {
    let mut weather = weather(HashMap::new(), 1, false);
    weather.early_forecasts = Some(EarlyForecasts {
        readings: vec![reading("KPWM", "temp_high", 66.0, None)],
        computed_at: start(),
    });
    weather.event.readings[0].baseline = None;
    assert_eq!(weather.forecast("KPWM", Metric::TempHigh), None);

    // Only an event without an authoritative reading uses the early forecast.
    weather.event.readings.clear();
    assert_eq!(weather.forecast("KPWM", Metric::TempHigh), Some(66.0));
    assert!(!weather.forecasts_unavailable());
    weather.early_forecasts = None;
    assert!(weather.forecasts_unavailable());
}

#[test]
fn early_forecasts_are_recomputed_hourly_and_once_the_window_opens() {
    let computed = |at: OffsetDateTime| EarlyForecasts {
        readings: vec![],
        computed_at: at,
    };
    let minute = time::Duration::MINUTE;
    let before = computed(start() - 30 * minute);
    assert!(!before.due(start(), start() - 5 * minute));
    assert!(before.due(start(), start() + minute), "the window opened");
    assert!(before.due(start() + 2 * time::Duration::HOUR, start() + 31 * minute));
    let after = computed(start() + minute);
    assert!(!after.due(start(), start() + 50 * minute));
    assert!(after.due(start(), start() + 62 * minute));
}

/// A stand-in for the oracle's public API that counts each endpoint's requests.
struct FakeOracle {
    url: String,
    forecasts: Arc<AtomicUsize>,
    events: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl FakeOracle {
    /// `forecasts` answers `/stations/forecasts`: rows, or `None` for a server error.
    async fn start(forecasts: Option<serde_json::Value>) -> Self {
        use axum::{http::StatusCode, response::IntoResponse, routing::get, Json, Router};
        let forecast_calls = Arc::new(AtomicUsize::new(0));
        let event_calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/stations/forecasts",
                get({
                    let calls = forecast_calls.clone();
                    move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        match forecasts {
                            Some(rows) => Json(rows).into_response(),
                            None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                        }
                    }
                }),
            )
            .route(
                "/stations/observations",
                get(|| async { Json(serde_json::json!([])) }),
            )
            .route(
                "/oracle/events/{id}",
                get({
                    let calls = event_calls.clone();
                    move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // The coordinator creates the oracle's event once entries are in.
                        StatusCode::NOT_FOUND
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url,
            forecasts: forecast_calls,
            events: event_calls,
            server,
        }
    }
}

impl Drop for FakeOracle {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A competition whose window opens `hours` from now.
fn upcoming(hours: i64) -> Competition {
    let mut competition = competition();
    let start = OffsetDateTime::now_utc() + time::Duration::hours(hours);
    competition.event_submission.start_observation_date = start;
    competition.event_submission.end_observation_date = start + time::Duration::hours(18);
    competition
}

/// A forecast failure is not data: it is retried within 30 s, not kept for ten minutes.
#[tokio::test]
async fn failed_forecasts_are_retried_soon() {
    let oracle = FakeOracle::start(None).await;
    let cache = WeatherCache::new(&oracle.url).unwrap();
    let competition = upcoming(2);
    let read = cache.weather(&competition, Duration::from_secs(5)).await;
    let weather = read.value().expect("the event and observations were read");
    assert!(weather.forecasts_unavailable());
    assert_eq!(weather.forecast("KPWM", Metric::TempHigh), None);
    assert_eq!(cache.weather_fetch(&competition).0, RETRY_AFTER);
    assert_eq!(oracle.forecasts.load(Ordering::SeqCst), 1);
}

/// Beyond the forecast horizon the oracle has no forecasts yet. That answer is kept like any
/// other, not asked for again on every refresh.
#[tokio::test]
async fn an_empty_forecast_answer_is_kept() {
    let oracle = FakeOracle::start(Some(serde_json::json!([]))).await;
    let cache = WeatherCache::new(&oracle.url).unwrap();
    let competition = upcoming(24 * 10);
    let read = cache.weather(&competition, Duration::from_secs(5)).await;
    let weather = read.value().unwrap();
    assert!(!weather.forecasts_unavailable());
    assert_eq!(weather.forecast("KPWM", Metric::TempHigh), None);
    assert_eq!(cache.weather_fetch(&competition).0, UPCOMING_TTL);

    // The next refresh reads the event again but keeps the forecasts it has.
    let (_, fetch) = cache.weather_fetch(&competition);
    let refreshed = fetch().await.unwrap();
    assert!(refreshed.early_forecasts.is_some());
    assert_eq!(oracle.events.load(Ordering::SeqCst), 2);
    assert_eq!(oracle.forecasts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn early_forecasts_fill_in_for_an_event_the_oracle_has_not_read_yet() {
    let today = OffsetDateTime::now_utc() + time::Duration::hours(2);
    let tomorrow = today + time::Duration::DAY;
    let rows: Vec<serde_json::Value> = [today, tomorrow]
        .into_iter()
        .flat_map(|day| {
            ["KPWM", "KBTV"].map(|station| {
                serde_json::json!({
                    "station_id": station,
                    "date": format!("{} 00:00:00", day.date()),
                    "temp_high": 70.0,
                    "temp_low": 50.0,
                    "wind_speed": 8.0,
                })
            })
        })
        .collect();
    let oracle = FakeOracle::start(Some(serde_json::Value::Array(rows))).await;
    let cache = WeatherCache::new(&oracle.url).unwrap();
    let read = cache.weather(&upcoming(2), Duration::from_secs(5)).await;
    let weather = read.value().unwrap();
    assert_eq!(weather.forecast("KPWM", Metric::TempHigh), Some(70.0));
    assert_eq!(weather.forecast("KBTV", Metric::WindSpeed), Some(8.0));
}
