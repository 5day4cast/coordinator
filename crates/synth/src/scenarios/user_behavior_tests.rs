use super::*;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio::sync::Barrier;

#[derive(Default)]
struct Protocol {
    capacity: usize,
    tickets: BTreeMap<Uuid, String>,
    ticket_requests: Vec<Value>,
    paid: BTreeSet<Uuid>,
    entries: Vec<Value>,
    events: Vec<String>,
    attempts: usize,
    recycle: bool,
    /// Recycling hands out a fresh seat instead of the unpaid one: Some(true) once another
    /// player has paid for the unpaid seat, Some(false) while it stays reserved.
    other_seat: Option<bool>,
    first_replacement_blocked: bool,
    /// Ticket requests are refused as the competition no longer accepts entries.
    tickets_closed: bool,
    duplicate_status: u16,
    closed: bool,
    refund_failure: Option<Uuid>,
    missing_registration: bool,
    fail_first_payment: bool,
    payment_gate: Option<Arc<Barrier>>,
    db: Option<SynthDb>,
    durable_before_pay: Vec<bool>,
    lose_submission_response: bool,
    entry_deadline: Option<OffsetDateTime>,
    /// Seats other people paid for and entered.
    others: usize,
    /// Refunds settle only from then, as an escrow opens its refund leaf.
    refunds_open: Option<OffsetDateTime>,
}

type Shared = Arc<Mutex<Protocol>>;

struct Mock {
    client: CoordinatorClient,
    state: Shared,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn new(protocol: Protocol) -> Self {
        let state = Arc::new(Mutex::new(protocol));
        let app = Router::new()
            .route("/api/v1/competitions", post(|Json(body): Json<Value>| async move { Json(json!({"id":body["id"],"created_at":"2026-01-01T00:00:00Z","event_submission":{}})) }))
            .route("/api/v1/competitions/{id}", get(competition))
            .route("/api/v1/competitions/{id}/ticket", post(ticket))
            .route("/api/v1/competitions/{id}/tickets/{ticket}/status", get(status))
            .route("/api/v1/competitions/{id}/tickets/{ticket}/refund", get(refund))
            .route("/admin/api/test/settle-invoice/{ticket}", post(pay))
            .route("/api/v1/entries", post(submit).get(entries))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            CoordinatorClient::new(&format!("http://{}", listener.local_addr().unwrap()), None);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            client,
            state,
            task,
        }
    }
}

async fn competition(State(state): State<Shared>, Path(id): Path<Uuid>) -> Json<Value> {
    let state = state.lock().unwrap();
    Json(
        json!({"id":id,"created_at":"2026-01-01T00:00:00Z", "event_submission":{"start_observation_date":state.entry_deadline.unwrap_or_else(|| OffsetDateTime::now_utc()+time::Duration::hours(1)).format(&Rfc3339).unwrap(), "total_allowed_entries":state.capacity}, "total_entries":state.entries.len() + state.others, "total_paid_entries":state.paid.len() + state.others, "awaiting_attestation_at":if state.entries.len() + state.others==state.capacity { Some("2026-01-01T00:01:00Z") }else{None} }),
    )
}

async fn ticket(State(state): State<Shared>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.attempts += 1;
    state.ticket_requests.push(body);
    if state.tickets_closed {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Competition is no longer accepting entries"})),
        );
    }
    let id = if state.tickets.len() + state.others < state.capacity {
        Uuid::now_v7()
    } else {
        if !state.first_replacement_blocked {
            state.first_replacement_blocked = true;
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"No ticket available for competition"})),
            );
        }
        if !state.recycle {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"No ticket available for competition"})),
            );
        }
        match state
            .tickets
            .keys()
            .find(|id| !state.paid.contains(id))
            .copied()
        {
            Some(unpaid) => match state.other_seat {
                None => unpaid,
                Some(taken) => {
                    if taken {
                        state.paid.insert(unpaid);
                    }
                    Uuid::now_v7()
                }
            },
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"No ticket available for competition"})),
                );
            }
        }
    };
    let hash = format!("hash-{}", state.attempts);
    state.tickets.insert(id, hash.clone());
    state.events.push(format!("request:{id}"));
    (
        StatusCode::OK,
        Json(
            json!({"ticket_id":id,"payment_request":"mock-invoice","payment_hash":hash,"amount_sats":1100,"keymeld_user_id":Uuid::nil(),"keymeld_gateway_url":null,"keymeld_session_id":if state.missing_registration { Some("missing-assignment") } else { None },"keymeld_enclave_public_key":null,"keymeld_registration":null}),
        ),
    )
}

fn begin_payment_attempt(
    state: &Shared,
    ticket: Uuid,
) -> (Option<Arc<Barrier>>, bool, Option<SynthDb>) {
    let mut state = state.lock().unwrap();
    let first = state.events.iter().all(|event| !event.starts_with("pay:"));
    state.events.push(format!("pay:{ticket}"));
    (
        state.payment_gate.clone(),
        state.fail_first_payment && first,
        state.db.clone(),
    )
}

