//! One durable GetVtxos budget shared by swapd, Coordinator, and Synth.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ark_grpc::{VtxoBudgetError, VtxoRequestBudget, VtxoRequestPriority};

use crate::store::Store;

// About 7.5 a minute across swapd, Coordinator and Synth: enough for a 20-seat competition to
// fill, still far below the polling that drew Arkade's complaint (about 60 a minute from one
// process). Ten a window refused entry payments until 20-seat competitions went unfilled.
pub const REQUESTS: i64 = 30;
// Background reads stop when ten slots are used; twenty remain reserved for payments.
pub const BACKGROUND_REQUESTS: i64 = 10;
pub const WINDOW_MS: i64 = 240_000;

pub fn deferred(error: &anyhow::Error) -> Option<&VtxoBudgetError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<VtxoBudgetError>())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock must be after the Unix epoch")
        .as_millis()
        .try_into()
        .expect("Unix milliseconds fit i64")
}

impl VtxoRequestBudget for Store {
    fn reserve(
        &self,
        priority: VtxoRequestPriority,
    ) -> Pin<Box<dyn Future<Output = Result<(), VtxoBudgetError>> + Send + '_>> {
        Box::pin(async move {
            match self.reserve_indexer_request(now_ms(), priority).await {
                Ok(None) => Ok(()),
                Ok(Some(retry_after)) => Err(VtxoBudgetError {
                    retry_after,
                    message: "shared GetVtxos budget exhausted or capacity reserved for payments"
                        .into(),
                }),
                Err(error) => Err(VtxoBudgetError {
                    retry_after: Duration::from_secs(60),
                    message: format!("cannot reserve the shared GetVtxos budget: {error}"),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preimages::PreimageKey;

    fn key() -> PreimageKey {
        PreimageKey::from_wallet_secret(&[1; 32])
    }

    #[tokio::test]
    async fn background_bursts_preserve_payment_capacity_even_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reserved.sqlite");
        let now = 1_800_000_000_000;
        let store = Store::open(&path, key()).await.unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let store = store.clone();
            tasks.spawn(async move {
                store
                    .reserve_indexer_request(now, VtxoRequestPriority::Background)
                    .await
                    .unwrap()
                    .is_none()
            });
        }
        let mut background = 0;
        while let Some(result) = tasks.join_next().await {
            background += i64::from(result.unwrap());
        }
        assert_eq!(background, BACKGROUND_REQUESTS);
        drop(store);
        let mut stores = Vec::new();
        for _ in 0..3 {
            stores.push(Store::open(&path, key()).await.unwrap());
        }
        for index in 0..100 {
            let store = stores[index % stores.len()].clone();
            tasks.spawn(async move {
                store
                    .reserve_indexer_request(now, VtxoRequestPriority::Payment)
                    .await
                    .unwrap()
                    .is_none()
            });
        }
        let mut payments = 0;
        while let Some(result) = tasks.join_next().await {
            payments += i64::from(result.unwrap());
        }
        assert_eq!(payments, REQUESTS - BACKGROUND_REQUESTS);
        for priority in [
            VtxoRequestPriority::Background,
            VtxoRequestPriority::Payment,
        ] {
            assert!(stores[0]
                .reserve_indexer_request(now, priority)
                .await
                .unwrap()
                .is_some());
        }
    }

    #[tokio::test]
    async fn a_hundred_callers_across_independent_pools_share_the_budget_and_restart_keeps_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("budget.sqlite");
        let mut stores = Vec::new();
        for _ in 0..3 {
            stores.push(Store::open(&path, key()).await.unwrap());
        }
        let now = 1_800_000_000_000;
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..100 {
            let store = stores[index % stores.len()].clone();
            tasks.spawn(async move {
                store
                    .reserve_indexer_request(now, VtxoRequestPriority::Payment)
                    .await
                    .unwrap()
            });
        }
        let mut granted = 0;
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                None => granted += 1,
                Some(wait) => assert_eq!(wait, Duration::from_millis(WINDOW_MS as u64)),
            }
        }
        assert_eq!(granted, REQUESTS);
        drop(stores);
        let reopened = Store::open(&path, key()).await.unwrap();
        for tick in [0, 1_000, 60_000, 120_000, 239_999] {
            assert!(reopened
                .reserve_indexer_request(now + tick, VtxoRequestPriority::Payment)
                .await
                .unwrap()
                .is_some());
        }
        // A backward clock movement must not refill the budget.
        assert!(reopened
            .reserve_indexer_request(now - 60_000, VtxoRequestPriority::Payment)
            .await
            .unwrap()
            .is_some());
        assert!(reopened
            .reserve_indexer_request(now + WINDOW_MS, VtxoRequestPriority::Payment)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn the_window_rolls_instead_of_resetting_at_wall_clock_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("budget.sqlite"), key())
            .await
            .unwrap();
        let now = 1_800_000_000_000;
        for index in 0..REQUESTS {
            assert!(store
                .reserve_indexer_request(now + index * 1_000, VtxoRequestPriority::Payment)
                .await
                .unwrap()
                .is_none());
        }
        assert!(store
            .reserve_indexer_request(now + WINDOW_MS - 1, VtxoRequestPriority::Payment)
            .await
            .unwrap()
            .is_some());
        // Only the oldest request expires: a new caller can consume its slot,
        // but the other nine reservations still count for their full lifetime.
        assert!(store
            .reserve_indexer_request(now + WINDOW_MS, VtxoRequestPriority::Payment)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .reserve_indexer_request(now + WINDOW_MS, VtxoRequestPriority::Payment)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .reserve_indexer_request(now + WINDOW_MS + 999, VtxoRequestPriority::Payment)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .reserve_indexer_request(now + WINDOW_MS + 1_000, VtxoRequestPriority::Payment)
            .await
            .unwrap()
            .is_none());
    }
}
