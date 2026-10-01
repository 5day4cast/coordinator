use log::{info, warn};
use prometheus::IntGauge;
use std::time::Duration;
use tokio::{
    sync::{watch, Notify},
    time::{sleep, sleep_until, Instant},
};

/// Whether an LND subscription is connected, shared by its subscriber and the watcher
/// that sweeps the same data. While the subscription is up the watcher only sweeps
/// slowly to reconcile missed events; while it is down the watcher polls at its fallback
/// interval.
pub struct SubscriptionHealth {
    name: &'static str,
    up: watch::Sender<bool>,
    wake: Notify,
    gauge: Option<IntGauge>,
}

impl SubscriptionHealth {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            up: watch::Sender::new(false),
            wake: Notify::new(),
            gauge: None,
        }
    }

    /// Report the state in `gauge` as 1 (up) or 0 (down).
    pub fn with_gauge(mut self, gauge: IntGauge) -> Self {
        gauge.set(i64::from(self.is_up()));
        self.gauge = Some(gauge);
        self
    }

    pub fn is_up(&self) -> bool {
        *self.up.borrow()
    }

    /// Called by the subscriber once its stream is connected.
    pub fn set_up(&self) {
        if let Some(gauge) = &self.gauge {
            gauge.set(1);
        }
        if !self.up.send_replace(true) {
            info!("{} subscription connected", self.name);
        }
    }

    /// Called by the subscriber when its stream ends or fails.
    pub fn set_down(&self) {
        if let Some(gauge) = &self.gauge {
            gauge.set(0);
        }
        if self.up.send_replace(false) {
            warn!(
                "{} subscription dropped; polling at the fallback interval until it reconnects",
                self.name
            );
        }
    }

    /// Ask the watcher to sweep now, e.g. after the subscription reported an event that
    /// the watcher finishes. Requests made during a sweep coalesce into one more sweep.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// How long the watcher sleeps between sweeps.
    pub fn interval(&self, fallback: Duration, subscribed: Duration) -> Duration {
        if self.is_up() {
            subscribed
        } else {
            fallback
        }
    }

    /// Sleep between two sweeps. A subscription that drops during the sleep shortens it
    /// to the fallback interval, and a wake request ends it.
    pub async fn wait(&self, fallback: Duration, subscribed: Duration) {
        let started = Instant::now();
        let mut up = self.up.subscribe();
        let interval = if *up.borrow_and_update() {
            subscribed
        } else {
            fallback
        };
        let dropped = async {
            if interval == fallback || up.wait_for(|up| !up).await.is_err() {
                return std::future::pending().await;
            }
            sleep_until(started + fallback).await
        };
        tokio::select! {
            _ = sleep(interval) => {}
            _ = dropped => {}
            _ = self.wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FALLBACK: Duration = Duration::from_secs(5);
    const SUBSCRIBED: Duration = Duration::from_secs(60);

    fn health() -> SubscriptionHealth {
        SubscriptionHealth::new("Test").with_gauge(IntGauge::new("test_up", "test").unwrap())
    }

    async fn timed_wait(health: &SubscriptionHealth) -> Duration {
        let started = Instant::now();
        health.wait(FALLBACK, SUBSCRIBED).await;
        started.elapsed()
    }

    #[test]
    fn the_interval_follows_the_subscription() {
        let health = health();
        assert_eq!(health.interval(FALLBACK, SUBSCRIBED), FALLBACK);
        health.set_up();
        assert_eq!(health.interval(FALLBACK, SUBSCRIBED), SUBSCRIBED);
        assert_eq!(health.gauge.as_ref().unwrap().get(), 1);
        health.set_down();
        assert_eq!(health.interval(FALLBACK, SUBSCRIBED), FALLBACK);
        assert_eq!(health.gauge.as_ref().unwrap().get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_subscription_switches_to_the_fallback_and_a_reconnect_back() {
        let health = std::sync::Arc::new(health());
        assert_eq!(timed_wait(&health).await, FALLBACK);

        health.set_up();
        assert_eq!(timed_wait(&health).await, SUBSCRIBED);

        // Dropping ten seconds into a slow sleep ends it: the fallback time has passed.
        let dropping = health.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(10)).await;
            dropping.set_down();
        });
        assert_eq!(timed_wait(&health).await, Duration::from_secs(10));
        assert_eq!(timed_wait(&health).await, FALLBACK);
        assert_eq!(timed_wait(&health).await, FALLBACK);

        health.set_up();
        assert_eq!(timed_wait(&health).await, SUBSCRIBED);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wake_request_sweeps_without_waiting() {
        let health = health();
        health.set_up();
        health.wake();
        assert_eq!(timed_wait(&health).await, Duration::ZERO);
        assert_eq!(timed_wait(&health).await, SUBSCRIBED);
    }
}
