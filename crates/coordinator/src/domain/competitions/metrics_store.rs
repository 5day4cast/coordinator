//! Aggregate counts for the metrics listener. Each query is a single pass that returns a
//! handful of numbers; the listener caches the results between scrapes.

use sqlx::Row;
use time::OffsetDateTime;

use super::{states::CompetitionStatus, CompetitionStore, TicketStatus};

/// Counts of entries, tickets, payouts and payout jobs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreCounts {
    /// Entries whose ticket has been paid (`EntryStatus::Paid`).
    pub entries_paid: i64,
    /// Entries whose owner completed signing (`EntryStatus::Signed`).
    pub entries_signed: i64,
    /// Tickets by the status `Ticket::get_status` reports.
    pub tickets: Vec<(TicketStatus, i64)>,
    pub payouts_pending: i64,
    pub payouts_succeeded: i64,
    pub payouts_failed: i64,
    /// Automatic payout jobs neither completed nor failed.
    pub payout_jobs_open: i64,
    pub payout_jobs_failed: i64,
    /// Open jobs that have failed at least once and wait for another try.
    pub payout_jobs_retrying: i64,
    /// Age of the oldest open job, 0 when none is open.
    pub oldest_open_payout_job_age_secs: i64,
}

impl CompetitionStore {
    /// Competitions by the state name their runner reports.
    pub async fn competition_state_counts(&self) -> Result<Vec<(&'static str, i64)>, sqlx::Error> {
        let mut counts: Vec<(&'static str, i64)> = Vec::new();
        for competition in self.get_competitions(false).await? {
            let state = CompetitionStatus::from(competition).state_name();
            match counts.iter_mut().find(|(name, _)| *name == state) {
                Some((_, count)) => *count += 1,
                None => counts.push((state, 1)),
            }
        }
        Ok(counts)
    }

    pub async fn store_counts(&self) -> Result<StoreCounts, sqlx::Error> {
        let pool = self.db_connection.read();

        let entries = sqlx::query(
            "SELECT
                COALESCE(SUM(tickets.paid_at IS NOT NULL), 0) AS paid,
                COALESCE(SUM(entries.signed_at IS NOT NULL), 0) AS signed
             FROM entries LEFT JOIN tickets ON entries.ticket_id = tickets.id",
        )
        .fetch_one(pool)
        .await?;

        // Mirrors `Ticket::get_status`: loaded tickets never reach their expiry, so a
        // ticket expires only when its unpaid reservation is older than 10 minutes.
        let ticket_rows = sqlx::query(
            "SELECT
                CASE
                    WHEN entries.id IS NOT NULL THEN 'used'
                    WHEN tickets.settled_at IS NOT NULL THEN 'settled'
                    WHEN tickets.paid_at IS NOT NULL THEN 'paid'
                    WHEN tickets.reserved_at IS NOT NULL
                         AND julianday(tickets.reserved_at) < julianday('now', '-10 minutes')
                        THEN 'expired'
                    WHEN tickets.reserved_at IS NOT NULL THEN 'reserved'
                    ELSE 'created'
                END AS status,
                COUNT(*) AS count
             FROM tickets LEFT JOIN entries ON entries.ticket_id = tickets.id
             GROUP BY status",
        )
        .fetch_all(pool)
        .await?;
        let mut tickets = Vec::with_capacity(ticket_rows.len());
        for row in ticket_rows {
            let status = match row.try_get::<String, _>("status")?.as_str() {
                "used" => TicketStatus::Used,
                "settled" => TicketStatus::Settled,
                "paid" => TicketStatus::Paid,
                "expired" => TicketStatus::Expired,
                "reserved" => TicketStatus::Reserved,
                _ => TicketStatus::Created,
            };
            tickets.push((status, row.try_get("count")?));
        }

        let payouts = sqlx::query(
            "SELECT
                COALESCE(SUM(succeed_at IS NOT NULL), 0) AS succeeded,
                COALESCE(SUM(succeed_at IS NULL AND failed_at IS NOT NULL), 0) AS failed,
                COALESCE(SUM(succeed_at IS NULL AND failed_at IS NULL), 0) AS pending
             FROM payouts",
        )
        .fetch_one(pool)
        .await?;

        let jobs = sqlx::query(
            "SELECT
                COALESCE(SUM(completed_at IS NULL AND failed_at IS NULL), 0) AS open,
                COALESCE(SUM(failed_at IS NOT NULL), 0) AS failed,
                COALESCE(SUM(completed_at IS NULL AND failed_at IS NULL AND attempts > 0), 0)
                    AS retrying,
                MIN(CASE WHEN completed_at IS NULL AND failed_at IS NULL THEN created_at END)
                    AS oldest_open
             FROM payout_jobs",
        )
        .fetch_one(pool)
        .await?;
        let oldest_open: Option<i64> = jobs.try_get("oldest_open")?;
        let now = OffsetDateTime::now_utc().unix_timestamp();

        Ok(StoreCounts {
            entries_paid: entries.try_get("paid")?,
            entries_signed: entries.try_get("signed")?,
            tickets,
            payouts_pending: payouts.try_get("pending")?,
            payouts_succeeded: payouts.try_get("succeeded")?,
            payouts_failed: payouts.try_get("failed")?,
            payout_jobs_open: jobs.try_get("open")?,
            payout_jobs_failed: jobs.try_get("failed")?,
            payout_jobs_retrying: jobs.try_get("retrying")?,
            oldest_open_payout_job_age_secs: oldest_open
                .map(|created| now.saturating_sub(created).max(0))
                .unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::competitions::{Competition, CreateEvent};
    use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
    use uuid::Uuid;

    fn competition() -> Competition {
        let start = OffsetDateTime::now_utc() + time::Duration::days(1);
        Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + time::Duration::days(2),
            start_observation_date: start,
            end_observation_date: start + time::Duration::DAY,
            locations: vec!["KORD".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 5,
            entry_fee: 1_000,
            coordinator_fee_percentage: 10,
            total_competition_pool: 4_500,
            relative_locktime_block_delta: None,
            unlisted: false,
        })
    }

    #[tokio::test]
    async fn counts_follow_the_domain_statuses() {
        let directory = tempfile::tempdir().unwrap();
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let store = CompetitionStore::new(database.clone());

        assert_eq!(store.competition_state_counts().await.unwrap(), vec![]);
        assert_eq!(store.store_counts().await.unwrap(), StoreCounts::default());

        let event = competition();
        let event_id = event.id;
        store
            .add_competition_with_tickets(event, vec![])
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        database
            .execute_write(move |pool| async move {
                let event = event_id.to_string();
                for (ticket, reserved, paid, settled) in [
                    ("t-created", None, None, None),
                    ("t-reserved", Some("-1 minutes"), None, None),
                    ("t-expired", Some("-30 minutes"), None, None),
                    ("t-paid", Some("-2 minutes"), Some("-1 minutes"), None),
                    (
                        "t-used-1",
                        Some("-9 minutes"),
                        Some("-8 minutes"),
                        Some("-7 minutes"),
                    ),
                    ("t-used-2", Some("-9 minutes"), Some("-8 minutes"), None),
                    ("t-used-3", Some("-9 minutes"), Some("-8 minutes"), None),
                ] {
                    sqlx::query(
                        "INSERT INTO tickets (id, event_id, encrypted_preimage, hash,
                             reserved_at, paid_at, settled_at)
                         VALUES (?, ?, 'encrypted', ?,
                             datetime('now', ?), datetime('now', ?), datetime('now', ?))",
                    )
                    .bind(ticket)
                    .bind(&event)
                    .bind(format!("hash-{ticket}"))
                    .bind(reserved)
                    .bind(paid)
                    .bind(settled)
                    .execute(&pool)
                    .await?;
                }
                for (entry, ticket, signed) in [
                    ("e-1", "t-used-1", true),
                    ("e-2", "t-used-2", false),
                    ("e-3", "t-used-3", false),
                ] {
                    sqlx::query(
                        "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey,
                             payout_hash, entry_submission, signed_at)
                         VALUES (?, ?, ?, 'owner', ?, ?, '{}',
                             CASE WHEN ? THEN datetime('now') END)",
                    )
                    .bind(entry)
                    .bind(&event)
                    .bind(ticket)
                    .bind(format!("pubkey-{entry}"))
                    .bind(format!("hash-{entry}"))
                    .bind(signed)
                    .execute(&pool)
                    .await?;
                }
                for (payout, entry, succeeded, failed) in [
                    ("p-failed", "e-1", false, true),
                    ("p-succeeded", "e-1", true, false),
                    ("p-pending", "e-2", false, false),
                ] {
                    sqlx::query(
                        "INSERT INTO payouts (id, entry_id, payout_payment_request,
                             payout_amount_sats, initiated_at, succeed_at, failed_at)
                         VALUES (?, ?, 'lnbc', 1, datetime('now'),
                             CASE WHEN ? THEN datetime('now') END,
                             CASE WHEN ? THEN datetime('now') END)",
                    )
                    .bind(payout)
                    .bind(entry)
                    .bind(succeeded)
                    .bind(failed)
                    .execute(&pool)
                    .await?;
                }
                for (job, entry, age, attempts, completed, failed) in [
                    ("j-done", "e-1", 900, 0, true, false),
                    ("j-gave-up", "e-1", 800, 3, false, true),
                    ("j-open", "e-2", 600, 0, false, false),
                    ("j-retrying", "e-3", 60, 2, false, false),
                ] {
                    sqlx::query(
                        "INSERT INTO payout_jobs (id, entry_id, request_json, created_at,
                             retry_at, attempts, completed_at, failed_at)
                         VALUES (?, ?, '{}', ?, ?, ?,
                             CASE WHEN ? THEN ? END, CASE WHEN ? THEN ? END)",
                    )
                    .bind(job)
                    .bind(entry)
                    .bind(now - age)
                    .bind(now)
                    .bind(attempts)
                    .bind(completed)
                    .bind(now)
                    .bind(failed)
                    .bind(now)
                    .execute(&pool)
                    .await?;
                }
                Ok(())
            })
            .await
            .unwrap();

