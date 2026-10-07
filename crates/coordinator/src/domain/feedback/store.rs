use std::{fmt, str::FromStr};

use sqlx::{sqlite::SqliteRow, FromRow, Row};
use time::{format_description::BorrowedFormatItem, macros::format_description, OffsetDateTime};
use uuid::Uuid;

use crate::{domain::Error, infra::db::DBConnection};

/// How times are written: UTC, one fixed width, so they sort as text and use the index.
const STORED_TIME: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z");

fn stored_time(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(STORED_TIME)
        .expect("a UTC time formats")
}

fn read_time(text: &str) -> Result<OffsetDateTime, time::error::Parse> {
    time::PrimitiveDateTime::parse(text, STORED_TIME).map(time::PrimitiveDateTime::assume_utc)
}

/// Where the operator is with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FeedbackStatus {
    New,
    Seen,
    Done,
}

impl FeedbackStatus {
    pub const ALL: [FeedbackStatus; 3] = [Self::New, Self::Seen, Self::Done];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Seen => "seen",
            Self::Done => "done",
        }
    }
}

impl fmt::Display for FeedbackStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FeedbackStatus {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "new" => Ok(Self::New),
            "seen" => Ok(Self::Seen),
            "done" => Ok(Self::Done),
            other => Err(Error::BadRequest(format!(
                "unknown feedback status {other:?}"
            ))),
        }
    }
}

/// A message as the form handler stores it; every text is cleaned and capped already.
#[derive(Clone, Debug)]
pub struct NewFeedback {
    pub message: String,
    pub contact: Option<String>,
    pub page: Option<String>,
    pub rid: Option<String>,
    pub sid: Option<String>,
    pub pubkey: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FeedbackRow {
    pub id: Uuid,
    pub created_at: OffsetDateTime,
    pub message: String,
    pub contact: Option<String>,
    pub page: Option<String>,
    pub rid: Option<String>,
    pub sid: Option<String>,
    pub pubkey: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub status: FeedbackStatus,
    pub operator_note: Option<String>,
    pub notified_at: Option<OffsetDateTime>,
}

fn decode_error(
    column: &str,
    error: impl std::error::Error + Send + Sync + 'static,
) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.to_string(),
        source: Box::new(error),
    }
}

impl FromRow<'_, SqliteRow> for FeedbackRow {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let id: String = row.try_get("id")?;
        let created_at: String = row.try_get("created_at")?;
        let status: String = row.try_get("status")?;
        let notified_at: Option<String> = row.try_get("notified_at")?;
        Ok(Self {
            id: Uuid::parse_str(&id).map_err(|e| decode_error("id", e))?,
            created_at: read_time(&created_at).map_err(|e| decode_error("created_at", e))?,
            message: row.try_get("message")?,
            contact: row.try_get("contact")?,
            page: row.try_get("page")?,
            rid: row.try_get("rid")?,
            sid: row.try_get("sid")?,
            pubkey: row.try_get("pubkey")?,
            ip: row.try_get("ip")?,
            user_agent: row.try_get("user_agent")?,
            status: status
                .parse()
                .map_err(|e: Error| decode_error("status", std::io::Error::other(e.to_string())))?,
            operator_note: row.try_get("operator_note")?,
            notified_at: notified_at
                .as_deref()
                .map(read_time)
                .transpose()
                .map_err(|e| decode_error("notified_at", e))?,
        })
    }
}

const COLUMNS: &str = "id, created_at, message, contact, page, rid, sid, pubkey, ip, user_agent, \
                       status, operator_note, notified_at";

/// Feedback messages, in the users database.
#[derive(Clone, Debug)]
pub struct FeedbackStore {
    db: DBConnection,
}

impl FeedbackStore {
    pub fn new(db: DBConnection) -> Self {
        Self { db }
    }

