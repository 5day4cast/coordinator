//! Values fetched from a slow service, kept fresh in the background.
//!
//! A reader gets the value it finds at once, even a stale one, and a stale or missing value
//! starts one refresh in the background. Only a reader that finds nothing waits, and for no
//! longer than it asks to. A failed refresh keeps the last good value and is not retried for
//! [`RETRY_AFTER`], so an outage costs the slow service one request per key per interval.

use std::{
    collections::HashMap,
    future::Future,
    hash::Hash,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use log::warn;
use time::OffsetDateTime;
use tokio::sync::watch;

/// How long a failed refresh waits before the next attempt.
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Keys kept before unused ones are dropped.
const MAX_KEYS: usize = 512;

/// A value and when it was fetched.
#[derive(Debug)]
pub struct Fetched<V> {
    pub value: V,
    pub fetched_at: OffsetDateTime,
    fetched: Instant,
}

impl<V> Fetched<V> {
    /// A value fetched just now.
    pub fn new(value: V) -> Self {
        Self {
            value,
            fetched_at: OffsetDateTime::now_utc(),
            fetched: Instant::now(),
        }
    }

    pub fn age(&self) -> Duration {
        self.fetched.elapsed()
    }
}

struct Slot<V> {
    latest: watch::Sender<Option<Arc<Fetched<V>>>>,
    refreshing: bool,
    failed_at: Option<Instant>,
    read_at: Instant,
}

pub struct RefreshCache<K, V> {
    slots: Mutex<HashMap<K, Slot<V>>>,
}

impl<K, V> Default for RefreshCache<K, V> {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V> RefreshCache<K, V>
where
    K: Eq + Hash + Clone + std::fmt::Debug + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// The value for `key`. Older than `ttl`, or missing, it is refreshed with `fetch` in the
    /// background. With nothing cached yet, this waits up to `wait` for that refresh.
    pub async fn get<F, Fut>(
        self: &Arc<Self>,
        key: K,
        ttl: Duration,
        wait: Duration,
        fetch: F,
    ) -> Option<Arc<Fetched<V>>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
    {
        let (current, mut updates) = self.read(&key, ttl, fetch);
        if current.is_some() || wait.is_zero() {
            return current;
        }
        let refreshed = match tokio::time::timeout(wait, updates.wait_for(Option::is_some)).await {
            Ok(Ok(value)) => value.clone(),
            _ => None,
        };
        refreshed
    }

    /// The cached value for `key`, without refreshing it.
    pub fn peek(&self, key: &K) -> Option<Arc<Fetched<V>>> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .and_then(|slot| slot.latest.borrow().clone())
    }

    /// Start a refresh of `key` if it is due, without reading it.
    pub fn refresh<F, Fut>(self: &Arc<Self>, key: K, ttl: Duration, fetch: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
    {
        drop(self.read(&key, ttl, fetch));
    }

    #[allow(clippy::type_complexity)]
    fn read<F, Fut>(
        self: &Arc<Self>,
        key: &K,
        ttl: Duration,
        fetch: F,
    ) -> (
        Option<Arc<Fetched<V>>>,
        watch::Receiver<Option<Arc<Fetched<V>>>>,
    )
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
    {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if slots.len() >= MAX_KEYS && !slots.contains_key(key) {
            let oldest = slots
                .iter()
                .filter(|(_, slot)| !slot.refreshing)
                .min_by_key(|(_, slot)| slot.read_at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                slots.remove(&oldest);
            }
        }
        let slot = slots.entry(key.clone()).or_insert_with(|| Slot {
            latest: watch::channel(None).0,
            refreshing: false,
            failed_at: None,
            read_at: Instant::now(),
        });
        slot.read_at = Instant::now();
        let current = slot.latest.borrow().clone();
        let due = current.as_ref().is_none_or(|value| value.age() >= ttl);
        let backing_off = slot
            .failed_at
            .is_some_and(|failed| failed.elapsed() < RETRY_AFTER);
        if due && !slot.refreshing && !backing_off {
            slot.refreshing = true;
            let cache = Arc::clone(self);
            let key = key.clone();
            tokio::spawn(async move {
                let result = fetch().await;
                let mut slots = cache.slots.lock().unwrap_or_else(PoisonError::into_inner);
                let Some(slot) = slots.get_mut(&key) else {
                    return;
                };
                slot.refreshing = false;
                match result {
                    Ok(value) => {
                        slot.failed_at = None;
                        slot.latest
                            .send_replace(Some(Arc::new(Fetched::new(value))));
                    }
                    Err(e) => {
                        slot.failed_at = Some(Instant::now());
                        warn!("Refreshing {key:?} failed; keeping the last value: {e:#}");
                    }
                }
            });
        }
        (current, slot.latest.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting(
        calls: &Arc<AtomicUsize>,
        delay: Duration,
        value: anyhow::Result<u32>,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<u32>> + Send>> {
        let calls = calls.clone();
        move || {
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                value
            })
        }
    }

    #[tokio::test]
    async fn a_first_read_waits_only_as_long_as_asked() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let hour = Duration::from_secs(3600);
        let slow = counting(&calls, Duration::from_millis(300), Ok(7));
        let started = Instant::now();
        assert!(cache
            .get(1, hour, Duration::from_millis(20), slow)
            .await
            .is_none());
        assert!(started.elapsed() < Duration::from_millis(200));

        // The refresh already running is not repeated; this read waits for it.
        let again = counting(&calls, Duration::ZERO, Ok(8));
        let value = cache.get(1, hour, Duration::from_secs(2), again).await;
        assert_eq!(value.map(|fetched| fetched.value), Some(7));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stale_value_is_served_while_it_refreshes() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let wait = Duration::from_secs(2);
        let first = counting(&calls, Duration::ZERO, Ok(1));
        assert_eq!(
            cache
                .get(1, Duration::ZERO, wait, first)
                .await
                .unwrap()
                .value,
            1
        );
        // Stale at once (ttl zero): the old value comes back and a refresh starts.
        let second = counting(&calls, Duration::from_millis(50), Ok(2));
        assert_eq!(
            cache
                .get(1, Duration::ZERO, wait, second)
                .await
                .unwrap()
                .value,
            1
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let fresh = counting(&calls, Duration::ZERO, Ok(3));
        assert_eq!(
            cache
                .get(1, Duration::from_secs(3600), wait, fresh)
                .await
                .unwrap()
                .value,
            2
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_last_value_and_backs_off() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let wait = Duration::from_secs(2);
        let good = counting(&calls, Duration::ZERO, Ok(1));
        cache.get(1, Duration::ZERO, wait, good).await.unwrap();
        let failing = counting(&calls, Duration::ZERO, Err(anyhow::anyhow!("down")));
        assert_eq!(
            cache
                .get(1, Duration::ZERO, wait, failing)
                .await
                .unwrap()
                .value,
            1
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let skipped = counting(&calls, Duration::ZERO, Ok(9));
        assert_eq!(
            cache
                .get(1, Duration::ZERO, wait, skipped)
                .await
                .unwrap()
                .value,
            1
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "no retry within RETRY_AFTER"
        );
    }
}