        // Three entries without a full field: the competition still collects entries.
        assert_eq!(
            store.competition_state_counts().await.unwrap(),
            vec![("collecting_entries", 1)]
        );
        let mut counts = store.store_counts().await.unwrap();
        counts
            .tickets
            .sort_by_key(|(status, _)| format!("{status:?}"));
        let mut expected_tickets = vec![
            (TicketStatus::Created, 1),
            (TicketStatus::Reserved, 1),
            (TicketStatus::Expired, 1),
            (TicketStatus::Paid, 1),
            (TicketStatus::Used, 3),
        ];
        expected_tickets.sort_by_key(|(status, _)| format!("{status:?}"));
        assert!(
            (600..700).contains(&counts.oldest_open_payout_job_age_secs),
            "{counts:?}"
        );
        assert_eq!(
            counts,
            StoreCounts {
                entries_paid: 3,
                entries_signed: 1,
                tickets: expected_tickets,
                payouts_pending: 1,
                payouts_succeeded: 1,
                payouts_failed: 1,
                payout_jobs_open: 2,
                payout_jobs_failed: 1,
                payout_jobs_retrying: 1,
                oldest_open_payout_job_age_secs: counts.oldest_open_payout_job_age_secs,
            }
        );
        database.close().await.unwrap();
    }
}
