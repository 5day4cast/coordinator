use super::*;
use crate::domain::competitions::{Competition, CreateEvent};
use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tempfile::TempDir;

async fn open(directory: &TempDir) -> DBConnection {
    DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap()
}

fn fast() -> Pacing {
    Pacing {
        idle: Duration::from_millis(200),
        sweep: Duration::from_millis(100),
        lease_ttl: Duration::from_millis(900),
        lease_retry: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        max_concurrent_steps: 16,
        max_immediate_steps: 10,
        wake_poll: Duration::from_millis(50),
    }
}

/// Records every step, and every time two steps of one competition overlapped.
#[derive(Default)]
struct FakeSteps {
    active: Mutex<Vec<Uuid>>,
    delays: Mutex<HashMap<Uuid, Duration>>,
    /// Finish a competition after this many steps.
    finish_after: Mutex<HashMap<Uuid, usize>>,
    panic_once: Mutex<HashSet<Uuid>>,
    /// Between steps, sleep this long; without it, wait for a wake.
    pause: Mutex<Option<Duration>>,
    in_step: Mutex<HashSet<Uuid>>,
    overlaps: AtomicUsize,
    log: Mutex<Vec<(Uuid, String)>>,
}

impl FakeSteps {
    fn steps(&self, competition_id: Uuid) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == competition_id)
            .map(|(_, holder)| holder.clone())
            .collect()
    }
}

#[async_trait]
impl CompetitionSteps for FakeSteps {
    async fn step(&self, id: Uuid, lease: &Lease, _: &Pacing) -> Result<Step, StepError> {
        if self.panic_once.lock().unwrap().remove(&id) {
            panic!("a step for {id} panicked");
        }
        if !self.in_step.lock().unwrap().insert(id) {
            self.overlaps.fetch_add(1, Ordering::SeqCst);
        }
        let delay = self.delays.lock().unwrap().get(&id).copied();
        tokio::time::sleep(delay.unwrap_or(Duration::from_millis(5))).await;
        self.in_step.lock().unwrap().remove(&id);
        let taken = {
            let mut log = self.log.lock().unwrap();
            log.push((id, lease.holder.clone()));
            log.iter().filter(|(logged, _)| *logged == id).count()
        };
        if self
            .finish_after
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|last| taken >= *last)
        {
            return Ok(Step::Finished);
        }
        let pause = self
            .pause
            .lock()
            .unwrap()
            .unwrap_or(Duration::from_secs(3600));
        Ok(Step::Next(Wait::Until(OffsetDateTime::now_utc() + pause)))
    }

    async fn active_competitions(&self) -> Result<Vec<Uuid>, anyhow::Error> {
        Ok(self.active.lock().unwrap().clone())
    }

    async fn clean_up(&self) -> Result<(), anyhow::Error> {
        Ok(())
    }
}

/// One coordinator process: its own database pools, runners, and shutdown token.
struct Process {
    runners: Arc<CompetitionRunners>,
    wakes: CompetitionWakes,
    store: Arc<CompetitionStore>,
    tracker: TaskTracker,
    cancel: CancellationToken,
}

impl Process {
    async fn start(directory: &TempDir, name: &str, steps: Arc<FakeSteps>) -> Self {
        let store = Arc::new(CompetitionStore::new(open(directory).await));
        let (wakes, requests) = CompetitionWakes::new();
        let tracker = TaskTracker::new();
        let cancel = CancellationToken::new();
        let runners = CompetitionRunners::new(
            steps,
            store.clone(),
            wakes.clone(),
            format!("{name}-{}", Uuid::now_v7()),
            fast(),
            tracker.clone(),
            cancel.clone(),
        );
        tracker.spawn(runners.clone().supervise(requests));
        Self {
            runners,
            wakes,
            store,
            tracker,
            cancel,
        }
    }

