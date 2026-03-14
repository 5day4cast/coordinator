use anyhow::{Context, Result};
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SynthDb {
    pool: SqlitePool,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct TestRun {
    pub id: String,
    pub scenario: String,
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub config_json: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TestStep {
    pub id: String,
    pub run_id: String,
    pub step_name: String,
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub details_json: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SynthUserRecord {
    pub id: String,
    pub name: String,
    pub nostr_secret_key: String,
    pub nostr_pubkey: String,
    pub created_at: String,
}

impl SynthDb {
    pub async fn new(path: &str) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create database directory")?;
        }

        let url = format!("sqlite:{}?mode=rwc", path);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .context("Failed to connect to SQLite database")?;

        let db = Self { pool };
        db.run_migrations().await?;
        Ok(db)
    }

    async fn run_migrations(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS test_runs (
                id TEXT PRIMARY KEY,
                scenario TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'running',
                started_at TEXT NOT NULL,
                completed_at TEXT,
                error_message TEXT,
                config_json TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS test_steps (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL REFERENCES test_runs(id),
                step_name TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'running',
                started_at TEXT NOT NULL,
                completed_at TEXT,
                duration_ms INTEGER,
                details_json TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS synth_users (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                nostr_secret_key TEXT NOT NULL,
                nostr_pubkey TEXT NOT NULL,
                created_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // --- Test Runs ---

    pub async fn create_run(&self, scenario: &str, config_json: Option<&str>) -> Result<String> {
        let id = Uuid::now_v7().to_string();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;

        sqlx::query(
            "INSERT INTO test_runs (id, scenario, status, started_at, config_json) VALUES (?, ?, 'running', ?, ?)",
        )
        .bind(&id)
        .bind(scenario)
        .bind(&now)
        .bind(config_json)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    pub async fn complete_run(&self, id: &str, error: Option<&str>) -> Result<()> {
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;
        let status = if error.is_some() { "failed" } else { "passed" };

        sqlx::query(
            "UPDATE test_runs SET status = ?, completed_at = ?, error_message = ? WHERE id = ?",
        )
        .bind(status)
        .bind(&now)
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn list_runs(&self, limit: i64) -> Result<Vec<TestRun>> {
        let runs = sqlx::query_as::<_, TestRun>(
            "SELECT * FROM test_runs ORDER BY started_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(runs)
    }

    pub async fn get_run(&self, id: &str) -> Result<Option<TestRun>> {
        let run = sqlx::query_as::<_, TestRun>("SELECT * FROM test_runs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(run)
    }

    // --- Test Steps ---

    pub async fn create_step(&self, run_id: &str, step_name: &str) -> Result<String> {
        let id = Uuid::now_v7().to_string();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;

        sqlx::query(
            "INSERT INTO test_steps (id, run_id, step_name, status, started_at) VALUES (?, ?, ?, 'running', ?)",
        )
        .bind(&id)
        .bind(run_id)
        .bind(step_name)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    pub async fn complete_step(
        &self,
        id: &str,
        duration_ms: i64,
        error: Option<&str>,
        details_json: Option<&str>,
    ) -> Result<()> {
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;
        let status = if error.is_some() { "failed" } else { "passed" };

        sqlx::query(
            "UPDATE test_steps SET status = ?, completed_at = ?, duration_ms = ?, details_json = ? WHERE id = ?",
        )
        .bind(status)
        .bind(&now)
        .bind(duration_ms)
        .bind(details_json)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_steps(&self, run_id: &str) -> Result<Vec<TestStep>> {
        let steps = sqlx::query_as::<_, TestStep>(
            "SELECT * FROM test_steps WHERE run_id = ? ORDER BY started_at ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(steps)
    }

    // --- Synth Users ---

    pub async fn get_or_create_user(
        &self,
        name: &str,
    ) -> Result<SynthUserRecord> {
        // Try to find existing user
        if let Some(user) = sqlx::query_as::<_, SynthUserRecord>(
            "SELECT * FROM synth_users WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        {
            return Ok(user);
        }

        // Create new user
        let user = crate::crypto::keys::SynthUser::new_random(name)?;
        let id = Uuid::now_v7().to_string();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;

        sqlx::query(
            "INSERT INTO synth_users (id, name, nostr_secret_key, nostr_pubkey, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(&user.nostr_secret_key_hex())
        .bind(&user.nostr_pubkey_hex())
        .bind(&now)
        .execute(&self.pool)
        .await?;

        Ok(SynthUserRecord {
            id,
            name: name.to_string(),
            nostr_secret_key: user.nostr_secret_key_hex(),
            nostr_pubkey: user.nostr_pubkey_hex(),
            created_at: now,
        })
    }

    pub async fn list_users(&self) -> Result<Vec<SynthUserRecord>> {
        let users =
            sqlx::query_as::<_, SynthUserRecord>("SELECT * FROM synth_users ORDER BY name")
                .fetch_all(&self.pool)
                .await?;

        Ok(users)
    }
}