async fn pay(State(state): State<Shared>, Path(ticket): Path<Uuid>) -> (StatusCode, Json<Value>) {
    let (gate, fail, db) = begin_payment_attempt(&state, ticket);
    if let Some(gate) = gate {
        gate.wait().await;
    }
    if let Some(db) = db {
        let runs = db.list_runs(1).await.unwrap();
        let steps = db.get_steps(&runs[0].id).await.unwrap();
        let traces = crate::trail::tracker::entries_of(&steps);
        let durable = traces.iter().any(|trace| {
            trace.ticket_id == Some(ticket)
                && trace.payment_started == Some(true)
                && trace.behavior.is_some()
                && trace.waits[0].elapsed_ms.is_some()
                && trace.waits[1].elapsed_ms.is_some()
                && trace
                    .pending_submission
                    .as_ref()
                    .is_some_and(|entry| entry.ticket_id == ticket)
        });
        state.lock().unwrap().durable_before_pay.push(durable);
        if !durable {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"payment started without durable actor trace"})),
            );
        }
    }
    if fail {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"payer unavailable"})),
        );
    }
    state.lock().unwrap().paid.insert(ticket);
    (StatusCode::OK, Json(json!({})))
}

async fn status(
    State(state): State<Shared>,
    Path((_id, ticket)): Path<(Uuid, Uuid)>,
) -> Json<Value> {
    Json(json!(if state.lock().unwrap().paid.contains(&ticket) {
        "Paid"
    } else {
        "Reserved"
    }))
}

async fn refund(
    State(state): State<Shared>,
    Path((_id, ticket)): Path<(Uuid, Uuid)>,
) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.events.push(format!("refund:{ticket}"));
    if state.refund_failure == Some(ticket) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"refund temporarily unavailable"})),
        );
    }
    (
        StatusCode::OK,
        Json(
            if state.closed
                && state.paid.contains(&ticket)
                && state
                    .refunds_open
                    .is_none_or(|open| OffsetDateTime::now_utc() >= open)
            {
                json!({"state":"settled","paid_sats":1000,"ark_txid":"mock-refund","updated_at":0})
            } else {
                Value::Null
            },
        ),
    )
}

async fn submit(State(state): State<Shared>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    let ticket = body["ticket_id"].as_str().unwrap().parse::<Uuid>().unwrap();
    state.events.push(format!("submit:{ticket}"));
    if state.closed {
        return (
            StatusCode::from_u16(state.duplicate_status.max(400)).unwrap(),
            Json(json!({"error":"Competition is no longer accepting entries"})),
        );
    }
    if state
        .entries
        .iter()
        .any(|entry| entry["ticket_id"] == body["ticket_id"])
    {
        return (
            StatusCode::from_u16(state.duplicate_status.max(400)).unwrap(),
            Json(json!({"error":"Ticket has already been used"})),
        );
    }
    assert!(
        state.paid.contains(&ticket),
        "entry submitted before payment"
    );
    let response = json!({"id":body["id"],"ticket_id":body["ticket_id"],"event_id":body["event_id"],"ephemeral_pubkey":body["ephemeral_pubkey"],"pubkey":"mock","signed_at":null,"paid_at":null,"paid_out_at":null});
    state.entries.push(response.clone());
    if state.lose_submission_response {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"response lost after commit"})),
        );
    }
    (StatusCode::OK, Json(response))
}

async fn entries(State(state): State<Shared>) -> Json<Value> {
    Json(json!(state.lock().unwrap().entries))
}

fn plan(behavior: EntryBehavior) -> EntryPlan {
    EntryPlan {
        user_index: 0,
        arrival_secs: 0,
        before_payment_secs: 0,
        before_submit_secs: 0,
        behavior,
    }
}
fn config(users: usize) -> ScenarioConfig {
    ScenarioConfig {
        users,
        seed: Some(19),
        poll_interval_secs: 0,
        state_timeout_secs: 2,
        ..Default::default()
    }
    .resolve_plan("full_lifecycle")
    .unwrap()
}
async fn db() -> (tempfile::TempDir, SynthDb) {
    let directory = tempfile::tempdir().unwrap();
    let db = SynthDb::new(directory.path().join("synth.sqlite").to_str().unwrap())
        .await
        .unwrap();
    (directory, db)
}

#[tokio::test]
async fn concurrent_actors_preserve_durable_recorder_before_either_payment() {
    let (_directory, db) = db().await;
    let mock = Mock::new(Protocol {
        capacity: 2,
        payment_gate: Some(Arc::new(Barrier::new(2))),
        db: Some(db.clone()),
        ..Default::default()
    })
    .await;
    let runner = crate::runner::Runner::for_tests(
        mock.client.clone(),
        db.clone(),
        crate::events::Events::new(),
    );
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        runner.run_scenario("full_lifecycle", config(2)),
    )
    .await
    .expect("sequential actors deadlock at the payment barrier")
    .unwrap();
    assert_eq!(result.status, ScenarioStatus::Passed);
    assert_eq!(
        mock.state.lock().unwrap().durable_before_pay,
        vec![true, true]
    );
    let runs = db.list_runs(1).await.unwrap();
    let traces = crate::trail::tracker::entries_of(&db.get_steps(&runs[0].id).await.unwrap());
    assert_eq!(traces.len(), 2);
    assert!(traces.iter().all(|trace| trace.paid
        && trace.entry_submitted
        && trace.waits.iter().all(|wait| wait.elapsed_ms.is_some())));
}

