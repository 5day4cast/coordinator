use super::*;

fn listed(sizes: &[usize]) -> Vec<PoolSummary> {
    sizes
        .iter()
        .enumerate()
        .map(|(index, players)| PoolSummary {
            competition_id: Uuid::now_v7(),
            pool_index: index as u32,
            players: *players,
        })
        .collect()
}

/// Place `tickets` into `pools` in order, filling each to its listed size.
fn place(pools: &[PoolSummary], tickets: &[Uuid]) -> BTreeMap<Uuid, Uuid> {
    let mut rest = tickets.iter();
    pools
        .iter()
        .flat_map(|pool| {
            rest.by_ref()
                .take(pool.players)
                .map(|ticket| (*ticket, pool.competition_id))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn fresh(count: usize) -> Vec<Uuid> {
    (0..count).map(|_| Uuid::now_v7()).collect()
}

fn rules() -> PoolRules {
    PoolRules::new(2, 25).unwrap()
}

#[test]
fn twenty_seven_entries_split_into_two_even_pools_holding_each_once() {
    let tickets = fresh(27);
    let pools = listed(&[14, 13]);
    check_split(&rules(), &pools, &tickets, &place(&pools, &tickets)).unwrap();
    // The coordinator may list the larger pool second.
    let pools = listed(&[13, 14]);
    check_split(&rules(), &pools, &tickets, &place(&pools, &tickets)).unwrap();
    let tickets = fresh(51);
    let pools = listed(&[17, 17, 17]);
    check_split(&rules(), &pools, &tickets, &place(&pools, &tickets)).unwrap();
}

#[test]
fn a_small_queue_forms_one_pool_of_everyone() {
    let tickets = fresh(5);
    let pools = listed(&[5]);
    check_split(&rules(), &pools, &tickets, &place(&pools, &tickets)).unwrap();
}

#[test]
fn uneven_or_too_many_or_too_few_pools_are_refused() {
    let tickets = fresh(27);
    let cases: [&[usize]; 4] = [&[15, 12], &[25, 2], &[9, 9, 9], &[27]];
    for sizes in cases {
        let pools = listed(sizes);
        assert!(
            check_split(&rules(), &pools, &tickets, &place(&pools, &tickets)).is_err(),
            "{sizes:?}"
        );
    }
    let pools = listed(&[1]);
    assert!(check_split(&rules(), &pools, &tickets[..1], &place(&pools, &tickets)).is_err());
}

#[test]
fn every_entry_must_be_in_exactly_one_listed_pool() {
    let tickets = fresh(27);
    let pools = listed(&[14, 13]);
    let placed = place(&pools, &tickets);

    let mut missing = placed.clone();
    missing.remove(&tickets[0]);
    assert!(check_split(&rules(), &pools, &tickets, &missing).is_err());

    let mut elsewhere = placed.clone();
    elsewhere.insert(tickets[0], Uuid::now_v7());
    assert!(check_split(&rules(), &pools, &tickets, &elsewhere).is_err());

    // Right sizes in the listing, but one pool holds an entry of the other.
    let mut crowded = placed.clone();
    crowded.insert(tickets[26], pools[0].competition_id);
    assert!(check_split(&rules(), &pools, &tickets, &crowded).is_err());

    let mut stray = placed.clone();
    stray.insert(Uuid::now_v7(), pools[0].competition_id);
    assert!(check_split(&rules(), &pools, &tickets, &stray).is_err());

    let mut duplicated = tickets.clone();
    duplicated[1] = duplicated[0];
    assert!(check_split(&rules(), &pools, &duplicated, &placed).is_err());
}

#[test]
fn other_players_can_share_the_pools() {
    // Synth's 20 tickets, and 7 other people's, split into pools of 14 and 13.
    let everyone = fresh(27);
    let pools = listed(&[14, 13]);
    let placed = place(&pools, &everyone);
    let ours = everyone[..20].to_vec();
    let ours_placed = placed
        .iter()
        .filter(|(ticket, _)| ours.contains(ticket))
        .map(|(ticket, pool)| (*ticket, *pool))
        .collect();
    check_split(&rules(), &pools, &ours, &ours_placed).unwrap();
    // Two synth players with three others: a queue too small by synth's count forms a pool.
    let pools = listed(&[5]);
    let ours = fresh(2);
    check_split(&rules(), &pools, &ours, &place(&pools, &ours)).unwrap();
    // The split is still checked against everyone the pools list.
    let pools = listed(&[20, 7]);
    assert!(check_split(&rules(), &pools, &ours, &place(&pools, &ours)).is_err());
}

#[test]
fn pool_indexes_must_count_from_zero() {
    let tickets = fresh(27);
    let mut pools = listed(&[14, 13]);
    pools[1].pool_index = 2;
    assert!(check_split(&rules(), &pools, &tickets, &place(&pools, &tickets)).is_err());
    pools[1].pool_index = 0;
    assert!(check_split(&rules(), &pools, &tickets, &place(&pools, &tickets)).is_err());
}

fn shape(scenario: &str, players: Option<usize>, max: Option<usize>) -> Result<QueueShape> {
    let config = ScenarioConfig {
        queue_players: players,
        max_pool_players: max,
        ..Default::default()
    };
    Ok(QueueShape::of(scenario, &config)?.expect("a queued scenario"))
}

#[test]
fn each_queued_scenario_has_its_own_shape_and_refuses_one_that_defeats_it() {
    let split = shape(QUEUED_SPLIT, None, None).unwrap();
    assert_eq!((split.players, split.abandoned), (27, 0));
    assert_eq!(split.sizes(), Some(vec![14, 13]));
    assert_eq!(
        shape(QUEUED_SPLIT, Some(5), Some(3)).unwrap().sizes(),
        Some(vec![3, 2])
    );
    assert!(shape(QUEUED_SPLIT, Some(25), None).is_err());
    assert!(shape(QUEUED_SPLIT, Some(101), None).is_err());

    let one = shape(QUEUED_ONE_POOL, None, None).unwrap();
    assert_eq!(one.rules.min_players(), 3);
    assert_eq!(one.sizes(), Some(vec![20]));
    for players in [0, 1, 2] {
        assert!(shape(QUEUED_ONE_POOL, Some(players), None).is_err());
    }
    for players in [3, 20] {
        assert_eq!(
            shape(QUEUED_ONE_POOL, Some(players), None).unwrap().sizes(),
            Some(vec![players])
        );
    }
    assert!(shape(QUEUED_ONE_POOL, Some(26), None).is_err());
    // The default competition: one pool of 20 seats whose winner takes the pot.
    assert_eq!(
        (one.rules.max_players(), one.max_entries, one.places),
        (20, Some(20), 1)
    );
    assert!(shape(QUEUED_ONE_POOL, Some(21), None).is_err());
    assert_eq!((split.max_entries, split.places), (None, 1));

    let too_few = shape(QUEUED_TOO_FEW, None, None).unwrap();
    assert_eq!(too_few.rules.min_players(), 3);
    assert_eq!((too_few.players, too_few.sizes()), (2, None));
    assert!(shape(QUEUED_TOO_FEW, Some(3), None).is_err());

    let leftover = shape(QUEUED_LEFTOVER_REFUND, None, None).unwrap();
    assert_eq!((leftover.users(), leftover.sizes()), (4, Some(vec![3])));
    assert!(shape(QUEUED_LEFTOVER_REFUND, Some(1), None).is_err());

    assert!(
        shape(QUEUED_SPLIT, None, Some(26)).is_err(),
        "pools hold 25"
    );
    assert!(QueueShape::of("full_lifecycle", &ScenarioConfig::default())
        .unwrap()
        .is_none());
}

#[test]
fn a_queue_takes_its_configured_entry_cap_and_places() {
    let configured = |max_entries, places, max_pool| ScenarioConfig {
        queue_max_entries: max_entries,
        places,
        max_pool_players: max_pool,
        ..Default::default()
    };
    // Two places in the default competition's pool, where the coordinator allows them.
    let two_places = QueueShape::of(QUEUED_ONE_POOL, &configured(None, Some(2), None))
        .unwrap()
        .unwrap();
    assert_eq!((two_places.max_entries, two_places.places), (Some(20), 2));
    // Two places need pools of at most 20; a cap above one pool would split it.
    assert!(QueueShape::of(QUEUED_ONE_POOL, &configured(None, Some(2), Some(25))).is_err());
    assert!(QueueShape::of(QUEUED_ONE_POOL, &configured(Some(21), None, None)).is_err());
    assert!(QueueShape::of(QUEUED_ONE_POOL, &configured(None, Some(3), None)).is_err());
    assert!(QueueShape::of(QUEUED_ONE_POOL, &configured(Some(4), None, None)).is_err());
    assert!(QueueShape::of(QUEUED_SPLIT, &configured(None, Some(2), None)).is_err());
    let split = QueueShape::of(QUEUED_SPLIT, &configured(Some(60), Some(2), Some(20)))
        .unwrap()
        .unwrap();
    assert_eq!((split.max_entries, split.places), (Some(60), 2));
}

#[tokio::test]
async fn default_queue_requests_twenty_seats_three_minimum_and_one_place_before_any_payment() {
    use axum::{
        extract::State,
        routing::{get, post},
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    async fn create(
        State(state): State<Arc<Mutex<Value>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *state.lock().unwrap() = body.clone();
        Json(
            json!({"id": body["id"], "created_at": "2026-10-05T00:00:00Z",
            "event_submission": body, "kind": "queued"}),
        )
    }
    async fn get_queue(State(state): State<Arc<Mutex<Value>>>) -> Json<Value> {
        let body = state.lock().unwrap();
        Json(
            json!({"id": body["id"], "created_at": "2026-10-05T00:00:00Z",
            "event_submission": {"number_of_places_win": body["number_of_places_win"]},
            "kind": "queued", "max_entries": body["max_entries"],
            "pool_rules": {"min_players": body["min_players"], "max_players": body["max_pool_size"]}}),
        )
    }
    let state = Arc::new(Mutex::new(Value::Null));
    let app = Router::new()
        .route("/api/v1/competitions/queued", post(create))
        .route("/api/v1/competitions/{id}", get(get_queue))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = crate::client::CoordinatorClient::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        None,
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = crate::config::SynthConfig::default()
        .scenario_config()
        .resolve_plan(QUEUED_ONE_POOL)
        .unwrap();
    let shape = QueueShape::of(QUEUED_ONE_POOL, &config).unwrap().unwrap();
    let id = create_queue(&client, &config, &shape).await.unwrap();
    {
        let body = state.lock().unwrap();
        assert_eq!(body["min_players"], 3);
        assert_eq!(body["max_pool_size"], 20);
        assert_eq!(body["max_entries"], 20);
        assert_eq!(body["number_of_places_win"], 1);
    }
    check_queue(&client, &id, &shape).await.unwrap();
    // Refuse an older or misconfigured coordinator that silently ignored one of the terms.
    for (field, wrong) in [
        ("min_players", 2),
        ("max_pool_size", 25),
        ("max_entries", 21),
        ("number_of_places_win", 2),
    ] {
        let original = state.lock().unwrap()[field].clone();
        state.lock().unwrap()[field] = json!(wrong);
        assert!(
            check_queue(&client, &id, &shape).await.is_err(),
            "accepted incorrect {field}"
        );
        state.lock().unwrap()[field] = original;
    }
    server.abort();
}

/// A coordinator whose queue has already closed: it formed `pools` from the tickets, in order,
/// or was cancelled if `pools` is empty. Its pools are already awaiting their attestation.
mod mock {
    use crate::client::CoordinatorClient;
    use axum::{
        extract::{Path, State},
        http::StatusCode,
        routing::get,
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[derive(Default)]
    pub struct Queue {
        pub id: Uuid,
        /// Each pool's id and tickets, by index.
        pub pools: Vec<(Uuid, Vec<Uuid>)>,
        /// Every entry's ticket and competition.
        pub entries: Vec<(Uuid, Uuid)>,
        /// Tickets whose refund has settled.
        pub refunded: BTreeSet<Uuid>,
        /// Refunds asked about, by competition and ticket.
        pub refund_lookups: Vec<(Uuid, Uuid)>,
        /// Pools cancelled by their kickoff check, by index.
        pub kickoff_failed: BTreeSet<usize>,
        /// Pools that failed for another reason, by index.
        pub failed: BTreeSet<usize>,
        /// Pools whose kickoff check waits for fees to fall until then, and passes after, by index.
        pub fee_wait: std::collections::BTreeMap<usize, time::OffsetDateTime>,
    }

    type Shared = Arc<Mutex<Queue>>;

    pub struct Mock {
        pub client: CoordinatorClient,
        pub state: Shared,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Mock {
        pub async fn new(queue: Queue) -> Self {
            let state = Arc::new(Mutex::new(queue));
            let app = Router::new()
                .route("/api/v1/competitions/{id}", get(competition))
                .route(
                    "/api/v1/competitions/{id}/tickets/{ticket}/refund",
                    get(refund),
                )
                .route("/api/v1/entries", get(entries))
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

    async fn competition(
        State(state): State<Shared>,
        Path(id): Path<Uuid>,
    ) -> (StatusCode, Json<Value>) {
        const AT: &str = "2026-09-27T00:00:00Z";
        let state = state.lock().unwrap();
        if id == state.id {
            let pools: Vec<Value> = state
                .pools
                .iter()
                .enumerate()
                .map(|(index, (pool, tickets))| {
                    json!({"competition_id": pool, "pool_index": index, "players": tickets.len()})
                })
                .collect();
            let formed = !pools.is_empty();
            return (
                StatusCode::OK,
                Json(json!({
                    "id": id, "created_at": AT, "event_submission": {}, "kind": "queued",
                    "pool_rules": {"min_players": 2, "max_players": 25},
                    "pools_formed_at": formed.then_some(AT),
                    "cancelled_at": (!formed).then_some(AT),
                    "pools": pools,
                })),
            );
        }
        match state.pools.iter().position(|(pool, _)| *pool == id) {
            Some(index) if state.kickoff_failed.contains(&index) => (
                StatusCode::OK,
                Json(json!({
                    "id": id, "created_at": AT, "event_submission": {}, "kind": "pool",
                    "parent_id": state.id, "pool_index": index,
                    "escrow_funds_confirmed_at": AT, "failed_at": AT,
                    "kickoff_check": {"players": 3, "min_players": 5, "sat_per_vb": 4,
                        "passed": false},
                })),
            ),
            Some(index)
                if state
                    .fee_wait
                    .get(&index)
                    .is_some_and(|until| time::OffsetDateTime::now_utc() < *until) =>
            {
                let until = state.fee_wait[&index]
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap();
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": id, "created_at": AT, "event_submission": {}, "kind": "pool",
                        "parent_id": state.id, "pool_index": index,
                        "escrow_funds_confirmed_at": AT,
                        "kickoff_check": {"players": 3, "min_players": 5, "sat_per_vb": 4,
                            "passed": false, "retry_until": until},
                    })),
                )
            }
            Some(index) if state.failed.contains(&index) => (
                StatusCode::OK,
                Json(json!({
                    "id": id, "created_at": AT, "event_submission": {}, "kind": "pool",
                    "parent_id": state.id, "pool_index": index,
                    "escrow_funds_confirmed_at": AT, "failed_at": AT,
                })),
            ),
            Some(index) => (
                StatusCode::OK,
                Json(json!({
                    "id": id, "created_at": AT, "event_submission": {}, "kind": "pool",
                    "parent_id": state.id, "pool_index": index,
                    "escrow_funds_confirmed_at": AT, "awaiting_attestation_at": AT,
                })),
            ),
            None => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))),
        }
    }

    /// Every player's entries: the scenario looks each ticket up by id.
    async fn entries(State(state): State<Shared>) -> Json<Value> {
        let state = state.lock().unwrap();
        Json(json!(state
            .entries
            .iter()
            .map(|(ticket, event)| json!({
                "id": ticket, "event_id": event, "ticket_id": ticket, "pubkey": "mock",
                "ephemeral_pubkey": "mock", "signed_at": null, "paid_at": null,
                "paid_out_at": null,
            }))
            .collect::<Vec<_>>()))
    }

    async fn refund(
        State(state): State<Shared>,
        Path((competition, ticket)): Path<(Uuid, Uuid)>,
    ) -> Json<Value> {
        let mut state = state.lock().unwrap();
        state.refund_lookups.push((competition, ticket));
        Json(if state.refunded.contains(&ticket) {
            json!({"state": "settled", "paid_sats": 1000, "ark_txid": "mock", "updated_at": 0})
        } else {
            Value::Null
        })
    }
}

struct Run {
    mock: mock::Mock,
    users: Vec<SynthUser>,
    traces: Vec<EntryTrace>,
    config: ScenarioConfig,
    shape: QueueShape,
}

impl Run {
    /// `scenario`'s players have paid and entered, or paid and left, and its queue has closed
    /// into `pools` of the complete tickets, in order.
    async fn new(scenario: &str, pools: &[usize]) -> Self {
        let config = ScenarioConfig {
            poll_interval_secs: 0,
            state_timeout_secs: 2,
            refund_timeout_secs: 2,
            ..Default::default()
        };
        let shape = QueueShape::of(scenario, &config).unwrap().unwrap();
        let users: Vec<SynthUser> = (0..shape.users())
            .map(|index| SynthUser::new_random(&format!("user_{index}")).unwrap())
            .collect();
        let traces: Vec<EntryTrace> = users
            .iter()
            .enumerate()
            .map(|(index, user)| {
                let mut trace = EntryTrace::new(user);
                trace.ticket_id = Some(Uuid::now_v7());
                trace.paid = true;
                trace.entry_submitted = index < shape.players;
                trace
            })
            .collect();
        let queue = Uuid::now_v7();
        let mut complete = traces
            .iter()
            .filter(|trace| trace.entry_submitted)
            .map(|trace| trace.ticket_id.unwrap());
        let pools: Vec<(Uuid, Vec<Uuid>)> = pools
            .iter()
            .map(|size| (Uuid::now_v7(), complete.by_ref().take(*size).collect()))
            .collect();
        let mut entries: Vec<(Uuid, Uuid)> = pools
            .iter()
            .flat_map(|(pool, tickets)| tickets.iter().map(move |ticket| (*ticket, *pool)))
            .collect();
        // Entries no pool took stay on the queue.
        entries.extend(complete.map(|ticket| (ticket, queue)));
        let mock = mock::Mock::new(mock::Queue {
            id: queue,
            pools,
            entries,
            ..Default::default()
        })
        .await;
        Self {
            mock,
            users,
            traces,
            config,
            shape,
        }
    }

    fn queue(&self) -> Uuid {
        self.mock.state.lock().unwrap().id
    }

    fn refund_everyone(&self) {
        let tickets = self.traces.iter().filter_map(|trace| trace.ticket_id);
        self.mock.state.lock().unwrap().refunded.extend(tickets);
    }

    /// Every wait is bounded by the config's short timeouts; this bounds the run as a whole too.
    async fn after_entries(&self) -> (Steps, std::result::Result<(), Box<StepResult>>) {
        let mut steps = Steps::new();
        let queue = self.queue();
        let run = after_entries(
            &self.mock.client,
            &self.users,
            &queue,
            &self.config,
            &self.shape,
            OffsetDateTime::now_utc(),
            &self.traces,
            &mut steps,
        );
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), run)
            .await
            .expect("a queued run against the mock finishes");
        (steps, result)
    }
}

