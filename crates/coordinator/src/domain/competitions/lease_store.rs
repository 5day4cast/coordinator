//! Leases on work that only one coordinator process may do at a time.
//!
//! During a blue/green deploy two coordinators share one database. Each competition, and each
//! singleton worker, is driven only by the process holding its lease. A lease expires unless its
//! holder renews it, so a crashed process's work moves to the other one. Taking a lease from
//! another holder raises its token, and state writes are fenced on the token, so a process that
//! lost a lease while working cannot overwrite the new holder's state.

use dashmap::DashMap;
use log::warn;
use sqlx::Row;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

use super::CompetitionStore;
use crate::infra::db::DatabaseWriteError;

/// A held lease. `token` fences writes made under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub resource: String,
    pub holder: String,
    pub token: i64,
}

impl Lease {
    /// The lease on driving one competition.
    pub fn competition_resource(competition_id: uuid::Uuid) -> String {
        format!("competition:{competition_id}")
    }
}

fn unix_now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Leases count UNIX milliseconds, so a short lease is not cut short by rounding.
fn unix_now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn expiry(ttl: Duration) -> i64 {
    unix_now_ms().saturating_add(ttl.as_millis() as i64)
}

impl CompetitionStore {
    /// Take `resource` for `holder` until `ttl` from now, if nobody else holds it.
    ///
    /// The holder's own lease is extended with the same token. An expired or released lease
    /// passes to `holder` with a higher token.
    pub async fn acquire_lease(
        &self,
        resource: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<Option<Lease>, DatabaseWriteError> {
        let (resource, holder) = (resource.to_owned(), holder.to_owned());
        let (expires_at, now) = (expiry(ttl), unix_now_ms());
        self.db_connection
            .execute_write(move |pool| async move {
                let token: Option<i64> = sqlx::query_scalar(
                    "INSERT INTO leases (resource, holder, token, expires_at) VALUES (?1, ?2, 1, ?3)
                     ON CONFLICT (resource) DO UPDATE SET
                         token = CASE WHEN leases.holder = excluded.holder
                             THEN leases.token ELSE leases.token + 1 END,
                         holder = excluded.holder,
                         expires_at = excluded.expires_at
                     WHERE leases.holder = excluded.holder OR leases.expires_at <= ?4
                     RETURNING token",
                )
                .bind(&resource)
                .bind(&holder)
                .bind(expires_at)
                .bind(now)
                .fetch_optional(&pool)
                .await?;
                Ok(token.map(|token| Lease {
                    resource,
                    holder,
                    token,
                }))
            })
            .await
    }

    /// Extend a held lease. False if another holder has taken it.
    pub async fn renew_lease(
        &self,
        lease: &Lease,
        ttl: Duration,
    ) -> Result<bool, DatabaseWriteError> {
        let lease = lease.clone();
        let expires_at = expiry(ttl);
        self.db_connection
            .execute_write(move |pool| async move {
                let renewed = sqlx::query(
                    "UPDATE leases SET expires_at = ?
                     WHERE resource = ? AND holder = ? AND token = ?",
                )
                .bind(expires_at)
                .bind(&lease.resource)
                .bind(&lease.holder)
                .bind(lease.token)
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(renewed == 1)
            })
            .await
    }

    /// Give up a held lease, so another process can take it at once.
    pub async fn release_lease(&self, lease: &Lease) -> Result<(), DatabaseWriteError> {
        let lease = lease.clone();
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE leases SET expires_at = 0
                     WHERE resource = ? AND holder = ? AND token = ?",
                )
                .bind(&lease.resource)
                .bind(&lease.holder)
                .bind(lease.token)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    /// Who holds `resource` now, if anyone.
    pub async fn lease_holder(&self, resource: &str) -> Result<Option<Lease>, sqlx::Error> {
        let row =
            sqlx::query("SELECT holder, token FROM leases WHERE resource = ? AND expires_at > ?")
                .bind(resource)
                .bind(unix_now_ms())
                .fetch_optional(self.db_connection.read())
                .await?;
        row.map(|row| {
            Ok(Lease {
                resource: resource.to_owned(),
                holder: row.try_get("holder")?,
                token: row.try_get("token")?,
            })
        })
        .transpose()
    }
}

/// Another coordinator took the lease while this one worked.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("another coordinator took the lease")]
pub struct LeaseLost;

/// Drive `work` to completion, renewing `lease` every third of `ttl` meanwhile.
///
/// The work always completes, since it may have side effects in flight. If the lease was lost
/// meanwhile, the result is `LeaseLost`, and fenced writes made under it will have failed.
pub async fn while_leased<F: Future>(
    store: &CompetitionStore,
    lease: &Lease,
    ttl: Duration,
    work: F,
) -> Result<F::Output, LeaseLost> {
    tokio::pin!(work);
    let every = (ttl / 3).max(Duration::from_millis(100));
    let mut renew = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut lost = false;
    loop {
        tokio::select! {
            output = &mut work => return if lost { Err(LeaseLost) } else { Ok(output) },
            _ = renew.tick(), if !lost => match store.renew_lease(lease, ttl).await {
                Ok(true) => {}
                Ok(false) => lost = true,
                // A fenced write decides if the database stays unreachable.
                Err(e) => warn!("Cannot renew lease {}: {e}", lease.resource),
            },
        }
    }
}

/// Leases on background workers that must run in one coordinator process at a time, such as
/// the ones that pay winners. Each keeps its lease between ticks, so work stays in one process
/// until it stops.
pub struct WorkerLeases {
    store: Arc<CompetitionStore>,
    holder: String,
    ttl: Duration,
    held: DashMap<String, Lease>,
}

impl WorkerLeases {
    pub fn new(store: Arc<CompetitionStore>, holder: String, ttl: Duration) -> Self {
        Self {
            store,
            holder,
            ttl,
            held: DashMap::new(),
        }
    }