/// Real people can take any number of a competition's seats alongside synth's players: none,
/// one, several, or all but one. Each player left without a seat stands down, and the run still
/// follows the competition through.
#[tokio::test]
async fn seats_taken_by_other_players_do_not_fail_the_run() {
    for (seats, others) in [(5, 0), (5, 1), (5, 3), (5, 4), (8, 5)] {
        let (_directory, db) = db().await;
        let mock = Mock::new(Protocol {
            capacity: seats,
            others,
            ..Default::default()
        })
        .await;
        let runner = crate::runner::Runner::for_tests(
            mock.client.clone(),
            db.clone(),
            crate::events::Events::new(),
        );
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            runner.run_scenario("full_lifecycle", config(seats)),
        )
        .await
        .unwrap()
        .unwrap();
        let case = format!("{others} of {seats} seats taken by others");
        assert_eq!(
            result.status,
            ScenarioStatus::Passed,
            "{case}: {:?}",
            result.error
        );
        let entering: Vec<&StepResult> = result
            .steps
            .iter()
            .filter(|step| step.name.ends_with("_enter"))
            .collect();
        assert_eq!(entering.len(), seats, "{case}");
        assert_eq!(
            entering
                .iter()
                .filter(|step| step.status == StepStatus::Skipped)
                .count(),
            others,
            "{case}"
        );
        let traces: Vec<EntryTrace> = entering
            .iter()
            .map(|step| serde_json::from_value(step.details.clone().unwrap()).unwrap())
            .collect();
        for stood_down in traces.iter().filter(|trace| trace.seat_taken) {
            assert!(
                !stood_down.paid && stood_down.ticket_id.is_none() && !stood_down.may_have_paid(),
                "{case}"
            );
        }
        assert_eq!(
            traces.iter().filter(|trace| trace.entry_submitted).count(),
            seats - others,
            "{case}"
        );
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.name == "wait_awaiting_attestation"),
            "{case}"
        );
    }
}