#[tokio::test]
async fn a_split_queue_is_checked_and_each_pool_followed_to_its_attestation() {
    let run = Run::new(QUEUED_SPLIT, &[14, 13]).await;
    let (steps, result) = run.after_entries().await;
    assert!(result.is_ok(), "{:?}", result.err());
    let names = steps.names();
    assert_eq!(&names[..2], ["wait_pools_formed", "verify_pools"]);
    assert_eq!(names.last(), Some(&"wait_pools_awaiting_attestation"));
    assert!(
        run.mock.state.lock().unwrap().refund_lookups.is_empty(),
        "every ticket found a pool"
    );
}

#[tokio::test]
async fn an_entry_left_on_the_queue_fails_the_split() {
    // 27 complete tickets, but the pools the queue lists hold only 26 of them.
    let run = Run::new(QUEUED_SPLIT, &[13, 13]).await;
    let (_, result) = run.after_entries().await;
    let failed = result.unwrap_err();
    assert_eq!(failed.name, "verify_pools");
}

#[tokio::test]
async fn a_leftover_ticket_is_refunded_from_the_queue_while_its_pool_runs() {
    let run = Run::new(QUEUED_LEFTOVER_REFUND, &[3]).await;
    run.refund_everyone();
    let (steps, result) = run.after_entries().await;
    assert!(result.is_ok(), "{:?}", result.err());
    assert!(steps.names().contains(&"wait_pools_awaiting_attestation"));
    let leftover = run.traces.last().unwrap();
    let queue = run.queue();
    let lookups = run.mock.state.lock().unwrap().refund_lookups.clone();
    assert_eq!(
        lookups,
        [(queue, leftover.ticket_id.unwrap())],
        "only the ticket without an entry is refunded, from the queue"
    );
}

