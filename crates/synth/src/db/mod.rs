use anyhow::{Context, Result};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    SqlitePool,
};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SynthDb {
    pool: SqlitePool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct TestRun {
    pub id: String,
    pub scenario: String,
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub config_json: Option<String>,
    /// The competition the run created, saved with the step that created it.
    #[sqlx(default)]
    pub competition_id: Option<String>,
    /// Where the run's money stands, once synth has followed it: see [`crate::trail::Money`].
    #[sqlx(default)]
    pub money: Option<String>,
}

/// A run whose money is stuck, for the dashboard's list of held money.
#[derive(Debug, Clone)]
pub struct HeldRun {
    pub run: TestRun,
    pub trail: crate::trail::Trail,
}

/// How a run's money was last judged, saved in one transaction with what the verdict does to the
/// run, so a restart cannot leave one without the other.
pub struct Verdict<'a> {
    pub trail: &'a crate::trail::Trail,
    /// Whether synth should keep looking at the run's money.
    pub follow: bool,
    /// A step to add to the run: its name, how long after the run it came, its error, and its
    /// details.
    pub step: Option<(&'a str, i64, Option<&'a str>, &'a str)>,
    /// Fail a run that passed its steps, with this error.
    pub fail_passed_run: Option<&'a str>,
}

/// A scenario's recent record, for the dashboard.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ScenarioHealth {
    pub scenario: String,
    pub runs: i64,
    pub passed: i64,
    pub failed: i64,
    pub last_started_at: String,
    pub last_status: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct TestStep {
    pub id: String,
    pub run_id: String,
    pub step_name: String,
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub details_json: Option<String>,
    #[sqlx(default)]
    pub error_message: Option<String>,
}

/// A rebalance about to be recorded.
#[derive(Debug, Clone)]
pub struct Rebalance {
    /// `channel`, for the payer's channel, or `arkade`, for ark-swapd's wallet.
    pub kind: &'static str,
    /// The channel, or `ark-swapd` for its wallet.
    pub channel_id: String,
    pub amount_sats: u64,
    pub local_before_sats: u64,
    pub capacity_sats: u64,
    /// The on-chain transaction, for an Arkade top-up.
    pub txid: Option<String>,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct RebalanceRecord {
    pub id: String,
    #[sqlx(default)]
    pub kind: Option<String>,
    #[sqlx(default)]
    pub txid: Option<String>,
    pub channel_id: String,
    pub amount_sats: i64,
    pub local_before_sats: i64,
    pub capacity_sats: i64,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SynthUserRecord {
    pub id: String,
    pub name: String,
    pub nostr_secret_key: String,
    pub nostr_pubkey: String,
    pub created_at: String,
}

/// A run, with where its money stands.
const RUN_COLUMNS: &str = "SELECT test_runs.*, money_trails.money AS money FROM test_runs \
     LEFT JOIN money_trails ON money_trails.run_id = test_runs.id";

/// Bumped with each migration that rewrites existing rows, so each runs once.
const SCHEMA_VERSION: i64 = 1;

fn now_rfc3339() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?)
}

impl SynthDb {
    pub async fn new(path: &str) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent).context("Failed to create database directory")?;
        }

        let url = format!("sqlite:{}?mode=rwc", path);
        // The tracker can backfill thousands of old runs. Its commits must not block page
        // readers, and a confirmed money trail must remain durable across a power loss.
        let options = url
            .parse::<SqliteConnectOptions>()?
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
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
                details_json TEXT,
                error_message TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Databases made before steps kept their errors.
        self.add_column_if_missing("test_steps", "error_message", "TEXT")
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

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS rebalances (
                id TEXT PRIMARY KEY,
                channel_id TEXT NOT NULL,
                amount_sats INTEGER NOT NULL,
                local_before_sats INTEGER NOT NULL,
                capacity_sats INTEGER NOT NULL,
                status TEXT NOT NULL,
                error_message TEXT,
                created_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS money_trails (
                run_id TEXT PRIMARY KEY REFERENCES test_runs(id),
                money TEXT NOT NULL,
                trail_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Databases made before synth kept ark-swapd's wallet funded.
        self.add_column_if_missing("rebalances", "kind", "TEXT NOT NULL DEFAULT 'channel'")
            .await?;
        self.add_column_if_missing("rebalances", "txid", "TEXT")
            .await?;
        // The tracker reads which competition each run made on every tick, so it is saved with
        // the run rather than dug out of the steps' details each time.
        self.add_column_if_missing("test_runs", "competition_id", "TEXT")
            .await?;
        // Whether the tracker still looks at a run's money: while it moves, and while it is
        // stuck, until it moves again.
        self.add_column_if_missing("money_trails", "follow", "INTEGER NOT NULL DEFAULT 1")
            .await?;

        // Hourly runs add a few dozen steps an hour. Without these, listing runs and reading a
        // run's steps scan every step ever recorded.
        sqlx::query("CREATE INDEX IF NOT EXISTS test_steps_by_run ON test_steps (run_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS test_runs_by_start ON test_runs (started_at)")
            .execute(&self.pool)
            .await?;
        // Covers the scenario health query, which reads nothing else.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS test_runs_by_scenario_start \
             ON test_runs (scenario, started_at DESC, status)",
        )
        .execute(&self.pool)
        .await?;

        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&self.pool)
            .await?;
        if version < 1 {
            // Once: runs made before the column existed name their competition in a step.
            sqlx::query(
                "UPDATE test_runs SET competition_id = ( \
                     SELECT CASE WHEN json_valid(details_json) \
                         THEN json_extract(details_json, '$.competition_id') END FROM test_steps \
                     WHERE test_steps.run_id = test_runs.id AND CASE WHEN json_valid(details_json) \
                         THEN json_extract(details_json, '$.competition_id') END IS NOT NULL \
                     LIMIT 1) \
                 WHERE competition_id IS NULL",
            )
            .execute(&self.pool)
            .await?;
            sqlx::query("UPDATE money_trails SET follow = money IN ('following', 'stuck')")
                .execute(&self.pool)
                .await?;
        }
        if version < SCHEMA_VERSION {
            sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
                .execute(&self.pool)
                .await?;
        }

        Ok(())
    }

    async fn add_column_if_missing(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<()> {
        let exists: bool =
            sqlx::query_scalar("SELECT count(*) > 0 FROM pragma_table_info(?) WHERE name = ?")
                .bind(table)
                .bind(column)
                .fetch_one(&self.pool)
                .await?;
        if !exists {
            sqlx::query(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    // --- Rebalances ---

    pub async fn record_rebalance(&self, rebalance: &Rebalance) -> Result<()> {
        let now =
            OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?;
        let status = if rebalance.error.is_some() {
            "failed"
        } else {
            "moved"
        };
        sqlx::query(
            "INSERT INTO rebalances (id, kind, channel_id, amount_sats, local_before_sats, \
             capacity_sats, txid, status, error_message, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(rebalance.kind)
        .bind(&rebalance.channel_id)
        .bind(rebalance.amount_sats as i64)
        .bind(rebalance.local_before_sats as i64)
        .bind(rebalance.capacity_sats as i64)
        .bind(&rebalance.txid)
        .bind(status)
        .bind(&rebalance.error)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// When the last rebalance of `kind` that went through was made.
    pub async fn last_rebalance_at(&self, kind: &str) -> Result<Option<OffsetDateTime>> {
        let at: Option<String> = sqlx::query_scalar(
            "SELECT created_at FROM rebalances WHERE kind = ? AND status = 'moved' \
             ORDER BY rowid DESC LIMIT 1",
        )
        .bind(kind)
        .fetch_optional(&self.pool)
        .await?;
        at.map(|at| OffsetDateTime::parse(&at, &time::format_description::well_known::Rfc3339))
            .transpose()
            .context("read when the last rebalance was")
    }

    pub async fn list_rebalances(&self, limit: i64) -> Result<Vec<RebalanceRecord>> {
        let rebalances = sqlx::query_as::<_, RebalanceRecord>(
            "SELECT * FROM rebalances ORDER BY rowid DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rebalances)
    }

    // --- Test Runs ---

    pub async fn create_run(&self, scenario: &str, config_json: Option<&str>) -> Result<String> {
        let id = Uuid::now_v7().to_string();
        let now =
            OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?;

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
        let now =
            OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?;
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
        let runs = sqlx::query_as::<_, TestRun>(&format!(
            "{RUN_COLUMNS} ORDER BY started_at DESC LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(runs)
    }

    /// Pending money must stay visible even after newer runs fill the dashboard's page.
    pub async fn runs_to_follow(&self) -> Result<Vec<TestRun>> {
        sqlx::query_as::<_, TestRun>(&format!(
            "{RUN_COLUMNS} WHERE test_runs.competition_id IS NOT NULL \
             AND (money_trails.run_id IS NULL OR money_trails.follow = 1) ORDER BY started_at"
        ))
        .fetch_all(&self.pool)
        .await
        .context("list runs whose money is still being followed")
    }

    /// Runs whose money is stuck now, with their trails, oldest first.
    pub async fn held_runs(&self) -> Result<Vec<HeldRun>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            #[sqlx(flatten)]
            run: TestRun,
            trail_json: String,
        }
        let rows = sqlx::query_as::<_, Row>(
            "SELECT test_runs.*, money_trails.money AS money, money_trails.trail_json \
             FROM money_trails JOIN test_runs ON test_runs.id = money_trails.run_id \
             WHERE money_trails.money = 'stuck' ORDER BY test_runs.started_at",
        )
        .fetch_all(&self.pool)
        .await
        .context("list runs whose money is stuck")?;
        rows.into_iter()
            .map(|row| {
                Ok(HeldRun {
                    trail: serde_json::from_str(&row.trail_json)
                        .with_context(|| format!("read run {}'s money trail", row.run.id))?,
                    run: row.run,
                })
            })
            .collect()
    }

    /// How each scenario has been doing, most recently run first.
    ///
    /// A scenario is only as good as its last runs, so this counts the recent window rather than
    /// all of history: a run that failed a month ago says nothing about the deployment today.
    pub async fn scenario_health(&self, window: i64) -> Result<Vec<ScenarioHealth>> {
        let health = sqlx::query_as::<_, ScenarioHealth>(
            "WITH recent AS (
                 SELECT scenario, status, started_at,
                        ROW_NUMBER() OVER (PARTITION BY scenario ORDER BY started_at DESC) AS age
                 FROM test_runs
             )
             SELECT scenario,
                    COUNT(*) AS runs,
                    SUM(CASE WHEN status = 'passed' THEN 1 ELSE 0 END) AS passed,
                    SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) AS failed,
                    MAX(started_at) AS last_started_at,
                    (SELECT status FROM recent inner_recent
                      WHERE inner_recent.scenario = recent.scenario AND inner_recent.age = 1)
                      AS last_status
             FROM recent WHERE age <= ?
             GROUP BY scenario ORDER BY last_started_at DESC",
        )
        .bind(window)
        .fetch_all(&self.pool)
        .await?;

        Ok(health)
    }

    pub async fn get_run(&self, id: &str) -> Result<Option<TestRun>> {
        let run = sqlx::query_as::<_, TestRun>(&format!("{RUN_COLUMNS} WHERE test_runs.id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(run)
    }

    /// Mark the runs a restart cut short, and the steps they were on, which would otherwise read
    /// as running forever. Call before any run starts. Returns how many runs there were.
    pub async fn interrupt_unfinished_runs(&self) -> Result<u64> {
        let now = now_rfc3339()?;
        let mut transaction = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE test_runs SET status = 'interrupted', completed_at = ?, \
             error_message = 'synth restarted before the run finished' WHERE status = 'running'",
        )
        .bind(&now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE test_steps SET status = 'interrupted', completed_at = ?, \
             error_message = 'synth restarted before the step finished' WHERE status = 'running'",
        )
        .bind(&now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(done.rows_affected())
    }

    // --- Money trails ---

    /// Save a run's trail, and what its verdict does to the run, together.
    pub async fn record_money(&self, run_id: &str, verdict: &Verdict<'_>) -> Result<()> {
        let now = now_rfc3339()?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO money_trails (run_id, money, trail_json, updated_at, follow) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(run_id) DO UPDATE SET money = excluded.money, \
             trail_json = excluded.trail_json, updated_at = excluded.updated_at, \
             follow = excluded.follow",
        )
        .bind(run_id)
        .bind(verdict.trail.money.label())
        .bind(serde_json::to_string(verdict.trail)?)
        .bind(&now)
        .bind(verdict.follow)
        .execute(&mut *transaction)
        .await?;
        if let Some((name, duration_ms, error, details)) = verdict.step {
            sqlx::query(
                "INSERT INTO test_steps (id, run_id, step_name, status, started_at, completed_at, \
                 duration_ms, details_json, error_message) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(run_id)
            .bind(name)
            .bind(if error.is_some() { "failed" } else { "passed" })
            .bind(&now)
            .bind(&now)
            .bind(duration_ms)
            .bind(details)
            .bind(error)
            .execute(&mut *transaction)
            .await?;
        }
        // A run that already failed keeps its own error.
        if let Some(error) = verdict.fail_passed_run {
            sqlx::query(
                "UPDATE test_runs SET status = 'failed', error_message = ? \
                 WHERE id = ? AND status = 'passed'",
            )
            .bind(error)
            .bind(run_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn get_trail(&self, run_id: &str) -> Result<Option<crate::trail::Trail>> {
        let json: Option<String> =
            sqlx::query_scalar("SELECT trail_json FROM money_trails WHERE run_id = ?")
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await?;
        json.map(|json| serde_json::from_str(&json).context("read the saved money trail"))
            .transpose()
    }

    // --- Test Steps ---

    /// Record a step that happened after its run's scenario ended.
    pub async fn add_step(
        &self,
        run_id: &str,
        step_name: &str,
        duration_ms: i64,
        error: Option<&str>,
        details_json: Option<&str>,
    ) -> Result<()> {
        let id = self.create_step(run_id, step_name).await?;
        self.complete_step(&id, duration_ms, error, details_json)
            .await
    }

    /// Keep what a step has done so far, before it finishes.
    pub async fn update_step_details(&self, id: &str, details_json: &str) -> Result<()> {
        sqlx::query("UPDATE test_steps SET details_json = ? WHERE id = ?")
            .bind(details_json)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn create_step(&self, run_id: &str, step_name: &str) -> Result<String> {
        let id = Uuid::now_v7().to_string();
        let now =
            OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?;

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
        let now =
            OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?;
        let status = if error.is_some() { "failed" } else { "passed" };

        sqlx::query(
            "UPDATE test_steps SET status = ?, completed_at = ?, duration_ms = ?, details_json = ?, \
             error_message = ? WHERE id = ?",
        )
        .bind(status)
        .bind(&now)
        .bind(duration_ms)
        .bind(details_json)
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        // The step that made the run's competition names it, and the run keeps it.
        let competition = details_json
            .and_then(|details| serde_json::from_str::<serde_json::Value>(details).ok())
            .and_then(|details| details.get("competition_id")?.as_str().map(str::to_owned));
        if let Some(competition) = competition {
            sqlx::query(
                "UPDATE test_runs SET competition_id = ? WHERE competition_id IS NULL \
                 AND id = (SELECT run_id FROM test_steps WHERE id = ?)",
            )
            .bind(competition)
            .bind(id)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    pub async fn get_steps(&self, run_id: &str) -> Result<Vec<TestStep>> {
        let steps = sqlx::query_as::<_, TestStep>(
            "SELECT * FROM test_steps WHERE run_id = ? ORDER BY rowid",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(steps)
    }

    // --- Synth Users ---

    pub async fn get_or_create_user(&self, name: &str) -> Result<SynthUserRecord> {
        // Try to find existing user
        if let Some(user) =
            sqlx::query_as::<_, SynthUserRecord>("SELECT * FROM synth_users WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
        {
            return Ok(user);
        }

        // Create new user
        let user = crate::crypto::keys::SynthUser::new_random(name)?;
        let id = Uuid::now_v7().to_string();
        let now =
            OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?;

        sqlx::query(
            "INSERT INTO synth_users (id, name, nostr_secret_key, nostr_pubkey, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(user.nostr_secret_key_hex())
        .bind(user.nostr_pubkey_hex())
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
        let users = sqlx::query_as::<_, SynthUserRecord>("SELECT * FROM synth_users ORDER BY name")
            .fetch_all(&self.pool)
            .await?;

        Ok(users)
    }
}

#[cfg(test)]
impl SynthDb {
    /// Fill the database with `runs` hourly lifecycle runs, each with its steps and a settled
    /// money trail, the oldest first: for measuring how pages hold up as history grows.
    pub(crate) async fn seed_history(&self, runs: usize, trail_json: &str) -> Result<Vec<String>> {
        let mut transaction = self.pool.begin().await?;
        let start = OffsetDateTime::now_utc() - time::Duration::hours(runs as i64);
        let mut ids = Vec::with_capacity(runs);
        for index in 0..runs {
            let id = Uuid::now_v7().to_string();
            let at = (start + time::Duration::hours(index as i64))
                .format(&time::format_description::well_known::Rfc3339)?;
            let competition = Uuid::now_v7().to_string();
            sqlx::query(
                "INSERT INTO test_runs (id, scenario, status, started_at, completed_at, competition_id) \
                 VALUES (?, 'full_lifecycle', 'passed', ?, ?, ?)",
            )
            .bind(&id)
            .bind(&at)
            .bind(&at)
            .bind(&competition)
            .execute(&mut *transaction)
            .await?;
            let mut steps = vec![(
                "create_competition".to_string(),
                serde_json::json!({ "competition_id": competition }).to_string(),
            )];
            for user in ["alice", "bob", "charlie"] {
                steps.push((
                    format!("user_{user}_enter"),
                    serde_json::json!({
                        "user": user, "nostr_pubkey": "00".repeat(32), "paid": true,
                        "entry_submitted": true, "amount_sats": 1100,
                        "ticket_id": Uuid::now_v7(), "entry_id": Uuid::now_v7(),
                        "payment_hash": "ab".repeat(32), "invoice": "lntbs11u1".repeat(40),
                    })
                    .to_string(),
                ));
            }
            for wait in 0..9 {
                steps.push((format!("wait_{wait}"), "{}".to_string()));
            }
            for (name, details) in steps {
                sqlx::query(
                    "INSERT INTO test_steps (id, run_id, step_name, status, started_at, \
                     completed_at, duration_ms, details_json) VALUES (?, ?, ?, 'passed', ?, ?, 1, ?)",
                )
                .bind(Uuid::now_v7().to_string())
                .bind(&id)
                .bind(name)
                .bind(&at)
                .bind(&at)
                .bind(details)
                .execute(&mut *transaction)
                .await?;
            }
            sqlx::query(
                "INSERT INTO money_trails (run_id, money, trail_json, updated_at, follow) \
                 VALUES (?, 'paid_out', ?, ?, 0)",
            )
            .bind(&id)
            .bind(trail_json)
            .bind(&at)
            .execute(&mut *transaction)
            .await?;
            ids.push(id);
        }
        transaction.commit().await?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tracker commits a verdict every few seconds while pages read. In WAL mode a reader
    /// holds a snapshot rather than a lock, so a page part way through reading does not hold up
    /// the tracker's commit, and reads what was committed when it began. With a rollback
    /// journal the commit would wait for the reader. An open read transaction stands in for a
    /// long read here.
    #[tokio::test]
    async fn a_page_part_way_through_a_read_does_not_hold_up_the_trackers_commit() {
        use crate::trail::Money;
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            synchronous, 2,
            "FULL: a saved verdict survives a power loss"
        );
        let run_id = db.create_run("full_lifecycle", None).await.unwrap();
        db.complete_run(&run_id, None).await.unwrap();

        let mut reader = db.pool.acquire().await.unwrap();
        sqlx::query("BEGIN").execute(&mut *reader).await.unwrap();
        let trails = "SELECT count(*) FROM money_trails";
        let before: i64 = sqlx::query_scalar(trails)
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        assert_eq!(before, 0);

        let following = trail(Money::Following);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            db.record_money(
                &run_id,
                &Verdict {
                    trail: &following,
                    follow: true,
                    step: None,
                    fail_passed_run: None,
                },
            ),
        )
        .await
        .expect("the tracker's commit must not wait for a reader")
        .unwrap();
        let during: i64 = sqlx::query_scalar(trails)
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        assert_eq!(during, 0, "the reader keeps the snapshot it began with");
        sqlx::query("COMMIT").execute(&mut *reader).await.unwrap();
        assert!(db.get_trail(&run_id).await.unwrap().is_some());
    }

    /// A scenario is judged by its recent runs, so an old failure must not follow it forever.
    #[tokio::test]
    async fn health_counts_a_scenario_by_its_recent_runs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.sqlite");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();

        let run = |db: SynthDb, scenario: &'static str, error: Option<&'static str>| async move {
            let id = db.create_run(scenario, None).await.unwrap();
            db.complete_run(&id, error).await.unwrap();
        };
        run(db.clone(), "escrow_refund", Some("no refund")).await;
        run(db.clone(), "escrow_refund", None).await;
        run(db.clone(), "full_lifecycle", None).await;

        let health = db.scenario_health(20).await.unwrap();
        let refund = health
            .iter()
            .find(|health| health.scenario == "escrow_refund")
            .expect("the refund scenario is listed");
        assert_eq!((refund.runs, refund.passed, refund.failed), (2, 1, 1));
        assert_eq!(refund.last_status, "passed", "its latest run decides");

        // Narrowing the window drops the older failure, leaving only what it did last.
        let recent = db.scenario_health(1).await.unwrap();
        let refund = recent
            .iter()
            .find(|health| health.scenario == "escrow_refund")
            .unwrap();
        assert_eq!((refund.runs, refund.passed, refund.failed), (1, 1, 0));

        // Scenarios are listed by when they last ran, most recent first.
        assert_eq!(
            health
                .iter()
                .map(|h| h.scenario.as_str())
                .collect::<Vec<_>>(),
            ["full_lifecycle", "escrow_refund"]
        );
    }

    /// A run a restart cut short says so, and so does the step it was on, rather than running
    /// forever.
    #[tokio::test]
    async fn runs_cut_short_by_a_restart_are_marked_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.sqlite");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        let finished = db.create_run("full_lifecycle", None).await.unwrap();
        db.add_step(&finished, "load_users", 1, None, None)
            .await
            .unwrap();
        db.complete_run(&finished, None).await.unwrap();
        let cut_short = db.create_run("full_lifecycle", None).await.unwrap();
        db.create_step(&cut_short, "user_alice_enter")
            .await
            .unwrap();

        assert_eq!(db.interrupt_unfinished_runs().await.unwrap(), 1);
        let run = db.get_run(&cut_short).await.unwrap().unwrap();
        assert_eq!(run.status, "interrupted");
        assert!(run.completed_at.is_some());
        let steps = db.get_steps(&cut_short).await.unwrap();
        assert_eq!(steps[0].status, "interrupted");
        assert!(steps[0].completed_at.is_some());
        assert_eq!(
            db.get_run(&finished).await.unwrap().unwrap().status,
            "passed"
        );
        assert_eq!(db.get_steps(&finished).await.unwrap()[0].status, "passed");
    }

    fn trail(money: crate::trail::Money) -> crate::trail::Trail {
        crate::trail::Trail {
            refreshed_at: OffsetDateTime::now_utc(),
            competition_id: Uuid::now_v7(),
            competition: None,
            settlement: None,
            swaps: Vec::new(),
            late_payments: Vec::new(),
            payouts: Vec::new(),
            refunds: Vec::new(),
            funding_tx: None,
            outcome_tx: None,
            closing_txs: Vec::new(),
            money,
            held: None,
            gaps: Vec::new(),
        }
    }

    /// A run's trail is saved over as the tracker learns more, and its money shows on the run.
    /// A verdict's step and its failing the run are saved with the trail, or not at all.
    #[tokio::test]
    async fn a_runs_money_trail_is_saved_with_what_its_verdict_does() {
        use crate::trail::Money;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.sqlite");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        let run = db.create_run("full_lifecycle", None).await.unwrap();
        db.add_step(
            &run,
            "create_competition",
            1,
            None,
            Some(&serde_json::json!({"competition_id": "c"}).to_string()),
        )
        .await
        .unwrap();
        db.complete_run(&run, None).await.unwrap();
        assert!(db.get_trail(&run).await.unwrap().is_none());
        assert_eq!(
            db.get_run(&run)
                .await
                .unwrap()
                .unwrap()
                .competition_id
                .as_deref(),
            Some("c"),
            "the step that made the competition names it on the run"
        );

        let following = trail(Money::Following);
        let verdict = |trail| Verdict {
            trail,
            follow: true,
            step: None,
            fail_passed_run: None,
        };
        db.record_money(&run, &verdict(&following)).await.unwrap();
        assert_eq!(db.runs_to_follow().await.unwrap().len(), 1);

        let stuck = trail(Money::Stuck {
            reason: "0 of 3 payouts sent".into(),
            since: OffsetDateTime::now_utc(),
        });
        db.record_money(
            &run,
            &Verdict {
                trail: &stuck,
                follow: true,
                step: Some(("money_settled", 5, Some("0 of 3 payouts sent"), "{}")),
                fail_passed_run: Some("its payouts never went out"),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            db.get_trail(&run).await.unwrap().unwrap().money,
            stuck.money
        );
        assert_eq!(
            db.list_runs(10).await.unwrap()[0].money.as_deref(),
            Some("stuck")
        );
        let failed = db.get_run(&run).await.unwrap().unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.error_message.as_deref(),
            Some("its payouts never went out")
        );
        let steps = db.get_steps(&run).await.unwrap();
        assert_eq!(steps.last().unwrap().step_name, "money_settled");
        assert_eq!(steps.last().unwrap().status, "failed");
        assert_eq!(
            db.runs_to_follow().await.unwrap().len(),
            1,
            "stuck is watched"
        );
        let held = db.held_runs().await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].run.id, run);

        let refunded = trail(Money::Refunded);
        db.record_money(
            &run,
            &Verdict {
                follow: false,
                ..verdict(&refunded)
            },
        )
        .await
        .unwrap();
        assert!(db.runs_to_follow().await.unwrap().is_empty());
        assert!(db.held_runs().await.unwrap().is_empty());
    }

    /// Databases made before runs kept their competition find it in the steps, once.
    #[tokio::test]
    async fn older_runs_find_their_competition_in_their_steps() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.sqlite");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        let run = db.create_run("full_lifecycle", None).await.unwrap();
        db.add_step(&run, "bad", 1, None, Some("{not json"))
            .await
            .unwrap();
        db.add_step(
            &run,
            "create_competition",
            1,
            None,
            Some(&serde_json::json!({"competition_id": "c"}).to_string()),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE test_runs SET competition_id = NULL")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("PRAGMA user_version = 0")
            .execute(&db.pool)
            .await
            .unwrap();
        db.pool.close().await;

        let reopened = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        assert_eq!(
            reopened
                .get_run(&run)
                .await
                .unwrap()
                .unwrap()
                .competition_id
                .as_deref(),
            Some("c")
        );
    }

    #[tokio::test]
    async fn older_unsettled_runs_remain_in_the_tracking_queue() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let old = db.create_run("full_lifecycle", None).await.unwrap();
        db.add_step(&old, "user_alice_enter", 1, None, Some("{"))
            .await
            .unwrap();
        let details = serde_json::json!({"competition_id": Uuid::now_v7()}).to_string();
        db.add_step(&old, "create_competition", 1, None, Some(&details))
            .await
            .unwrap();
        db.complete_run(&old, None).await.unwrap();
        for _ in 0..31 {
            let newer = db.create_run("full_lifecycle", None).await.unwrap();
            db.complete_run(&newer, None).await.unwrap();
        }
        assert!(!db
            .list_runs(30)
            .await
            .unwrap()
            .iter()
            .any(|run| run.id == old));
        let pending = db.runs_to_follow().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, old);
    }

    /// A failed rebalance is kept with its reason, so the dashboard can say why money stopped
    /// moving.
    #[tokio::test]
    async fn rebalances_are_listed_with_what_went_wrong() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.sqlite");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();

        let rebalance = |error: Option<&str>| Rebalance {
            kind: "channel",
            channel_id: "1".into(),
            amount_sats: 200_000,
            local_before_sats: 250_000,
            capacity_sats: 1_000_000,
            txid: None,
            error: error.map(str::to_owned),
        };
        db.record_rebalance(&rebalance(None)).await.unwrap();
        db.record_rebalance(&rebalance(Some("no route")))
            .await
            .unwrap();

        let listed = db.list_rebalances(10).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|r| (r.status.as_str(), r.error_message.as_deref()))
                .collect::<Vec<_>>(),
            [("failed", Some("no route")), ("moved", None)]
        );
        assert_eq!(listed[1].amount_sats, 200_000);
    }
}
