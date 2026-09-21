//! One task per competition, woken by the events it waits for.
//!
//! Each runner drives one competition through the typestate machine, one step at a time, while
//! its process holds the competition's lease. A slow step, such as an Arkade batch or an oracle
//! retry, holds up only its own competition. Between steps a runner sleeps until an event wakes
//! it or its state's next check is due. A sweep starts runners that are missing, after a restart
//! or a crash, and runs the cleanup of dead competitions.
//!
//! Two coordinators can run at once against one database, as during a blue/green deploy. Only
//! the lease holder drives a competition, and its state writes are fenced on the lease. On
//! shutdown a runner finishes its step and releases the lease, so the other process takes over
//! within `lease_retry`. A crashed process's leases pass on when they expire.
//! See `docs/COMPETITION_RUNNERS.md`.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::{mapref::entry::Entry, DashMap};
use futures::FutureExt;
use log::{error, info, warn};
use time::OffsetDateTime;
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use super::{CompetitionStore, Lease};

/// How runners pace their work.
#[derive(Debug, Clone)]
pub struct Pacing {
    /// The longest a waiting competition sleeps before it is checked again, unless its state
    /// says otherwise.
    pub idle: Duration,
    /// How often the sweep starts missing runners and cleans up dead competitions.
    pub sweep: Duration,
    /// How long a lease lasts without renewal. Runners renew at a third of this.
    pub lease_ttl: Duration,
    /// How often a runner retries a lease another coordinator holds.
    pub lease_retry: Duration,
    /// The longest backoff after repeated failed steps.
    pub max_backoff: Duration,
    /// How many steps run at once across all competitions.
    pub max_concurrent_steps: usize,
    /// How many immediate steps a runner takes before it yields.
    pub max_immediate_steps: usize,
    /// How often a coordinator picks up wakes another coordinator recorded.
    pub wake_poll: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(15),
            sweep: Duration::from_secs(60),
            lease_ttl: Duration::from_secs(30),
            lease_retry: Duration::from_secs(2),
            max_backoff: Duration::from_secs(300),
            max_concurrent_steps: 8,
            max_immediate_steps: 10,
            wake_poll: Duration::from_secs(1),
        }
    }
}

/// What a runner does after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The competition has more to do.
    Next(Wait),
    /// The competition has no lifecycle work left.
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Run the next step now.
    Now,
    /// Sleep until woken, or until this time at the latest.
    Until(OffsetDateTime),
}

#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// Another coordinator holds the competition now; its state was not written.
    #[error("another coordinator took the lease")]
    LeaseLost,
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// The work the runners drive. `Coordinator` implements it.
#[async_trait]
pub trait CompetitionSteps: Send + Sync + 'static {
    /// Advance a competition by one state, saving it only while `lease` holds it.
    async fn step(
        &self,
        competition_id: Uuid,
        lease: &Lease,
        pacing: &Pacing,
    ) -> Result<Step, StepError>;

    /// The competitions that still have lifecycle work.
    async fn active_competitions(&self) -> Result<Vec<Uuid>, anyhow::Error>;

    /// Release held invoices and reclaim escrows of dead competitions.
    async fn clean_up(&self) -> Result<(), anyhow::Error>;
}

/// Wakes competition runners. Event sources hold a clone.
#[derive(Clone)]
pub struct CompetitionWakes {
    signals: Arc<DashMap<Uuid, Arc<Notify>>>,
    starts: mpsc::UnboundedSender<Uuid>,
}

impl Default for CompetitionWakes {
    /// Wakes that no runners listen to, for a coordinator without runners.
    fn default() -> Self {
        Self::new().0
    }
}

impl CompetitionWakes {
    /// The wakes, and the requests to start runners that `CompetitionRunners` consumes.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Uuid>) {
        let (starts, requests) = mpsc::unbounded_channel();
        (
            Self {
                signals: Arc::default(),
                starts,
            },
            requests,
        )
    }

    /// Run the competition's next step soon, starting its runner if needed.
    ///
    /// A wake during a step is kept, so the runner steps again straight after.
    /// Call it after committing the change the competition should see.
    pub fn wake(&self, competition_id: Uuid) {
        self.signal(competition_id).notify_one();
        // Without runners nobody receives this, which is fine.
        let _ = self.starts.send(competition_id);
    }

    fn signal(&self, competition_id: Uuid) -> Arc<Notify> {
        self.signals
            .entry(competition_id)
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    fn forget(&self, competition_id: Uuid) {
        self.signals.remove(&competition_id);
    }
}