#[tokio::test]
async fn a_queue_too_small_for_a_pool_is_cancelled_and_refunds_everyone() {
    let run = Run::new(QUEUED_TOO_FEW, &[]).await;
    run.refund_everyone();
    let (steps, result) = run.after_entries().await;
    assert!(result.is_ok(), "{:?}", result.err());
    assert_eq!(&steps.names()[..2], ["wait_cancelled", "verify_no_pools"]);
    let lookups = run.mock.state.lock().unwrap().refund_lookups.clone();
    assert_eq!(lookups.len(), 2);
    assert!(lookups
        .iter()
        .all(|(competition, _)| *competition == run.queue()));
}

/// Synth's own two players formed a pool below the minimum of three: the split is wrong.
#[tokio::test]
async fn a_queue_that_forms_a_pool_too_small_for_its_rules_fails() {
    let run = Run::new(QUEUED_TOO_FEW, &[2]).await;
    let (steps, result) = run.after_entries().await;
    assert_eq!(steps.names(), ["wait_cancelled", "wait_pools_formed"]);
    assert_eq!(result.unwrap_err().name, "verify_pools");
}

/// Synth's two players are too few for a pool, but other people entered too, from one to many:
/// the queue forms pools and they run.
#[tokio::test]
async fn other_players_can_make_a_too_small_queue_form_pools() {
    for others in [1, 3, 10] {
        let run = Run::new(QUEUED_TOO_FEW, &[2]).await;
        run.mock.state.lock().unwrap().pools[0]
            .1
            .extend((0..others).map(|_| Uuid::now_v7()));
        let (steps, result) = run.after_entries().await;
        assert!(result.is_ok(), "{others} others: {:?}", result.err());
        let names = steps.names();
        assert_eq!(
            &names[..3],
            ["wait_cancelled", "wait_pools_formed", "verify_pools"]
        );
        assert_eq!(names.last(), Some(&"wait_pools_awaiting_attestation"));
    }
}

