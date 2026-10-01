//! One-time use of NIP-98 auth events.
//!
//! The `NostrAuth` extractor accepts an event whose `created_at` is within
//! [`MAX_EVENT_SKEW_SECS`] of now, before or after. An event dated that far
//! ahead is therefore accepted for up to twice that, 120 seconds from its
//! first use. Without this guard the same `Authorization` header could be
//! replayed for that whole time. Each event id is claimed once, after its
//! signature verifies, and is remembered until the extractor would reject it
//! as expired anyway.
//!
//! Production claims are committed to the shared users database before admitting
//! the request. Restarts and blue/green slots share the same uniqueness fence.
//! Database failure rejects authentication; the in-memory constructor is test-only.

use crate::infra::db::DBConnection;
use nostr::EventId;
#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

/// Largest accepted difference between an event's `created_at` and now.
pub const MAX_EVENT_SKEW_SECS: i64 = 60;

/// Default bound on remembered events: 100k ids over a two-minute window is
/// ~830 authenticated requests per second before new requests are refused.
pub const DEFAULT_REPLAY_CAPACITY: usize = 100_000;

pub struct Nip98ReplayGuard {
    database: Option<DBConnection>,
    /// Event id to the last unix second at which it could still be accepted.
    #[cfg(test)]
    seen: Mutex<HashMap<EventId, i64>>,
    capacity: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayRejection {
    /// The event was already used.
    Replayed,
    /// Every remembered event is still live; refuse rather than forget one.
    Full,
    Unavailable,
    Expired,
}

impl Nip98ReplayGuard {
    #[cfg(test)]
    pub fn new(capacity: usize) -> Self {
        Self {
            database: None,
            #[cfg(test)]
            seen: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    pub fn with_database(capacity: usize, database: DBConnection) -> Self {
        Self {
            database: Some(database),
            #[cfg(test)]
            seen: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    pub async fn claim_verified(
        &self,
        id: EventId,
        created_at: i64,
        now: i64,
    ) -> Result<(), ReplayRejection> {
        let Some(database) = &self.database else {
            #[cfg(test)]
            return self.claim(id, created_at, now);
            #[cfg(not(test))]
            return Err(ReplayRejection::Unavailable);
        };
        let capacity = i64::try_from(self.capacity).unwrap_or(i64::MAX);
        let id = id.to_hex();
        database
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                // The first write acquires SQLite's writer lock before counting/admitting.
                sqlx::query("DELETE FROM nip98_consumed_events WHERE expires_at < ?")
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                // Recheck after queueing and acquiring the writer lock. An old proof
                // must not be re-admitted after another request pruned its row.
                let admitted_at = time::OffsetDateTime::now_utc().unix_timestamp();
                if created_at.saturating_add(MAX_EVENT_SKEW_SECS) < admitted_at {
                    return Ok(Err(ReplayRejection::Expired));
                }
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM nip98_consumed_events WHERE event_id = ?)",
                )
                .bind(&id)
                .fetch_one(&mut *tx)
                .await?;
                if exists {
                    return Ok(Err(ReplayRejection::Replayed));
                }
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nip98_consumed_events")
                    .fetch_one(&mut *tx)
                    .await?;
                if count >= capacity {
                    return Ok(Err(ReplayRejection::Full));
                }
                sqlx::query(
                    "INSERT INTO nip98_consumed_events (event_id, expires_at) VALUES (?, ?)",
                )
                .bind(id)
                .bind(created_at.saturating_add(MAX_EVENT_SKEW_SECS))
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(Ok(()))
            })
            .await
            .map_err(|_| ReplayRejection::Unavailable)?
    }

    /// Claim `id` for a single use. Admission also prunes a full guard,
    /// keeping the common path to one hash lookup.
    #[cfg(test)]
    pub fn claim(&self, id: EventId, created_at: i64, now: i64) -> Result<(), ReplayRejection> {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.contains_key(&id) {
            return Err(ReplayRejection::Replayed);
        }
        if seen.len() >= self.capacity {
            seen.retain(|_, last_valid| *last_valid >= now);
        }
        if seen.len() >= self.capacity {
            return Err(ReplayRejection::Full);
        }
        seen.insert(id, created_at + MAX_EVENT_SKEW_SECS);
        Ok(())
    }

    /// Forget expired events during idle periods without waiting for capacity.
    #[cfg(test)]
    pub fn prune(&self, now: i64) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, last_valid| *last_valid >= now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> EventId {
        EventId::from_byte_array([byte; 32])
    }

    #[test]
    fn an_event_is_accepted_once() {
        let guard = Nip98ReplayGuard::new(8);
        assert_eq!(guard.claim(id(1), 1_000, 1_000), Ok(()));
        assert_eq!(
            guard.claim(id(1), 1_000, 1_030),
            Err(ReplayRejection::Replayed)
        );
        assert_eq!(guard.claim(id(2), 1_000, 1_030), Ok(()));
    }

    #[test]
    fn full_guard_forgets_only_expired_events() {
        let guard = Nip98ReplayGuard::new(2);
        guard.claim(id(1), 1_000, 1_000).unwrap();
        guard.claim(id(2), 1_050, 1_050).unwrap();

        // id(1) can be accepted until 1_060, so nothing may be forgotten yet.
        assert_eq!(guard.claim(id(3), 1_055, 1_055), Err(ReplayRejection::Full));

        // After 1_060, id(1) has expired and its slot is reused; id(2) is kept.
        assert_eq!(guard.claim(id(3), 1_061, 1_061), Ok(()));
        assert_eq!(
            guard.claim(id(2), 1_050, 1_061),
            Err(ReplayRejection::Replayed)
        );
    }

    #[test]
    fn maintenance_prunes_idle_entries_without_forgetting_live_events() {
        let guard = Nip98ReplayGuard::new(10);
        guard.claim(id(1), 1_000, 1_000).unwrap();
        guard.claim(id(2), 1_050, 1_050).unwrap();
        guard.prune(1_060);
        assert_eq!(guard.seen.lock().unwrap().len(), 2);
        guard.prune(1_061);
        assert_eq!(guard.seen.lock().unwrap().len(), 1);
        assert_eq!(
            guard.claim(id(2), 1_050, 1_061),
            Err(ReplayRejection::Replayed)
        );
    }
    async fn claim_before_guard_replacement(database: DBConnection, now: i64) {
        let first = Nip98ReplayGuard::with_database(8, database);
        first.claim_verified(id(7), now, now).await.unwrap();
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn database_claims_survive_guard_replacement_and_serialize_writers(
        pool: sqlx::SqlitePool,
    ) {
        let database = || {
            DBConnection::new_with_pools(
                "auth-test".into(),
                ":memory:".into(),
                pool.clone(),
                pool.clone(),
            )
        };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        claim_before_guard_replacement(database(), now).await;
        let second = Nip98ReplayGuard::with_database(8, database());
        let third = Nip98ReplayGuard::with_database(8, database());
        assert_eq!(
            second.claim_verified(id(7), now, now).await,
            Err(ReplayRejection::Replayed)
        );
        let (a, b) = tokio::join!(
            second.claim_verified(id(8), now, now),
            third.claim_verified(id(8), now, now),
        );
        assert!(matches!(
            (a, b),
            (Ok(()), Err(ReplayRejection::Replayed)) | (Err(ReplayRejection::Replayed), Ok(()))
        ));
        // Timestamp admission is repeated under the database lock, not trusted
        // from an extractor that may have been queued before expiry.
        assert_eq!(
            second.claim_verified(id(9), now - 61, now - 61).await,
            Err(ReplayRejection::Expired)
        );
    }
}