/// The runners of one coordinator process.
pub struct CompetitionRunners {
    steps: Arc<dyn CompetitionSteps>,
    store: Arc<CompetitionStore>,
    wakes: CompetitionWakes,
    /// This process, as a lease holder. Unique per process start.
    holder: String,
    pacing: Pacing,
    running: DashMap<Uuid, JoinHandle<()>>,
    permits: Arc<Semaphore>,
    cleaning: AtomicBool,
    tracker: TaskTracker,
    cancel: CancellationToken,
}

/// Wakes older than this are pruned; every coordinator has polled them long before.
const WAKE_RETENTION: Duration = Duration::from_secs(3600);

const CLEANUP_LEASE: &str = "worker:competition-cleanup";

impl CompetitionRunners {
    pub fn new(
        steps: Arc<dyn CompetitionSteps>,
        store: Arc<CompetitionStore>,
        wakes: CompetitionWakes,
        holder: String,
        pacing: Pacing,
        tracker: TaskTracker,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(pacing.max_concurrent_steps.max(1))),
            steps,
            store,
            wakes,
            holder,
            pacing,
            running: DashMap::new(),
            cleaning: AtomicBool::new(false),
            tracker,
            cancel,
        })
    }

    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// Start runners on request and on each sweep, and pass wakes between coordinators, until
    /// shutdown.
    ///
    /// Each local wake is recorded, so a competition another coordinator drives sees it at that
    /// coordinator's next poll.
    pub async fn supervise(
        self: Arc<Self>,
        mut starts: mpsc::UnboundedReceiver<Uuid>,
    ) -> Result<(), anyhow::Error> {
        info!("Competition runners started as {}", self.holder);
        let mut sweep = tokio::time::interval(self.pacing.sweep);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut poll = tokio::time::interval(self.pacing.wake_poll);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_wake = self.store.last_wake().await?;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                Some(competition_id) = starts.recv() => {
                    self.ensure(competition_id);
                    if let Err(e) = self.store.record_wake(competition_id, &self.holder).await {
                        warn!("Cannot share the wake of competition {competition_id}: {e}");
                    }
                }
                _ = poll.tick() => match self.store.wakes_since(last_wake, &self.holder).await {
                    Ok((woken, last)) => {
                        last_wake = last;
                        for competition_id in woken {
                            self.wakes.signal(competition_id).notify_one();
                            self.ensure(competition_id);
                        }
                    }
                    Err(e) => warn!("Cannot read other coordinators' wakes: {e}"),
                },
                _ = sweep.tick() => self.sweep(),
            }
        }
        info!("Competition runners stopping; each finishes its step first");
        Ok(())
    }

    /// Start a runner for every active competition, and clean up dead ones, in the background.
    fn sweep(self: &Arc<Self>) {
        if self.cleaning.swap(true, Ordering::AcqRel) {
            return;
        }
        let runners = self.clone();
        self.tracker.spawn(async move {
            runners.sweep_now().await;
            runners.cleaning.store(false, Ordering::Release);
        });
    }

    async fn sweep_now(self: &Arc<Self>) {
        match self.steps.active_competitions().await {
            Ok(competitions) => {
                for competition_id in competitions {
                    self.ensure(competition_id);
                }
            }
            Err(e) => error!("Cannot list active competitions: {e:#}"),
        }
        if let Err(e) = self.store.prune_wakes(WAKE_RETENTION).await {
            warn!("Cannot prune shared wakes: {e}");
        }
        // Cleanup has side effects on LND and the chain, so one process does it at a time.
        let lease = match self
            .store
            .acquire_lease(CLEANUP_LEASE, &self.holder, self.pacing.lease_ttl)
            .await
        {
            Ok(Some(lease)) => lease,
            Ok(None) => return,
            Err(e) => {
                warn!("Cannot take the competition cleanup lease: {e}");
                return;
            }
        };
        match self.leased(&lease, self.steps.clean_up()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!("Competition cleanup failed: {e:#}"),
            Err(StepError::LeaseLost) => warn!("Lost the competition cleanup lease mid-run"),
            Err(StepError::Failed(e)) => error!("Competition cleanup failed: {e:#}"),
        }
    }

    /// Start a runner for the competition, unless one is running.
    pub fn ensure(self: &Arc<Self>, competition_id: Uuid) {
        if self.cancel.is_cancelled() {
            return;
        }
        match self.running.entry(competition_id) {
            Entry::Occupied(mut running) if running.get().is_finished() => {
                running.insert(self.spawn(competition_id));
            }
            Entry::Occupied(_) => {}
            Entry::Vacant(missing) => {
                missing.insert(self.spawn(competition_id));
            }
        }
    }

    fn spawn(self: &Arc<Self>, competition_id: Uuid) -> JoinHandle<()> {
        let runners = self.clone();
        self.tracker.spawn(async move {
            // A panic stops only this runner; the next sweep starts it again.
            if AssertUnwindSafe(runners.clone().run(competition_id))
                .catch_unwind()
                .await
                .is_err()
            {
                error!("Runner for competition {competition_id} panicked; the sweep restarts it");
            }
        })
    }

    async fn run(self: Arc<Self>, competition_id: Uuid) {
        let signal = self.wakes.signal(competition_id);
        let resource = Lease::competition_resource(competition_id);
        let mut lease: Option<Lease> = None;
        let mut failures: u32 = 0;
        let mut immediate = 0;
        let mut finished = false;
        while !self.cancel.is_cancelled() {
            let held = match lease.take() {
                Some(held) => held,
                None => match self
                    .store
                    .acquire_lease(&resource, &self.holder, self.pacing.lease_ttl)
                    .await
                {
                    Ok(Some(held)) => held,
                    Ok(None) => {
                        // Another coordinator drives it; take over once it lets go.
                        if !self.sleep(&signal, self.pacing.lease_retry).await {
                            break;
                        }
                        continue;
                    }
                    Err(e) => {
                        failures += 1;
                        warn!("Cannot take the lease on competition {competition_id}: {e}");
                        if !self.sleep(&signal, self.backoff(failures)).await {
                            break;
                        }
                        continue;
                    }
                },
            };

            let permit = self
                .permits
                .clone()
                .acquire_owned()
                .await
                .expect("the step semaphore is never closed");
            // This step sees every change woken so far; only a wake during it asks for another.
            let _ = signal.notified().now_or_never();
            let step = self
                .leased(&held, self.steps.step(competition_id, &held, &self.pacing))
                .await
                .and_then(|step| step);
            drop(permit);

            let wait = match step {
                Ok(Step::Finished) => {
                    finished = true;
                    lease = Some(held);
                    break;
                }
                Ok(Step::Next(wait)) => {
                    failures = 0;
                    lease = Some(held);
                    wait
                }
                Err(StepError::LeaseLost) => {
                    warn!("Competition {competition_id} moved to another coordinator");
                    immediate = 0;
                    if !self.sleep(&signal, self.pacing.lease_retry).await {
                        break;
                    }
                    continue;
                }
                Err(StepError::Failed(e)) => {
                    failures += 1;
                    error!("Competition {competition_id} step failed: {e:#}");
                    lease = Some(held);
                    Wait::Until(OffsetDateTime::now_utc() + self.backoff(failures))
                }
            };

            let deadline = match wait {
                Wait::Now if immediate < self.pacing.max_immediate_steps => {
                    immediate += 1;
                    continue;
                }
                // A state that keeps asking to run at once yields for an idle period.
                Wait::Now => OffsetDateTime::now_utc() + self.pacing.idle,
                Wait::Until(deadline) => deadline,
            };
            immediate = 0;
            let held = lease.as_ref().expect("held after a step");
            match self.leased(held, self.sleep_until(&signal, deadline)).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => {
                    warn!("Competition {competition_id} moved to another coordinator while idle");
                    lease = None;
                }
            }
        }

        if let Some(held) = lease {
            if let Err(e) = self.store.release_lease(&held).await {
                warn!("Cannot release the lease on competition {competition_id}: {e}");
            }
        }
        if finished {
            self.wakes.forget(competition_id);
        }
        self.running.remove(&competition_id);
    }

    async fn leased<F: std::future::Future>(
        &self,
        lease: &Lease,
        work: F,
    ) -> Result<F::Output, StepError> {
        super::while_leased(&self.store, lease, self.pacing.lease_ttl, work)
            .await
            .map_err(|_| StepError::LeaseLost)
    }

    /// Sleep until woken or `deadline`. False on shutdown.
    async fn sleep_until(&self, signal: &Notify, deadline: OffsetDateTime) -> bool {
        let duration = (deadline - OffsetDateTime::now_utc())
            .try_into()
            .unwrap_or(Duration::ZERO);
        self.sleep(signal, duration).await
    }

    /// Sleep until woken or `duration` passes. False on shutdown.
    async fn sleep(&self, signal: &Notify, duration: Duration) -> bool {
        tokio::select! {
            _ = signal.notified() => true,
            _ = tokio::time::sleep(duration) => true,
            _ = self.cancel.cancelled() => false,
        }
    }

    fn backoff(&self, failures: u32) -> Duration {
        let base = Duration::from_secs(5);
        base.saturating_mul(1 << failures.saturating_sub(1).min(16))
            .min(self.pacing.max_backoff)
    }
}

#[cfg(test)]
#[path = "runners_tests.rs"]
mod tests;