/// Fees rose after the run drew its players, and one pool's kickoff check cancelled it: its
/// players' refunds are collected at the pool, and the other pool still runs.
#[tokio::test]
async fn a_pool_cancelled_by_its_kickoff_check_is_refunded_while_the_others_run() {
    let run = Run::new(QUEUED_SPLIT, &[14, 13]).await;
    run.refund_everyone();
    let pool = run.mock.state.lock().unwrap().pools[1].clone();
    run.mock.state.lock().unwrap().kickoff_failed.insert(1);
    let (steps, result) = run.after_entries().await;
    assert!(result.is_ok(), "{:?}", result.err());
    let names = steps.names();
    assert!(names.contains(&"pool_1_kickoff_failed"), "{names:?}");
    assert_eq!(names.last(), Some(&"wait_pools_awaiting_attestation"));
    let lookups = run.mock.state.lock().unwrap().refund_lookups.clone();
    let refunded: BTreeSet<Uuid> = lookups.iter().map(|(_, ticket)| *ticket).collect();
    assert_eq!(
        refunded,
        pool.1.iter().copied().collect(),
        "the cancelled pool's tickets"
    );
    assert!(lookups
        .iter()
        .all(|(competition, _)| *competition == pool.0));

    // Every pool cancelled: all are refunded, and the run still passes.
    let run = Run::new(QUEUED_SPLIT, &[14, 13]).await;
    run.refund_everyone();
    run.mock.state.lock().unwrap().kickoff_failed.extend([0, 1]);
    let (steps, result) = run.after_entries().await;
    assert!(result.is_ok(), "{:?}", result.err());
    assert_eq!(run.mock.state.lock().unwrap().refund_lookups.len(), 27);
    assert!(steps.names().contains(&"pool_0_kickoff_failed"));
}

/// A pool that failed for any other reason still fails the run.
#[tokio::test]
async fn a_pool_that_fails_otherwise_fails_the_run() {
    let run = Run::new(QUEUED_SPLIT, &[14, 13]).await;
    run.mock.state.lock().unwrap().failed.insert(0);
    let (_, result) = run.after_entries().await;
    assert_eq!(result.unwrap_err().name, "wait_pools_event_created");
}

/// A pool's kickoff check waits for fees to fall, longer than synth waits for a state, and then
/// passes: synth waits with it and follows the pool on, with nothing refunded.
#[tokio::test]
async fn a_pool_waiting_for_fees_to_fall_is_waited_for() {
    let run = Run::new(QUEUED_SPLIT, &[14, 13]).await;
    let until = time::OffsetDateTime::now_utc() + time::Duration::seconds(3);
    run.mock.state.lock().unwrap().fee_wait.insert(1, until);
    let (steps, result) = run.after_entries().await;
    assert!(result.is_ok(), "{:?}", result.err());
    assert!(time::OffsetDateTime::now_utc() >= until);
    assert!(!steps
        .names()
        .iter()
        .any(|name| name.contains("kickoff_failed")));
    assert!(run.mock.state.lock().unwrap().refund_lookups.is_empty());
}