/// Two runs at once, each tracked by its own competition. The mock's competition awaits its
/// attestation only once four players have entered, two from each run, so runs made one after the
/// other would never finish.
#[tokio::test]
async fn overlapping_runs_are_each_tracked_by_their_competition() {
    let (_directory, db) = db().await;
    let mock = Mock::new(Protocol {
        capacity: 4,
        ..Default::default()
    })
    .await;
    let runner = crate::runner::Runner::for_tests(
        mock.client.clone(),
        db.clone(),
        crate::events::Events::new(),
    );
    let plan = |seed| {
        let mut config = config(2);
        config.seed = Some(seed);
        config.competition_id = None;
        config.resolve_plan("full_lifecycle").unwrap()
    };
    let (first, second) = (plan(1), plan(2));
    let competitions = [
        first.competition_id.unwrap(),
        second.competition_id.unwrap(),
    ];
    assert_ne!(competitions[0], competitions[1]);
    let watching = runner.clone();
    let seen = tokio::spawn(async move {
        loop {
            let live = watching.live_runs();
            if live.len() == 2 {
                return live
                    .into_iter()
                    .map(|run| run.competition_id)
                    .collect::<Vec<_>>();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let (a, b) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            runner.run_scenario("full_lifecycle", first),
            runner.run_scenario("full_lifecycle", second)
        )
    })
    .await
    .expect("overlapping runs finish together");
    assert_eq!(a.unwrap().status, ScenarioStatus::Passed);
    assert_eq!(b.unwrap().status, ScenarioStatus::Passed);
    let mut seen = seen.await.unwrap();
    seen.sort();
    let mut expected = competitions.to_vec();
    expected.sort();
    assert_eq!(seen, expected);
    assert!(
        runner.live_runs().is_empty(),
        "each leaves the map when it ends"
    );
}

#[test]
fn only_cancellations_synth_did_not_cause_are_expected() {
    let competition = |json: Value| -> CompetitionResponse {
        let mut base = json!({"id": Uuid::now_v7(), "created_at": "2026-01-01T00:00:00Z",
            "event_submission": {"total_allowed_entries": 3}});
        base.as_object_mut()
            .unwrap()
            .extend(json.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    };
    let paid = |count: usize| -> Vec<EntryTrace> {
        (0..count)
            .map(|_| EntryTrace {
                paid: true,
                ..Default::default()
            })
            .collect()
    };
    let cancelled = "2026-01-01T01:00:00Z";
    // Synth's own three players, cancelled for no reason of theirs: a coordinator failure.
    assert_eq!(
        expected_cancellation(
            &competition(json!({"cancelled_at": cancelled, "total_paid_entries": 3})),
            &paid(3)
        ),
        None
    );
    // Someone else paid and never entered.
    assert!(expected_cancellation(
        &competition(json!({"cancelled_at": cancelled, "total_paid_entries": 3})),
        &paid(2)
    )
    .is_some());
    // Someone else held a seat until the deadline.
    let mut traces = paid(2);
    traces.push(EntryTrace {
        seat_taken: true,
        ..Default::default()
    });
    assert!(expected_cancellation(
        &competition(json!({"cancelled_at": cancelled, "total_paid_entries": 2})),
        &traces
    )
    .is_some());
    // The kickoff check failed on fees, cancelled or failed.
    let kickoff = json!({"players": 3, "min_players": 5, "sat_per_vb": 4, "passed": false});
    for stopped in ["cancelled_at", "failed_at"] {
        assert!(expected_cancellation(
            &competition(json!({stopped: cancelled, "kickoff_check": kickoff})),
            &paid(3)
        )
        .is_some());
    }
    // Still running: nothing to excuse.
    assert_eq!(
        expected_cancellation(&competition(json!({})), &traces),
        None
    );
}

#[tokio::test]
async fn one_failed_actor_does_not_cancel_another_users_in_flight_payment() {
    let (_directory, db) = db().await;
    let mock = Mock::new(Protocol {
        capacity: 2,
        payment_gate: Some(Arc::new(Barrier::new(2))),
        fail_first_payment: true,
        ..Default::default()
    })
    .await;
    let mut steps = Steps::new();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run_steps(
            &mock.client,
            &db,
            &config(2),
            Scenario::FullLifecycle,
            &Payer::TestEndpoint,
            &mut steps,
        ),
    )
    .await
    .unwrap();
    assert!(outcome.is_err());
    let result = finish_result(
        "test",
        OffsetDateTime::now_utc(),
        Instant::now(),
        steps,
        true,
    );
    let traces: Vec<EntryTrace> = result
        .steps
        .iter()
        .filter(|step| step.name.ends_with("_enter"))
        .map(|step| serde_json::from_value(step.details.clone().unwrap()).unwrap())
        .collect();
    assert_eq!(traces.len(), 2);
    assert_eq!(
        traces.iter().filter(|trace| trace.entry_submitted).count(),
        1
    );
    assert_eq!(
        traces.iter().filter(|trace| trace.may_have_paid()).count(),
        1
    );
    assert_eq!(mock.state.lock().unwrap().entries.len(), 1);
}

#[tokio::test]
async fn unpaid_dropout_never_pays_and_replacement_retries_then_rotates_its_ticket_hash() {
    let (_directory, db) = db().await;
    let mock = Mock::new(Protocol {
        capacity: 2,
        recycle: true,
        ..Default::default()
    })
    .await;
    let config = config(2).resolve_plan("abandoned_unpaid").unwrap();
    let mut steps = Steps::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        run_steps(
            &mock.client,
            &db,
            &config,
            Scenario::AbandonedUnpaid,
            &Payer::TestEndpoint,
            &mut steps,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let result = finish_result(
        "test",
        OffsetDateTime::now_utc(),
        Instant::now(),
        steps,
        false,
    );
    let traces: Vec<EntryTrace> = result
        .steps
        .iter()
        .filter(|step| step.name.ends_with("_enter"))
        .map(|step| serde_json::from_value(step.details.clone().unwrap()).unwrap())
        .collect();
    let abandoned = traces
        .iter()
        .find(|trace| trace.behavior == Some(EntryBehavior::AbandonUnpaid))
        .unwrap();
    assert!(!abandoned.paid && !abandoned.may_have_paid() && !abandoned.entry_submitted);
    let replacement = traces.iter().find(|trace| trace.user == "charlie").unwrap();
    assert_eq!(abandoned.ticket_id, replacement.ticket_id);
    assert_ne!(abandoned.payment_hash, replacement.payment_hash);
    assert!(replacement.paid && replacement.entry_submitted);
    assert!(replacement.waits[3].elapsed_ms.unwrap() >= 900);
    let state = mock.state.lock().unwrap();
    assert_eq!(
        state
            .events
            .iter()
            .filter(|event| event.starts_with("pay:"))
            .count(),
        2
    );
    assert_eq!(state.entries.len(), 2);
}

/// Another player may take the abandoned seat before the replacement asks; the replacement then
/// gets another, and passes as long as the abandoned ticket is no longer reserved.
#[tokio::test]
async fn replacement_may_get_another_seat_once_the_abandoned_one_is_released() {
    for taken in [true, false] {
        let (_directory, db) = db().await;
        let mock = Mock::new(Protocol {
            capacity: 2,
            recycle: true,
            other_seat: Some(taken),
            ..Default::default()
        })
        .await;
        let config = config(2).resolve_plan("abandoned_unpaid").unwrap();
        let mut steps = Steps::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_steps(
                &mock.client,
                &db,
                &config,
                Scenario::AbandonedUnpaid,
                &Payer::TestEndpoint,
                &mut steps,
            ),
        )
        .await
        .unwrap();
        if !taken {
            let step = result.unwrap_err();
            assert!(
                step.error.unwrap().contains("still reserved"),
                "an abandoned ticket still reserved fails the replacement"
            );
            continue;
        }
        result.unwrap();
        let result = finish_result(
            "test",
            OffsetDateTime::now_utc(),
            Instant::now(),
            steps,
            false,
        );
        let traces: Vec<EntryTrace> = result
            .steps
            .iter()
            .filter(|step| step.name.ends_with("_enter"))
            .map(|step| serde_json::from_value(step.details.clone().unwrap()).unwrap())
            .collect();
        let abandoned = traces
            .iter()
            .find(|trace| trace.behavior == Some(EntryBehavior::AbandonUnpaid))
            .unwrap();
        let replacement = traces.iter().find(|trace| trace.user == "charlie").unwrap();
        assert_ne!(abandoned.ticket_id, replacement.ticket_id);
        assert_ne!(abandoned.payment_hash, replacement.payment_hash);
        assert!(replacement.paid && replacement.entry_submitted);
    }
}

#[tokio::test]
async fn paid_dropout_keeps_its_seat_and_is_refunded_without_submitting_an_entry() {
    let mock = Mock::new(Protocol {
        capacity: 1,
        recycle: true,
        first_replacement_blocked: true,
        ..Default::default()
    })
    .await;
    let user = SynthUser::new_random("alice").unwrap();
    let comp = Uuid::now_v7();
    let (step, trace) = run_actor(
        &mock.client,
        &user,
        std::slice::from_ref(&user),
        &comp,
        &config(1),
        &Payer::TestEndpoint,
        &plan(EntryBehavior::AbandonPaid),
        Instant::now(),
        OffsetDateTime::now_utc() + time::Duration::hours(1),
    )
    .await;
    assert_eq!(step.status, StepStatus::Passed);
    assert!(trace.paid && !trace.entry_submitted);
    let other = SynthUser::new_random("replacement").unwrap();
    let error = full_lifecycle::request_entry(
        &mock.client,
        &other,
        &comp,
        None,
        "replacement",
        &mut EntryTrace::new(&other),
    )
    .await
    .err()
    .unwrap();
    assert!(error
        .downcast_ref::<ApiRejection>()
        .unwrap()
        .is_no_capacity());
    mock.state.lock().unwrap().closed = true;
    let refunded = super::super::escrow_refund::wait_for_refund(
        &mock.client,
        &user,
        &comp,
        &trace.ticket_id.unwrap(),
        None,
        &config(1),
    )
    .await
    .unwrap();
    assert_eq!(refunded["paid_sats"], 1000);
    assert!(mock.state.lock().unwrap().entries.is_empty());
}

/// An escrow is refunded only once its refund leaf opens, which can be long after the refund
/// timeout: the wait lasts until it opens and the timeout more.
#[tokio::test]
async fn a_refund_is_waited_for_until_its_escrow_opens() {
    let ticket = Uuid::now_v7();
    let opens = OffsetDateTime::now_utc() + time::Duration::seconds(2);
    let mock = Mock::new(Protocol {
        closed: true,
        paid: BTreeSet::from([ticket]),
        refunds_open: Some(opens),
        ..Default::default()
    })
    .await;
    let user = SynthUser::new_random("alice").unwrap();
    let mut config = config(1);
    config.refund_timeout_secs = 1;
    let competition = Uuid::now_v7();
    let wait = |refund_at| {
        super::super::escrow_refund::wait_for_refund(
            &mock.client,
            &user,
            &competition,
            &ticket,
            refund_at,
            &config,
        )
    };
    assert!(
        wait(None).await.is_err(),
        "the timeout alone ends before the escrow opens"
    );
    let refunded = wait(Some(opens.unix_timestamp() + 1)).await.unwrap();
    assert_eq!(refunded["paid_sats"], 1000);
}

#[tokio::test]
async fn duplicate_submission_requires_the_expected_400_and_one_accepted_entry() {
    for (status, expected) in [(400, StepStatus::Passed), (500, StepStatus::Failed)] {
        let mock = Mock::new(Protocol {
            capacity: 1,
            duplicate_status: status,
            ..Default::default()
        })
        .await;
        let user = SynthUser::new_random("alice").unwrap();
        let (step, trace) = run_actor(
            &mock.client,
            &user,
            std::slice::from_ref(&user),
            &Uuid::now_v7(),
            &config(1),
            &Payer::TestEndpoint,
            &plan(EntryBehavior::DuplicateSubmission),
            Instant::now(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await;
        assert_eq!(step.status, expected);
        assert_eq!(trace.rejected_submission.unwrap().status, status);
        assert!(trace.entry_submitted);
        assert_eq!(trace.submission_attempts, if status == 400 { 3 } else { 2 });
        let state = mock.state.lock().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(
            state
                .events
                .iter()
                .filter(|event| event.starts_with("pay:"))
                .count(),
            1,
            "double-click and retry never pay twice"
        );
    }
}

/// Synth sizes a competition to its players, so one that closed before a player got a seat was
/// filled by someone else: skipped, not failed. With no outside entry, it is a failure.
#[tokio::test]
async fn a_seat_taken_by_an_outside_player_skips_the_entry() {
    for (others, expected) in [(1, StepStatus::Skipped), (0, StepStatus::Failed)] {
        let mock = Mock::new(Protocol {
            capacity: 1,
            others,
            tickets_closed: true,
            ..Default::default()
        })
        .await;
        let user = SynthUser::new_random("alice").unwrap();
        let (step, trace) = run_actor(
            &mock.client,
            &user,
            std::slice::from_ref(&user),
            &Uuid::now_v7(),
            &config(1),
            &Payer::TestEndpoint,
            &plan(EntryBehavior::Complete),
            Instant::now(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await;
        assert_eq!(step.status, expected);
        assert_eq!(trace.outside_entries, Some(others as u64));
        assert_eq!(trace.seat_taken, others > 0);
        if others > 0 {
            assert_eq!(step.error.as_deref(), Some("seat taken by outside player"));
        }
        assert!(mock.state.lock().unwrap().events.is_empty());
    }
}

#[tokio::test]
async fn late_submission_requires_the_expected_400_and_no_accepted_entry() {
    for (status, passed) in [(400, true), (500, false)] {
        let mock = Mock::new(Protocol {
            capacity: 1,
            duplicate_status: status,
            ..Default::default()
        })
        .await;
        let user = SynthUser::new_random("alice").unwrap();
        let comp = Uuid::now_v7();
        let mut trace = planned_trace(&user, &plan(EntryBehavior::LateSubmission));
        let requested =
            full_lifecycle::request_entry(&mock.client, &user, &comp, None, "late", &mut trace)
                .await
                .unwrap();
        let prepared = full_lifecycle::register_entry(
            &mock.client,
            &user,
            &comp,
            &config(1),
            0,
            requested,
            "late",
            &mut trace,
        )
        .await
        .unwrap();
        full_lifecycle::pay_entry(
            &mock.client,
            &user,
            &comp,
            &prepared,
            &Payer::TestEndpoint,
            "late",
            &mut trace,
        )
        .await
        .unwrap();
        mock.state.lock().unwrap().closed = true;
        let result = assert_rejected_submission(
            &mock.client,
            &user,
            &comp,
            &prepared,
            "late",
            &mut trace,
            false,
        )
        .await;
        assert_eq!(result.is_ok(), passed);
        assert!(trace.paid && !trace.entry_submitted);
        assert_eq!(trace.rejected_submission.unwrap().status, status);
        assert!(mock.state.lock().unwrap().entries.is_empty());
    }
}

#[tokio::test]
async fn missed_invoice_deadline_aborts_before_requesting_or_paying() {
    let mock = Mock::new(Protocol {
        capacity: 1,
        ..Default::default()
    })
    .await;
    let user = SynthUser::new_random("alice").unwrap();
    let (step, trace) = run_actor(
        &mock.client,
        &user,
        std::slice::from_ref(&user),
        &Uuid::now_v7(),
        &config(1),
        &Payer::TestEndpoint,
        &plan(EntryBehavior::Complete),
        Instant::now(),
        OffsetDateTime::now_utc() + time::Duration::seconds(59),
    )
    .await;
    assert_eq!(step.status, StepStatus::Failed);
    assert_eq!(trace.payment_started, Some(false));
    assert!(mock.state.lock().unwrap().events.is_empty());
    assert!(ensure_submission_time(
        OffsetDateTime::now_utc() - time::Duration::seconds(1),
        &config(1)
    )
    .is_err());
}

#[test]
fn restart_markers_keep_legacy_uncertainty_but_distinguish_deliberate_nonpayment() {
    let mut trace: EntryTrace =
        serde_json::from_value(json!({"user":"alice","nostr_pubkey":"00","payment_hash":"hash"}))
            .unwrap();
    assert!(trace.may_have_paid());
    trace.payment_started = Some(false);
    assert!(!trace.may_have_paid());
    trace.payment_started = Some(true);
    assert!(trace.may_have_paid());
    trace.paid = true;
    assert!(!trace.may_have_paid());
}

#[test]
fn weather_picks_replay_per_seed_and_user() {
    let stations = vec!["KDEN".into(), "KORD".into(), "KJFK".into()];
    let picks_in = |seed, index, shape| {
        serde_json::to_value(full_lifecycle::generate_predictions(
            &stations,
            Some(seed),
            index,
            shape,
        ))
        .unwrap()
    };
    let picks = |seed, index| picks_in(seed, index, WindowShape::FullDay);
    assert_eq!(picks(19, 0), picks(19, 0));
    assert_ne!(picks(19, 0), picks(19, 1));
    assert_ne!(picks(19, 0), picks(20, 0));
    // A half picks only what it scores, and the same as a full day for those.
    let full = picks(19, 0);
    for (shape, kept, dropped) in [
        (WindowShape::Day, "temp_high", "temp_low"),
        (WindowShape::Night, "temp_low", "temp_high"),
    ] {
        let half = picks_in(19, 0, shape);
        for (half, full) in half
            .as_array()
            .unwrap()
            .iter()
            .zip(full.as_array().unwrap())
        {
            assert!(half[dropped].is_null(), "{shape:?}");
            assert_eq!(half[kept], full[kept]);
            assert_eq!(half["wind_speed"], full["wind_speed"]);
        }
    }
}

#[test]
fn paid_reservation_probe_and_cancellation_budget_include_their_actual_deadlines() {
    let now = OffsetDateTime::now_utc();
    assert_eq!(reservation_hold_ms(now, now), 601_000);
    assert_eq!(
        reservation_hold_ms(now, now + time::Duration::seconds(600)),
        1000
    );
    assert_eq!(
        reservation_hold_ms(now, now + time::Duration::seconds(601)),
        0
    );
    assert_eq!(
        cancellation_budget_secs(600, now + time::Duration::seconds(1200), now),
        1801
    );
    assert_eq!(
        cancellation_budget_secs(600, now - time::Duration::seconds(1), now),
        601
    );
}

#[tokio::test]
async fn late_wait_is_observed_and_an_unexpected_acceptance_is_preserved_for_tracking() {
    let mock = Mock::new(Protocol {
        capacity: 1,
        ..Default::default()
    })
    .await;
    let user = SynthUser::new_random("alice").unwrap();
    let comp = Uuid::now_v7();
    let plan = plan(EntryBehavior::LateSubmission);
    let mut trace = planned_trace(&user, &plan);
    let requested =
        full_lifecycle::request_entry(&mock.client, &user, &comp, None, "late", &mut trace)
            .await
            .unwrap();
    let prepared = full_lifecycle::register_entry(
        &mock.client,
        &user,
        &comp,
        &config(1),
        0,
        requested,
        "late",
        &mut trace,
    )
    .await
    .unwrap();
    full_lifecycle::pay_entry(
        &mock.client,
        &user,
        &comp,
        &prepared,
        &Payer::TestEndpoint,
        "late",
        &mut trace,
    )
    .await
    .unwrap();
    trace.submission_not_before =
        Some(OffsetDateTime::now_utc() + time::Duration::milliseconds(50));
    trace.waits.push(EntryWait {
        stage: "after_entry_close".into(),
        planned_ms: 50,
        elapsed_ms: None,
    });
    let started = Instant::now();
    let result = finish_entry(
        &mock.client,
        &user,
        &comp,
        &config(1),
        &prepared,
        &plan,
        OffsetDateTime::now_utc(),
        "late",
        &mut trace,
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("late submission unexpectedly accepted"));
    assert!(started.elapsed() >= Duration::from_millis(40));
    assert!(trace.waits[3].elapsed_ms.unwrap() >= 40);
    assert!(
        trace.entry_submitted,
        "a negative assertion must not hide a real accepted entry"
    );
    assert_eq!(trace.entry_id, Some(prepared.entry.id));
    assert_eq!(mock.state.lock().unwrap().entries.len(), 1);
}

#[tokio::test]
async fn one_refund_failure_does_not_prevent_checking_the_other_paid_ticket() {
    let users = [
        SynthUser::new_random("alice").unwrap(),
        SynthUser::new_random("bob").unwrap(),
    ];
    let tickets = [Uuid::now_v7(), Uuid::now_v7()];
    let mock = Mock::new(Protocol {
        capacity: 2,
        closed: true,
        paid: tickets.into_iter().collect(),
        refund_failure: Some(tickets[0]),
        ..Default::default()
    })
    .await;
    let traces: Vec<_> = users
        .iter()
        .zip(tickets)
        .map(|(user, ticket)| {
            let mut trace = EntryTrace::new(user);
            trace.ticket_id = Some(ticket);
            trace.paid = true;
            trace
        })
        .collect();
    let mut steps = Steps::new();
    assert!(collect_refunds(
        &mock.client,
        &users,
        &Uuid::now_v7(),
        &config(2),
        &traces,
        &mut steps
    )
    .await
    .is_err());
    let result = finish_result(
        "test",
        OffsetDateTime::now_utc(),
        Instant::now(),
        steps,
        true,
    );
    assert_eq!(result.steps.len(), 2);
    assert_eq!(
        result
            .steps
            .iter()
            .find(|step| step.name == "refund_alice")
            .unwrap()
            .status,
        StepStatus::Failed
    );
    assert_eq!(
        result
            .steps
            .iter()
            .find(|step| step.name == "refund_bob")
            .unwrap()
            .status,
        StepStatus::Passed
    );
    assert_eq!(
        mock.state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| event.starts_with("refund:"))
            .count(),
        2
    );
}

#[tokio::test]
async fn missing_required_refund_registration_stops_before_payment() {
    let mock = Mock::new(Protocol {
        capacity: 1,
        missing_registration: true,
        ..Default::default()
    })
    .await;
    let user = SynthUser::new_random("alice").unwrap();
    let (step, trace) = run_actor(
        &mock.client,
        &user,
        std::slice::from_ref(&user),
        &Uuid::now_v7(),
        &config(1),
        &Payer::TestEndpoint,
        &plan(EntryBehavior::AbandonPaid),
        Instant::now(),
        OffsetDateTime::now_utc() + time::Duration::hours(1),
    )
    .await;
    assert_eq!(step.status, StepStatus::Failed);
    assert!(step
        .error
        .unwrap()
        .contains("missing authorized Keymeld registration"));
    assert!(!trace.paid && !trace.may_have_paid());
    assert!(mock
        .state
        .lock()
        .unwrap()
        .events
        .iter()
        .all(|event| !event.starts_with("pay:")));
}

#[tokio::test]
async fn resumed_ticket_uses_the_saved_key_even_after_a_queue_assigns_an_entry_id() {
    let mock = Mock::new(Protocol {
        capacity: 2,
        ..Default::default()
    })
    .await;
    let user = SynthUser::new_random("alice").unwrap();
    let competition = Uuid::now_v7();
    let mut trace = EntryTrace::new(&user);
    full_lifecycle::request_entry(&mock.client, &user, &competition, None, "entry", &mut trace)
        .await
        .unwrap();
    let key_id = trace.key_derivation_id.unwrap();
    trace.entry_id = trace.ticket_id;
    let mut restored: EntryTrace =
        serde_json::from_slice(&serde_json::to_vec(&trace).unwrap()).unwrap();
    full_lifecycle::request_entry(
        &mock.client,
        &user,
        &competition,
        None,
        "entry",
        &mut restored,
    )
    .await
    .unwrap();
    assert_eq!(restored.key_derivation_id, Some(key_id));
    let protocol = mock.state.lock().unwrap();
    assert_eq!(protocol.ticket_requests[0], protocol.ticket_requests[1]);
}

fn saved_paid_entry(user: &SynthUser, competition: Uuid) -> EntryTrace {
    let derivation = Uuid::now_v7();
    let ticket = Uuid::now_v7();
    let key = user.derive_ephemeral_key(&derivation).unwrap();
    let (_, payout_hash) = crate::crypto::payout::generate_payout_pair(&key.secret_bytes);
    let entry = crate::client::entries::AddEntry {
        id: ticket,
        ticket_id: ticket,
        ephemeral_pubkey: key.public_key,
        payout_hash,
        event_id: competition,
        expected_observations: vec![],
        encrypted_keymeld_private_key: None,
        keymeld_auth_pubkey: None,
        keymeld_registration_context: None,
        keymeld_escrow_policy: None,
    };
    let trace = EntryTrace {
        behavior: Some(EntryBehavior::Complete),
        ticket_id: Some(ticket),
        entry_id: Some(ticket),
        key_derivation_id: Some(derivation),
        paid: true,
        payment_started: Some(true),
        pending_submission: Some(entry),
        ..EntryTrace::new(user)
    };
    // Exercise the actual persisted representation, including queued entry/key ID separation.
    serde_json::from_str(&serde_json::to_string(&trace).unwrap()).unwrap()
}

#[tokio::test]
async fn restart_submits_saved_paid_entry_once_even_when_the_response_is_lost() {
    for lost in [false, true] {
        let user = SynthUser::new_random("alice").unwrap();
        let competition = Uuid::now_v7();
        let mut trace = saved_paid_entry(&user, competition);
        let original = trace.clone();
        let ticket = trace.ticket_id.unwrap();
        let mock = Mock::new(Protocol {
            capacity: 1,
            paid: BTreeSet::from([ticket]),
            lose_submission_response: lost,
            ..Default::default()
        })
        .await;
        assert!(resume_paid_submission(
            &mock.client,
            &user,
            &competition,
            &config(1),
            "user_alice_enter",
            &mut trace
        )
        .await
        .unwrap());
        assert!(trace.entry_submitted && trace.pending_submission.is_none());
        // Simulate another crash before recording the accepted response.
        let mut trace = original;
        assert!(resume_paid_submission(
            &mock.client,
            &user,
            &competition,
            &config(1),
            "user_alice_enter",
            &mut trace
        )
        .await
        .unwrap());
        let state = mock.state.lock().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.events, [format!("submit:{ticket}")]);
        assert_eq!(state.attempts, 0, "no ticket or invoice requested");
    }
}

#[tokio::test]
async fn restart_does_not_submit_without_payment_or_for_intentional_abandonment() {
    let user = SynthUser::new_random("alice").unwrap();
    let competition = Uuid::now_v7();
    let mock = Mock::new(Protocol::default()).await;
    for behavior in [
        EntryBehavior::AbandonPaid,
        EntryBehavior::AbandonUnpaid,
        EntryBehavior::LateSubmission,
        EntryBehavior::DuplicateSubmission,
    ] {
        let mut trace = saved_paid_entry(&user, competition);
        trace.behavior = Some(behavior);
        assert!(!resume_paid_submission(
            &mock.client,
            &user,
            &competition,
            &config(1),
            "user_alice_enter",
            &mut trace
        )
        .await
        .unwrap());
    }
    let mut trace = saved_paid_entry(&user, competition);
    trace.paid = false;
    assert!(!resume_paid_submission(
        &mock.client,
        &user,
        &competition,
        &config(1),
        "user_alice_enter",
        &mut trace
    )
    .await
    .unwrap());
    trace.paid = true;
    trace.pending_submission = None;
    assert!(!resume_paid_submission(
        &mock.client,
        &user,
        &competition,
        &config(1),
        "user_alice_enter",
        &mut trace
    )
    .await
    .unwrap());
    assert!(mock.state.lock().unwrap().events.is_empty());
}

#[tokio::test]
async fn restart_leaves_closed_entries_for_refunds_and_rejects_mismatched_saved_bodies() {
    let user = SynthUser::new_random("alice").unwrap();
    let competition = Uuid::now_v7();
    let mut trace = saved_paid_entry(&user, competition);
    let mock = Mock::new(Protocol {
        entry_deadline: Some(OffsetDateTime::now_utc() - time::Duration::minutes(1)),
        ..Default::default()
    })
    .await;
    assert!(!resume_paid_submission(
        &mock.client,
        &user,
        &competition,
        &config(1),
        "user_alice_enter",
        &mut trace
    )
    .await
    .unwrap());
    trace.pending_submission.as_mut().unwrap().ticket_id = Uuid::now_v7();
    assert!(resume_paid_submission(
        &mock.client,
        &user,
        &competition,
        &config(1),
        "user_alice_enter",
        &mut trace
    )
    .await
    .is_err());
    assert!(mock.state.lock().unwrap().events.is_empty());
}

#[tokio::test]
async fn restart_preserves_the_submission_wait_and_does_not_overrun_the_deadline() {
    let user = SynthUser::new_random("alice").unwrap();
    let competition = Uuid::now_v7();
    let mut trace = saved_paid_entry(&user, competition);
    trace.waits.push(EntryWait {
        stage: "before_submit".into(),
        planned_ms: 7_200_000,
        elapsed_ms: None,
    });
    let mock = Mock::new(Protocol::default()).await;
    assert!(!resume_paid_submission(
        &mock.client,
        &user,
        &competition,
        &config(1),
        "user_alice_enter",
        &mut trace
    )
    .await
    .unwrap());
    let target = trace.submission_not_before.unwrap();
    assert!(target > OffsetDateTime::now_utc() + time::Duration::minutes(119));
    assert!(!resume_paid_submission(
        &mock.client,
        &user,
        &competition,
        &config(1),
        "user_alice_enter",
        &mut trace
    )
    .await
    .unwrap());
    assert_eq!(trace.submission_not_before, Some(target));
    assert!(mock.state.lock().unwrap().events.is_empty());
}
