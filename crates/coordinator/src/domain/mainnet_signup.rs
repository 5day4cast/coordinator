//! Email addresses submitted for the mainnet launch announcement.
use crate::{
    domain::{
        feedback::{FeedbackLimits, FEEDBACK_POW_BITS},
        Error, SignupPow,
    },
    infra::db::DBConnection,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub const MAX_EMAIL_BYTES: usize = 254;

pub struct MainnetSignup {
    pub enabled: bool,
    pub store: MainnetSignupStore,
    pub pow: SignupPow,
    pub limits: FeedbackLimits,
}

impl MainnetSignup {
    pub fn new(enabled: bool, db: DBConnection) -> Self {
        Self {
            enabled,
            store: MainnetSignupStore { db },
            pow: SignupPow::fixed(FEEDBACK_POW_BITS),
            limits: FeedbackLimits::default(),
        }
    }
}

/// Accept a single ASCII mailbox, including plus addressing; never silently truncate it.
/// Case and surrounding whitespace do not create another signup.
pub fn normalize_email(value: &str) -> Option<String> {
    let email = value.trim();
    if email.len() > MAX_EMAIL_BYTES || !email.is_ascii() {
        return None;
    }
    let (local, domain) = email.split_once('@')?;
    if local.is_empty()
        || local.len() > 64
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || !local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&b))
    {
        return None;
    }
    if !domain.contains('.')
        || !domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return None;
    }
    Some(email.to_ascii_lowercase())
}

#[derive(Clone)]
pub struct MainnetSignupStore {
    db: DBConnection,
}

#[derive(sqlx::FromRow)]
pub struct SignupRow {
    pub email: String,
    pub created_at: String,
}

impl MainnetSignupStore {
    /// The unique key makes retries and concurrent submissions idempotent.
    pub async fn insert(&self, email: String, at: OffsetDateTime) -> Result<(), Error> {
        let created_at = at.format(&Rfc3339).expect("UTC timestamp formats");
        self.db.execute_write(move |pool| async move {
            sqlx::query("INSERT INTO mainnet_signups (email, created_at) VALUES (?, ?) ON CONFLICT(email) DO NOTHING")
                .bind(email).bind(created_at).execute(&pool).await.map(|_| ())
        }).await?;
        Ok(())
    }

    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<SignupRow>, Error> {
        Ok(sqlx::query_as("SELECT email, created_at FROM mainnet_signups ORDER BY created_at DESC, email LIMIT ? OFFSET ?")
            .bind(limit).bind(offset).fetch_all(self.db.read()).await?)
    }

    pub async fn count(&self) -> Result<i64, Error> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM mainnet_signups")
            .fetch_one(self.db.read())
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_a_single_email_without_rewriting_invalid_input() {
        assert_eq!(
            normalize_email("  Player+Mainnet@Example.COM  ").as_deref(),
            Some("player+mainnet@example.com")
        );
        for email in [
            "",
            "a",
            "a@",
            "@example.com",
            "a@localhost",
            "a@@example.com",
            "a b@example.com",
            "a\nb@example.com",
            "a@example.com,b@example.com",
            "a@-example.com",
            "a@example..com",
            ".a@example.com",
            "a..b@example.com",
            "a@example.com<script>",
            "a\u{200b}@example.com",
        ] {
            assert!(normalize_email(email).is_none(), "{email:?}");
        }
        assert!(normalize_email(&format!("{}@example.com", "x".repeat(65))).is_none());
        assert!(normalize_email(&format!("a@{}.com", "x".repeat(64))).is_none());
    }
}
