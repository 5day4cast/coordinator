//! The worker: the lease holder advances swaps and refunds, and boards the wallet's on-chain coins.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::swap::Swapper;

/// The steps the worker takes. `Swapper` takes them; the tests stand in for it.
pub trait Steps: Send + Sync + 'static {
    fn tick(&self) -> impl Future<Output = ()> + Send;
    fn refund_tick(&self) -> impl Future<Output = ()> + Send;
    fn lookup_tick(&self) -> impl Future<Output = ()> + Send;
    fn board_tick(&self) -> impl Future<Output = ()> + Send;
}

impl Steps for Swapper {
    fn tick(&self) -> impl Future<Output = ()> + Send {
        Swapper::tick(self)
    }

    fn refund_tick(&self) -> impl Future<Output = ()> + Send {
        Swapper::refund_tick(self)
    }

    fn lookup_tick(&self) -> impl Future<Output = ()> + Send {
        Swapper::lookup_tick(self)
    }

    fn board_tick(&self) -> impl Future<Output = ()> + Send {
        Swapper::board_tick(self)
    }
}

/// How often the worker takes each step.
#[derive(Clone, Copy)]
pub struct Cadence {
    /// How often unfinished swaps and paid refunds advance.
    pub tick: Duration,
    /// How often settled swaps whose escrow VTXO is unknown are checked for lookups that are due.
    pub lookup_every: Duration,
    /// How often a boarding starts, once the last one has ended.
    pub board_every: Duration,
    /// How long a boarding may take before it is given up.
    pub board_timeout: Duration,
}

/// Take the worker's steps while `holding` says this instance holds the lease, until `stopped`
/// changes. Returns once the last step, and any boarding in flight, has finished.
pub async fn run<S: Steps>(
    steps: Arc<S>,
    cadence: Cadence,
    holding: Arc<AtomicBool>,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    // A boarding waits for a whole batch, minutes, so it runs beside the other steps, one boarding
    // at a time. The wallet's send lock still puts its spends in turn with theirs.
    let boarding = Arc::new(tokio::sync::Mutex::new(()));
    let mut interval = tokio::time::interval(cadence.tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut boarded_at = tokio::time::Instant::now();
    let mut looked_up_at = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = stopped.changed() => break,
        }
        if !holding.load(Ordering::SeqCst) {
            continue;
        }
        // A tick always finishes, so a payment in flight records its result.
        steps.tick().await;
        steps.refund_tick().await;
        if looked_up_at.elapsed() >= cadence.lookup_every {
            steps.lookup_tick().await;
            looked_up_at = tokio::time::Instant::now();
        }
        if boarded_at.elapsed() >= cadence.board_every {
            if let Ok(one_at_a_time) = boarding.clone().try_lock_owned() {
                let steps = steps.clone();
                tokio::spawn(async move {
                    let _one_at_a_time = one_at_a_time;
                    if tokio::time::timeout(cadence.board_timeout, steps.board_tick())
                        .await
                        .is_err()
                    {
                        log::warn!(
                            "gave up a boarding after {}s",
                            cadence.board_timeout.as_secs()
                        );
                    }
                });
                boarded_at = tokio::time::Instant::now();
            }
        }
    }
    // The lease is handed over only after a boarding in flight ends, so two instances never
    // board the same coins.
    drop(boarding.lock().await);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Counts the steps taken. A boarding lasts until it is released or given up.
    struct Counted {
        ticks: AtomicUsize,
        refund_ticks: AtomicUsize,
        lookups: AtomicUsize,
        boardings: AtomicUsize,
        boarding_now: AtomicUsize,
        most_boarding_at_once: AtomicUsize,
        release: tokio::sync::Semaphore,
    }

    impl Default for Counted {
        fn default() -> Self {
            Self {
                ticks: AtomicUsize::new(0),
                refund_ticks: AtomicUsize::new(0),
                lookups: AtomicUsize::new(0),
                boardings: AtomicUsize::new(0),
                boarding_now: AtomicUsize::new(0),
                most_boarding_at_once: AtomicUsize::new(0),
                release: tokio::sync::Semaphore::new(0),
            }
        }
    }

    /// Marks a boarding ended, however its future ends.
    struct Boarding<'a>(&'a AtomicUsize);

    impl Drop for Boarding<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Steps for Counted {
        async fn tick(&self) {
            self.ticks.fetch_add(1, Ordering::SeqCst);
        }

        async fn refund_tick(&self) {
            self.refund_ticks.fetch_add(1, Ordering::SeqCst);
        }

        async fn lookup_tick(&self) {
            self.lookups.fetch_add(1, Ordering::SeqCst);
        }

        async fn board_tick(&self) {
            self.boardings.fetch_add(1, Ordering::SeqCst);
            let now = self.boarding_now.fetch_add(1, Ordering::SeqCst) + 1;
            let _ended = Boarding(&self.boarding_now);
            self.most_boarding_at_once.fetch_max(now, Ordering::SeqCst);
            self.release.acquire().await.expect("never closed").forget();
        }
    }

    fn cadence(board_timeout: Duration) -> Cadence {
        Cadence {
            tick: Duration::from_millis(10),
            lookup_every: Duration::from_millis(20),
            board_every: Duration::from_millis(20),
            board_timeout,
        }
    }

    fn start(
        steps: &Arc<Counted>,
        cadence: Cadence,
    ) -> (
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(run(
            steps.clone(),
            cadence,
            Arc::new(AtomicBool::new(true)),
            stopped,
        ));
        (stop, worker)
    }

    #[tokio::test]
    async fn the_other_steps_run_while_a_boarding_is_in_flight() {
        let steps = Arc::new(Counted::default());
        let (stop, worker) = start(&steps, cadence(Duration::from_secs(60)));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(steps.boardings.load(Ordering::SeqCst), 1);
        let ticks = steps.ticks.load(Ordering::SeqCst);
        let lookups = steps.lookups.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(steps.ticks.load(Ordering::SeqCst) >= ticks + 5);
        assert!(steps.refund_ticks.load(Ordering::SeqCst) >= ticks + 5);
        assert!(steps.lookups.load(Ordering::SeqCst) > lookups);
        assert_eq!(
            steps.boardings.load(Ordering::SeqCst),
            1,
            "a second boarding waits for the first"
        );

        // Once it ends, the next one starts.
        steps.release.add_permits(1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(steps.boardings.load(Ordering::SeqCst), 2);
        assert_eq!(steps.most_boarding_at_once.load(Ordering::SeqCst), 1);

        // Stopping waits for the boarding in flight.
        stop.send(true).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!worker.is_finished());
        steps.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_boarding_that_outlasts_its_timeout_is_given_up() {
        let steps = Arc::new(Counted::default());
        let (stop, worker) = start(&steps, cadence(Duration::from_millis(50)));

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(steps.boardings.load(Ordering::SeqCst) >= 2);
        assert_eq!(steps.most_boarding_at_once.load(Ordering::SeqCst), 1);

        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
    }
}