    async fn stop(self) {
        self.cancel.cancel();
        self.tracker.close();
        tokio::time::timeout(Duration::from_secs(10), self.tracker.wait())
            .await
            .expect("runners stop after their steps");
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_competition_does_not_hold_up_another() {
    let directory = tempfile::tempdir().unwrap();
    let steps = Arc::new(FakeSteps::default());
    let (slow, quick) = (Uuid::now_v7(), Uuid::now_v7());
    steps
        .delays
        .lock()
        .unwrap()
        .insert(slow, Duration::from_secs(5));
    steps.finish_after.lock().unwrap().insert(quick, 3);
    *steps.pause.lock().unwrap() = Some(Duration::from_millis(10));
    let process = Process::start(&directory, "blue", steps.clone()).await;

    process.wakes.wake(slow);
    process.wakes.wake(quick);
    eventually("the quick competition finishes", || {
        steps.steps(quick).len() == 3
    })
    .await;
    assert!(
        steps.steps(slow).is_empty(),
        "the slow competition is still in its first step"
    );
    process.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wake_during_a_step_runs_it_again() {
    let directory = tempfile::tempdir().unwrap();
    let steps = Arc::new(FakeSteps::default());
    let competition = Uuid::now_v7();
    steps
        .delays
        .lock()
        .unwrap()
        .insert(competition, Duration::from_millis(300));
    let process = Process::start(&directory, "blue", steps.clone()).await;

    process.wakes.wake(competition);
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(steps.steps(competition).len(), 1, "one wake is one step");
    process.wakes.wake(competition);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The second step is running; this wake must not be lost.
    process.wakes.wake(competition);
    eventually("the third step runs", || {
        steps.steps(competition).len() == 3
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        steps.steps(competition).len(),
        3,
        "without another event, the runner waits"
    );
    process.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_wakes_start_one_runner() {
    let directory = tempfile::tempdir().unwrap();
    let steps = Arc::new(FakeSteps::default());
    let competition = Uuid::now_v7();
    steps
        .delays
        .lock()
        .unwrap()
        .insert(competition, Duration::from_millis(50));
    let process = Process::start(&directory, "blue", steps.clone()).await;

    let wakers: Vec<_> = (0..50)
        .map(|_| {
            let wakes = process.wakes.clone();
            tokio::spawn(async move { wakes.wake(competition) })
        })
        .collect();
    for waker in wakers {
        waker.await.unwrap();
    }
    eventually("the competition steps", || {
        !steps.steps(competition).is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(steps.overlaps.load(Ordering::SeqCst), 0);
    assert_eq!(process.runners.running.len(), 1);
    process.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_coordinators_share_competitions_and_hand_over_on_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let steps = Arc::new(FakeSteps::default());
    let competitions: Vec<Uuid> = (0..6).map(|_| Uuid::now_v7()).collect();
    *steps.active.lock().unwrap() = competitions.clone();
    *steps.pause.lock().unwrap() = Some(Duration::from_millis(30));
    let blue = Process::start(&directory, "blue", steps.clone()).await;
    let green = Process::start(&directory, "green", steps.clone()).await;
    let blue_holder = blue.runners.holder().to_owned();
    let green_holder = green.runners.holder().to_owned();

    eventually("every competition steps", || {
        competitions.iter().all(|id| steps.steps(*id).len() >= 3)
    })
    .await;
    // Whichever process took a competition keeps it while it runs.
    for id in &competitions {
        let holders: HashSet<_> = steps.steps(*id).into_iter().collect();
        assert_eq!(holders.len(), 1, "competition {id} changed hands");
    }

    // Blue stops, as on a deploy: its steps finish, and green takes its competitions over.
    blue.stop().await;
    let stopped = steps.log.lock().unwrap().len();
    eventually("green drives every competition", || {
        competitions.iter().all(|id| {
            steps.log.lock().unwrap()[stopped..]
                .iter()
                .any(|(logged, holder)| logged == id && *holder == green_holder)
        })
    })
    .await;
    assert!(steps.log.lock().unwrap()[stopped..]
        .iter()
        .all(|(_, holder)| *holder != blue_holder));
    assert_eq!(
        steps.overlaps.load(Ordering::SeqCst),
        0,
        "two coordinators stepped one competition at once"
    );
    green.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_coordinators_competitions_move_when_its_lease_expires() {
    let directory = tempfile::tempdir().unwrap();
    let steps = Arc::new(FakeSteps::default());
    let competition = Uuid::now_v7();
    *steps.active.lock().unwrap() = vec![competition];
    let store = CompetitionStore::new(open(&directory).await);
    // A coordinator took the lease, then died without renewing or releasing it.
    let crashed = store
        .acquire_lease(
            &Lease::competition_resource(competition),
            "crashed",
            Duration::from_secs(2),
        )
        .await
        .unwrap()
        .unwrap();

    let started = tokio::time::Instant::now();
    let green = Process::start(&directory, "green", steps.clone()).await;
    eventually("green takes over", || !steps.steps(competition).is_empty()).await;
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "green waited for the lease to expire"
    );
    let taken = green
        .store
        .lease_holder(&Lease::competition_resource(competition))
        .await
        .unwrap()
        .unwrap();
    assert!(taken.token > crashed.token);
    green.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_runner_is_restarted_by_the_sweep() {
    let directory = tempfile::tempdir().unwrap();
    let steps = Arc::new(FakeSteps::default());
    let competition = Uuid::now_v7();
    *steps.active.lock().unwrap() = vec![competition];
    steps.panic_once.lock().unwrap().insert(competition);
    let process = Process::start(&directory, "blue", steps.clone()).await;

    eventually("the competition steps after the panic", || {
        !steps.steps(competition).is_empty()
    })
    .await;
    process.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_lets_a_step_finish_and_releases_the_lease() {
    let directory = tempfile::tempdir().unwrap();
    let steps = Arc::new(FakeSteps::default());
    let competition = Uuid::now_v7();
    steps
        .delays
        .lock()
        .unwrap()
        .insert(competition, Duration::from_millis(500));
    let process = Process::start(&directory, "blue", steps.clone()).await;
    let store = process.store.clone();

    process.wakes.wake(competition);
    tokio::time::sleep(Duration::from_millis(100)).await;
    process.stop().await;
    assert_eq!(steps.steps(competition).len(), 1, "the step finished");
    assert!(store
        .lease_holder(&Lease::competition_resource(competition))
        .await
        .unwrap()
        .is_none());
}

fn event() -> CreateEvent {
    let now = OffsetDateTime::now_utc();
    CreateEvent {
        id: Uuid::now_v7(),
        signing_date: now + time::Duration::hours(3),
        start_observation_date: now + time::Duration::hours(1),
        end_observation_date: now + time::Duration::hours(2),
        locations: vec!["KDEN".into()],
        number_of_values_per_entry: 3,
        number_of_places_win: 1,
        total_allowed_entries: 2,
        entry_fee: 50_000,
        coordinator_fee_percentage: 0,
        total_competition_pool: 100_000,
        relative_locktime_block_delta: Some(72),
        unlisted: false,
    }
}

#[tokio::test]
async fn a_coordinator_that_lost_the_lease_cannot_save_the_competition() {
    let directory = tempfile::tempdir().unwrap();
    let blue = CompetitionStore::new(open(&directory).await);
    let green = CompetitionStore::new(open(&directory).await);
    let competition = Competition::new(&event());
    blue.add_competition_with_tickets(competition.clone(), vec![])
        .await
        .unwrap();
    let resource = Lease::competition_resource(competition.id);
    let ttl = Duration::from_secs(30);

    let blue_lease = blue
        .acquire_lease(&resource, "blue", ttl)
        .await
        .unwrap()
        .unwrap();
    assert!(green
        .acquire_lease(&resource, "green", ttl)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        blue.acquire_lease(&resource, "blue", ttl)
            .await
            .unwrap()
            .unwrap(),
        blue_lease,
        "renewing keeps the token"
    );

    blue.release_lease(&blue_lease).await.unwrap();
    let green_lease = green
        .acquire_lease(&resource, "green", ttl)
        .await
        .unwrap()
        .unwrap();
    assert!(green_lease.token > blue_lease.token);
    assert!(!blue.renew_lease(&blue_lease, ttl).await.unwrap());

    let mut stale = competition.clone();
    stale.failed_at = Some(OffsetDateTime::now_utc());
    assert!(!blue
        .update_competition_fenced(stale, &blue_lease)
        .await
        .unwrap());
    assert!(green
        .update_competition_fenced(competition.clone(), &green_lease)
        .await
        .unwrap());
    assert!(green
        .get_competition(competition.id)
        .await
        .unwrap()
        .failed_at
        .is_none());
}

#[tokio::test]
async fn a_singleton_worker_runs_in_one_coordinator_until_it_stops() {
    let directory = tempfile::tempdir().unwrap();
    let ttl = Duration::from_secs(30);
    let blue = super::super::WorkerLeases::new(
        Arc::new(CompetitionStore::new(open(&directory).await)),
        "blue".into(),
        ttl,
    );
    let green = super::super::WorkerLeases::new(
        Arc::new(CompetitionStore::new(open(&directory).await)),
        "green".into(),
        ttl,
    );

    assert_eq!(blue.tick("payout-watcher", async { 1 }).await, Some(1));
    assert_eq!(
        green.tick("payout-watcher", async { 2 }).await,
        None,
        "blue keeps the worker between ticks"
    );
    assert_eq!(
        green.tick("invoice-watcher", async { 3 }).await,
        Some(3),
        "each worker has its own lease"
    );
    assert_eq!(blue.tick("payout-watcher", async { 4 }).await, Some(4));

    // Blue's worker stops, as on a deploy, and green takes it at its next tick.
    blue.release("payout-watcher").await;
    assert_eq!(green.tick("payout-watcher", async { 5 }).await, Some(5));
    assert_eq!(blue.tick("payout-watcher", async { 6 }).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wake_reaches_the_coordinator_driving_the_competition() {
    let directory = tempfile::tempdir().unwrap();
    let steps = Arc::new(FakeSteps::default());
    let competition = Uuid::now_v7();
    let blue = Process::start(&directory, "blue", steps.clone()).await;
    let green = Process::start(&directory, "green", steps.clone()).await;

    blue.wakes.wake(competition);
    eventually("a coordinator drives the competition", || {
        steps.steps(competition).len() == 1
    })
    .await;
    // Either may have taken it: blue from its own wake, or green from blue's shared wake.
    let driver = steps.steps(competition)[0].clone();
    let other = if driver == blue.runners.holder() {
        &green
    } else {
        &blue
    };
    // A paid ticket handled by the other coordinator wakes the competition.
    other.wakes.wake(competition);
    eventually("the driving coordinator steps it again", || {
        steps.steps(competition) == [driver.clone(), driver.clone()]
    })
    .await;
    assert_eq!(steps.overlaps.load(Ordering::SeqCst), 0);
    blue.stop().await;
    green.stop().await;
}