    /// Run one tick of `worker`, if this process holds or can take its lease.
    /// `None` if another coordinator runs it.
    pub async fn tick<F: Future>(&self, worker: &str, work: F) -> Option<F::Output> {
        let resource = format!("worker:{worker}");
        let lease = match self
            .store
            .acquire_lease(&resource, &self.holder, self.ttl)
            .await
        {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                self.held.remove(&resource);
                return None;
            }
            Err(e) => {
                warn!("Cannot take the {worker} lease: {e}");
                return None;
            }
        };
        self.held.insert(resource.clone(), lease.clone());
        match while_leased(&self.store, &lease, self.ttl, work).await {
            Ok(output) => Some(output),
            Err(LeaseLost) => {
                warn!("Another coordinator took over the {worker}");
                self.held.remove(&resource);
                None
            }
        }
    }

    /// Hand `worker` over at once, when it stops.
    pub async fn release(&self, worker: &str) {
        if let Some((_, lease)) = self.held.remove(&format!("worker:{worker}")) {
            if let Err(e) = self.store.release_lease(&lease).await {
                warn!("Cannot release lease {}: {e}", lease.resource);
            }
        }
    }

    /// Hand every held worker over at once, on shutdown.
    pub async fn release_all(&self) {
        let held: Vec<Lease> = self
            .held
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        for lease in held {
            if let Err(e) = self.store.release_lease(&lease).await {
                warn!("Cannot release lease {}: {e}", lease.resource);
            }
        }
        self.held.clear();
    }
}

impl CompetitionStore {
    /// Record a wake, so another coordinator driving the competition sees it.
    pub async fn record_wake(
        &self,
        competition_id: uuid::Uuid,
        origin: &str,
    ) -> Result<(), DatabaseWriteError> {
        let origin = origin.to_owned();
        let now = unix_now();
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO competition_wakes (competition_id, origin, woken_at) VALUES (?, ?, ?)",
                )
                .bind(competition_id.to_string())
                .bind(&origin)
                .bind(now)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    /// Wakes other coordinators recorded after `after`, and the last sequence number seen.
    pub async fn wakes_since(
        &self,
        after: i64,
        origin: &str,
    ) -> Result<(Vec<uuid::Uuid>, i64), sqlx::Error> {
        let rows = sqlx::query(
            "SELECT seq, competition_id, origin FROM competition_wakes WHERE seq > ? ORDER BY seq",
        )
        .bind(after)
        .fetch_all(self.db_connection.read())
        .await?;
        let mut last = after;
        let mut woken = Vec::new();
        for row in rows {
            last = last.max(row.try_get::<i64, _>("seq")?);
            if row.try_get::<String, _>("origin")? == origin {
                continue;
            }
            let id: String = row.try_get("competition_id")?;
            woken.push(uuid::Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e)))?);
        }
        Ok((woken, last))
    }

    /// The latest wake's sequence number, where a starting coordinator begins polling.
    pub async fn last_wake(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM competition_wakes")
            .fetch_one(self.db_connection.read())
            .await
    }

    /// Forget wakes older than `age`; every coordinator has polled them by then.
    pub async fn prune_wakes(&self, age: Duration) -> Result<(), DatabaseWriteError> {
        let before = unix_now().saturating_sub(age.as_secs() as i64);
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query("DELETE FROM competition_wakes WHERE woken_at < ?")
                    .bind(before)
                    .execute(&pool)
                    .await?;
                Ok(())
            })
            .await
    }
}
