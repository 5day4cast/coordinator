//! Values fetched from a slow service, kept fresh in the background.
//!
//! A reader gets the value it finds at once, even a stale one, and a stale or missing value
//! starts one refresh in the background. Only a reader that finds nothing (or nothing it can
//! use) waits, for no longer than it asks to, and it stops waiting as soon as that refresh ends,
//! whether it succeeded or not. A failed refresh keeps the last good value and is not retried for [`RETRY_AFTER`], so an
//! outage costs the slow service one request per key per interval.

use std::{
    collections::HashMap,
    future::Future,
    hash::Hash,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use log::warn;
use time::OffsetDateTime;
use tokio::{sync::watch, time::Instant};

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

/// What a read found for a key.
#[derive(Debug)]
pub struct Cached<V> {
    /// The latest value, however old; `None` until a first fetch succeeds.
    pub latest: Option<Arc<Fetched<V>>>,
    /// A refresh is running, so a missing or stale value is on its way. A missing value that is
    /// not refreshing means the last attempt failed, and the next waits for [`RETRY_AFTER`].
    pub refreshing: bool,
}

impl<V> Cached<V> {
    pub fn value(&self) -> Option<&V> {
        self.latest.as_deref().map(|fetched| &fetched.value)
    }
}

impl<V> Clone for Cached<V> {
    fn clone(&self) -> Self {
        Self {
            latest: self.latest.clone(),
            refreshing: self.refreshing,
        }
    }
}

struct Slot<V> {
    state: watch::Sender<Cached<V>>,
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
    /// background. With nothing cached yet, this waits up to `wait` for that refresh to end.
    pub async fn get<F, Fut>(
        self: &Arc<Self>,
        key: K,
        ttl: Duration,
        wait: Duration,
        fetch: F,
    ) -> Cached<V>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
    {
        self.get_usable(key, ttl, wait, |_| true, fetch).await
    }

    /// Like [`get`](Self::get), but a cached value that `usable` rejects counts as missing
    /// while a refresh runs: the reader waits up to `wait` for that refresh to end, rather than
    /// being handed the unusable value at once.
    pub async fn get_usable<F, Fut>(
        self: &Arc<Self>,
        key: K,
        ttl: Duration,
        wait: Duration,
        usable: fn(&V) -> bool,
        fetch: F,
    ) -> Cached<V>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
    {
        let mut updates = self.read(&key, ttl, fetch);
        let settled = |state: &Cached<V>| !state.refreshing || state.value().is_some_and(usable);
        let current = updates.borrow_and_update().clone();
        if settled(&current) || wait.is_zero() {
            return current;
        }
        let ended = tokio::time::timeout(wait, updates.wait_for(settled))
            .await
            .ok()
            .and_then(Result::ok)
            .map(|state| state.clone());
        // Otherwise it was still refreshing when the wait ran out.
        ended.unwrap_or_else(|| updates.borrow().clone())
    }

    /// The cached value for `key`, without refreshing it.
    pub fn peek(&self, key: &K) -> Option<Arc<Fetched<V>>> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .and_then(|slot| slot.state.borrow().latest.clone())
    }

    /// Start a refresh of `key` if it is due, without reading it.
    pub fn refresh<F, Fut>(self: &Arc<Self>, key: K, ttl: Duration, fetch: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
    {
        drop(self.read(&key, ttl, fetch));
    }

    fn read<F, Fut>(
        self: &Arc<Self>,
        key: &K,
        ttl: Duration,
        fetch: F,
    ) -> watch::Receiver<Cached<V>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
    {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if slots.len() >= MAX_KEYS && !slots.contains_key(key) {
            let oldest = slots
                .iter()
                .filter(|(_, slot)| !slot.state.borrow().refreshing)
                .min_by_key(|(_, slot)| slot.read_at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                slots.remove(&oldest);
            }
        }
        let slot = slots.entry(key.clone()).or_insert_with(|| Slot {
            state: watch::channel(Cached {
                latest: None,
                refreshing: false,
            })
            .0,
            failed_at: None,
            read_at: Instant::now(),
        });
        slot.read_at = Instant::now();
        let (due, refreshing) = {
            let state = slot.state.borrow();
            let due = state.latest.as_ref().is_none_or(|value| value.age() >= ttl);
            (due, state.refreshing)
        };
        let backing_off = slot
            .failed_at
            .is_some_and(|failed| failed.elapsed() < RETRY_AFTER);
        if due && !refreshing && !backing_off {
            slot.state.send_modify(|state| state.refreshing = true);
            let refresh = Refresh {
                cache: Arc::clone(self),
                key: key.clone(),
                result: None,
            };
            tokio::spawn(async move {
                let result = fetch().await;
                refresh.finish(result);
            });
        }
        slot.state.subscribe()
    }
}

/// One refresh of one key. Dropping it stores the result, so a fetch that panics or is
/// cancelled still ends the refresh, as a failure, instead of leaving it running forever.
struct Refresh<K, V>
where
    K: Eq + Hash + std::fmt::Debug,
{
    cache: Arc<RefreshCache<K, V>>,
    key: K,
    result: Option<anyhow::Result<V>>,
}

impl<K, V> Refresh<K, V>
where
    K: Eq + Hash + std::fmt::Debug,
{
    fn finish(mut self, result: anyhow::Result<V>) {
        self.result = Some(result);
        drop(self);
    }
}

impl<K, V> Drop for Refresh<K, V>
where
    K: Eq + Hash + std::fmt::Debug,
{
    fn drop(&mut self) {
        let result = self.result.take();
        let mut slots = self
            .cache
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(slot) = slots.get_mut(&self.key) else {
            return;
        };
        let key = &self.key;
        match result {
            Some(Ok(value)) => {
                slot.failed_at = None;
                slot.state.send_modify(|state| {
                    state.latest = Some(Arc::new(Fetched::new(value)));
                    state.refreshing = false;
                });
            }
            Some(Err(e)) => {
                slot.failed_at = Some(Instant::now());
                slot.state.send_modify(|state| state.refreshing = false);
                warn!("Refreshing {key:?} failed; keeping the last value: {e:#}");
            }
            None => {
                slot.failed_at = Some(Instant::now());
                slot.state.send_modify(|state| state.refreshing = false);
                warn!("Refreshing {key:?} stopped before it finished; keeping the last value");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Fetch = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<u32>> + Send>>;

    fn counting(
        calls: &Arc<AtomicUsize>,
        delay: Duration,
        value: anyhow::Result<u32>,
    ) -> impl FnOnce() -> Fetch {
        let calls = calls.clone();
        move || {
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                value
            })
        }
    }

    const HOUR: Duration = Duration::from_secs(3600);
    const MS: Duration = Duration::from_millis(1);

    #[tokio::test(start_paused = true)]
    async fn a_first_read_waits_only_as_long_as_asked() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let slow = counting(&calls, 300 * MS, Ok(7));
        let started = Instant::now();
        let first = cache.get(1, HOUR, 20 * MS, slow).await;
        assert!(first.latest.is_none());
        assert!(first.refreshing, "the fetch is still running");
        let waited = started.elapsed();
        assert!(waited >= 20 * MS && waited < 300 * MS, "{waited:?}");

        // The refresh already running is not repeated; this read waits for it.
        let again = counting(&calls, Duration::ZERO, Ok(8));
        let value = cache.get(1, HOUR, Duration::from_secs(2), again).await;
        assert_eq!(value.value(), Some(&7));
        assert!(!value.refreshing);
        let waited = started.elapsed();
        assert!(
            waited >= 300 * MS && waited < Duration::from_secs(2),
            "{waited:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_value_is_served_while_it_refreshes() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let wait = Duration::from_secs(2);
        let first = counting(&calls, Duration::ZERO, Ok(1));
        assert_eq!(
            cache.get(1, Duration::ZERO, wait, first).await.value(),
            Some(&1)
        );
        // Stale at once (ttl zero): the old value comes back and a refresh starts.
        let second = counting(&calls, 50 * MS, Ok(2));
        let stale = cache.get(1, Duration::ZERO, wait, second).await;
        assert_eq!(stale.value(), Some(&1));
        assert!(stale.refreshing);
        tokio::time::sleep(100 * MS).await;
        let fresh = counting(&calls, Duration::ZERO, Ok(3));
        assert_eq!(cache.get(1, HOUR, wait, fresh).await.value(), Some(&2));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_refresh_keeps_the_last_value_and_backs_off() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let wait = Duration::from_secs(2);
        let good = counting(&calls, Duration::ZERO, Ok(1));
        cache.get(1, Duration::ZERO, wait, good).await;
        let failing = counting(&calls, Duration::ZERO, Err(anyhow::anyhow!("down")));
        assert_eq!(
            cache.get(1, Duration::ZERO, wait, failing).await.value(),
            Some(&1)
        );
        tokio::time::sleep(50 * MS).await;
        let skipped = counting(&calls, Duration::ZERO, Ok(9));
        let kept = cache.get(1, Duration::ZERO, wait, skipped).await;
        assert_eq!(kept.value(), Some(&1));
        assert!(!kept.refreshing, "no retry within RETRY_AFTER");
        tokio::time::sleep(RETRY_AFTER).await;
        let retried = counting(&calls, Duration::ZERO, Ok(9));
        assert!(cache.get(1, Duration::ZERO, wait, retried).await.refreshing);
        tokio::time::sleep(MS).await;
        assert_eq!(cache.peek(&1).map(|fetched| fetched.value), Some(9));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// A waiting reader learns at once that the first fetch failed, rather than waiting it out.
    #[tokio::test(start_paused = true)]
    async fn a_failed_first_fetch_ends_the_wait() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let failing = counting(&calls, 10 * MS, Err(anyhow::anyhow!("down")));
        let started = Instant::now();
        let read = cache.get(1, HOUR, Duration::from_secs(5), failing).await;
        assert!(read.latest.is_none());
        assert!(!read.refreshing);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A reader that finds only a value it cannot use waits for the refresh already running,
    /// as if nothing were cached; a usable or fresh value comes back at once.
    #[tokio::test(start_paused = true)]
    async fn an_unusable_value_is_waited_past_while_it_refreshes() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let wait = Duration::from_secs(2);
        let nonzero: fn(&u32) -> bool = |value| *value > 0;
        let unusable = counting(&calls, Duration::ZERO, Ok(0));
        let first = cache
            .get_usable(1, RETRY_AFTER, wait, nonzero, unusable)
            .await;
        assert_eq!(first.value(), Some(&0));
        assert!(!first.refreshing);

        // Not due yet: nothing to wait for, so the unusable value comes back at once.
        let skipped = counting(&calls, Duration::ZERO, Ok(9));
        let started = Instant::now();
        let early = cache
            .get_usable(1, RETRY_AFTER, wait, nonzero, skipped)
            .await;
        assert_eq!(early.value(), Some(&0));
        assert_eq!(started.elapsed(), Duration::ZERO);

        // Due: this read waits for the refresh it started.
        tokio::time::sleep(RETRY_AFTER).await;
        let retried = counting(&calls, 50 * MS, Ok(4));
        let started = Instant::now();
        let read = cache
            .get_usable(1, RETRY_AFTER, wait, nonzero, retried)
            .await;
        assert_eq!(read.value(), Some(&4));
        assert!(!read.refreshing);
        assert!(started.elapsed() >= 50 * MS && started.elapsed() < wait);

        // A usable value is served at once, even while it refreshes.
        tokio::time::sleep(RETRY_AFTER).await;
        let slow = counting(&calls, Duration::from_secs(1), Ok(5));
        let started = Instant::now();
        let stale = cache.get_usable(1, RETRY_AFTER, wait, nonzero, slow).await;
        assert_eq!(stale.value(), Some(&4));
        assert!(stale.refreshing);
        assert_eq!(started.elapsed(), Duration::ZERO);
        // Let the refresh this read started begin.
        tokio::time::sleep(MS).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// A fetch that panics must not leave its key refreshing forever.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_fetch_ends_its_refresh() {
        let cache = Arc::new(RefreshCache::<u8, u32>::new());
        let panicking = || -> Fetch {
            Box::pin(async {
                tokio::time::sleep(10 * MS).await;
                panic!("fetch panicked on purpose")
            })
        };
        let started = Instant::now();
        let read = cache.get(1, HOUR, Duration::from_secs(5), panicking).await;
        assert!(read.latest.is_none());
        assert!(!read.refreshing);
        assert!(started.elapsed() < Duration::from_secs(5));
        // After the back-off, the key refreshes again.
        tokio::time::sleep(RETRY_AFTER).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let good = counting(&calls, Duration::ZERO, Ok(5));
        let read = cache.get(1, HOUR, Duration::from_secs(5), good).await;
        assert_eq!(read.value(), Some(&5));
    }
}