    /// Store a message received at `at`; answers its id.
    pub async fn insert(&self, feedback: NewFeedback, at: OffsetDateTime) -> Result<Uuid, Error> {
        let id = Uuid::now_v7();
        let created_at = stored_time(at);
        self.db
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO feedback (id, created_at, message, contact, page, rid, sid, \
                     pubkey, ip, user_agent, status) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'new')",
                )
                .bind(id.to_string())
                .bind(created_at)
                .bind(feedback.message)
                .bind(feedback.contact)
                .bind(feedback.page)
                .bind(feedback.rid)
                .bind(feedback.sid)
                .bind(feedback.pubkey)
                .bind(feedback.ip)
                .bind(feedback.user_agent)
                .execute(&pool)
                .await
            })
            .await?;
        Ok(id)
    }

    /// Whether the same message was stored at or after `since`.
    pub async fn is_duplicate(&self, message: &str, since: OffsetDateTime) -> Result<bool, Error> {
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM feedback WHERE created_at >= ? AND message = ?)",
        )
        .bind(stored_time(since))
        .bind(message)
        .fetch_one(self.db.read())
        .await?;
        Ok(found)
    }

    /// The newest `limit` messages, of one status or all.
    pub async fn list(
        &self,
        status: Option<FeedbackStatus>,
        limit: i64,
    ) -> Result<Vec<FeedbackRow>, Error> {
        let rows = sqlx::query_as::<_, FeedbackRow>(&format!(
            "SELECT {COLUMNS} FROM feedback WHERE ?1 IS NULL OR status = ?1 \
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        ))
        .bind(status.map(FeedbackStatus::as_str))
        .bind(limit)
        .fetch_all(self.db.read())
        .await?;
        Ok(rows)
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<FeedbackRow>, Error> {
        let row = sqlx::query_as::<_, FeedbackRow>(&format!(
            "SELECT {COLUMNS} FROM feedback WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(self.db.read())
        .await?;
        Ok(row)
    }

    /// Messages the operator has not opened yet.
    pub async fn count_new(&self) -> Result<i64, Error> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE status = 'new'")
            .fetch_one(self.db.read())
            .await?;
        Ok(count)
    }

    /// Set a message's status, and its note when `note` is given (an empty note clears it).
    /// Answers whether the message exists.
    pub async fn update(
        &self,
        id: Uuid,
        status: FeedbackStatus,
        note: Option<String>,
    ) -> Result<bool, Error> {
        let result = self
            .db
            .execute_write(move |pool| async move {
                match note {
                    Some(note) => {
                        sqlx::query(
                            "UPDATE feedback SET status = ?, operator_note = NULLIF(?, '') \
                             WHERE id = ?",
                        )
                        .bind(status.as_str())
                        .bind(note)
                        .bind(id.to_string())
                        .execute(&pool)
                        .await
                    }
                    None => {
                        sqlx::query("UPDATE feedback SET status = ? WHERE id = ?")
                            .bind(status.as_str())
                            .bind(id.to_string())
                            .execute(&pool)
                            .await
                    }
                }
            })
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Mark a new message seen, as opening it does.
    pub async fn mark_seen(&self, id: Uuid) -> Result<(), Error> {
        self.db
            .execute_write(move |pool| async move {
                sqlx::query("UPDATE feedback SET status = 'seen' WHERE id = ? AND status = 'new'")
                    .bind(id.to_string())
                    .execute(&pool)
                    .await
            })
            .await?;
        Ok(())
    }

    /// Messages received at or after `since` whose alert has not gone out, oldest first.
    pub async fn unnotified(
        &self,
        since: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<FeedbackRow>, Error> {
        let rows = sqlx::query_as::<_, FeedbackRow>(&format!(
            "SELECT {COLUMNS} FROM feedback WHERE notified_at IS NULL AND created_at >= ? \
             ORDER BY created_at ASC, id ASC LIMIT ?"
        ))
        .bind(stored_time(since))
        .bind(limit)
        .fetch_all(self.db.read())
        .await?;
        Ok(rows)
    }

    /// Note that the alert for `ids` went out at `at`.
    pub async fn mark_notified(&self, ids: Vec<Uuid>, at: OffsetDateTime) -> Result<(), Error> {
        if ids.is_empty() {
            return Ok(());
        }
        let at = stored_time(at);
        self.db
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                for id in ids {
                    sqlx::query("UPDATE feedback SET notified_at = ? WHERE id = ?")
                        .bind(&at)
                        .bind(id.to_string())
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use time::macros::datetime;

    use super::*;

    fn store(pool: SqlitePool) -> FeedbackStore {
        FeedbackStore::new(DBConnection::new_with_pools(
            "test".to_string(),
            ":memory:".to_string(),
            pool.clone(),
            pool,
        ))
    }

    fn message(text: &str) -> NewFeedback {
        NewFeedback {
            message: text.into(),
            contact: Some("npub1example".into()),
            page: Some("/entries".into()),
            rid: Some("0199c1a2-0000-7000-8000-000000000001".into()),
            sid: Some("AbCdEfGhIjKlMnOpQrStUv".into()),
            pubkey: None,
            ip: Some("203.0.113.9".into()),
            user_agent: Some("Mozilla/5.0".into()),
        }
    }

    #[test]
    fn stored_times_have_one_width_and_read_back() {
        let at = datetime!(2026-10-07 18:49:22.5 UTC);
        assert_eq!(stored_time(at), "2026-10-07T18:49:22.500000Z");
        assert_eq!(read_time(&stored_time(at)).unwrap(), at);
        let whole = datetime!(2026-10-07 18:49:22 UTC);
        assert!(stored_time(whole) < stored_time(at));
        assert_eq!(
            stored_time(datetime!(2026-10-07 20:49:22 +2)),
            "2026-10-07T18:49:22.000000Z"
        );
    }

    #[test]
    fn statuses_read_back_and_unknown_ones_are_refused() {
        for status in FeedbackStatus::ALL {
            assert_eq!(status.as_str().parse::<FeedbackStatus>().unwrap(), status);
        }
        assert!("archived".parse::<FeedbackStatus>().is_err());
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn messages_are_stored_listed_and_updated(pool: SqlitePool) {
        let store = store(pool);
        let first_at = datetime!(2026-10-07 10:00 UTC);
        let first = store.insert(message("first"), first_at).await.unwrap();
        let second = store
            .insert(message("second"), first_at + time::Duration::minutes(5))
            .await
            .unwrap();

        let all = store.list(None, 50).await.unwrap();
        assert_eq!(
            all.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![second, first]
        );
        assert_eq!(all[1].message, "first");
        assert_eq!(all[1].created_at, first_at);
        assert_eq!(all[1].status, FeedbackStatus::New);
        assert_eq!(store.count_new().await.unwrap(), 2);

        store.mark_seen(first).await.unwrap();
        assert_eq!(store.count_new().await.unwrap(), 1);
        assert!(store
            .update(second, FeedbackStatus::Done, Some("replied".into()))
            .await
            .unwrap());
        let done = store.get(second).await.unwrap().unwrap();
        assert_eq!(done.status, FeedbackStatus::Done);
        assert_eq!(done.operator_note.as_deref(), Some("replied"));
        // Opening a message that is done leaves it done.
        store.mark_seen(second).await.unwrap();
        assert_eq!(
            store.get(second).await.unwrap().unwrap().status,
            FeedbackStatus::Done
        );
        assert!(store
            .update(second, FeedbackStatus::Done, Some(String::new()))
            .await
            .unwrap());
        assert_eq!(
            store.get(second).await.unwrap().unwrap().operator_note,
            None
        );

        let seen = store.list(Some(FeedbackStatus::Seen), 50).await.unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].id, first);
        assert!(!store
            .update(Uuid::now_v7(), FeedbackStatus::Seen, None)
            .await
            .unwrap());
        assert!(store.get(Uuid::now_v7()).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn a_repeat_counts_only_within_its_window(pool: SqlitePool) {
        let store = store(pool);
        let at = datetime!(2026-10-07 10:00 UTC);
        store.insert(message("same words"), at).await.unwrap();
        assert!(store
            .is_duplicate("same words", at - time::Duration::hours(24))
            .await
            .unwrap());
        assert!(!store
            .is_duplicate("same words", at + time::Duration::seconds(1))
            .await
            .unwrap());
        assert!(!store
            .is_duplicate("other words", at - time::Duration::hours(24))
            .await
            .unwrap());
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn alerts_cover_recent_unnotified_messages_once(pool: SqlitePool) {
        let store = store(pool);
        let now = datetime!(2026-10-07 10:00 UTC);
        let stale = store
            .insert(message("old"), now - time::Duration::hours(25))
            .await
            .unwrap();
        let fresh = store.insert(message("new"), now).await.unwrap();
        let pending = store
            .unnotified(now - time::Duration::hours(24), 50)
            .await
            .unwrap();
        assert_eq!(
            pending.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![fresh]
        );
        store.mark_notified(vec![fresh], now).await.unwrap();
        assert!(store
            .unnotified(now - time::Duration::hours(24), 50)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store.get(fresh).await.unwrap().unwrap().notified_at,
            Some(now)
        );
        assert_eq!(store.get(stale).await.unwrap().unwrap().notified_at, None);
    }
}
