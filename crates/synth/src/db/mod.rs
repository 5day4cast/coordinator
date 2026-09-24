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

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct TestRun {
    pub id: String,
    pub scenario: String,
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub config_json: Option<String>,
    /// The competition the run created, from its steps' details.
    #[sqlx(default)]
    pub competition_id: Option<String>,
    /// Where the run's money stands, once synth has followed it: see [`crate::trail::Money`].
    #[sqlx(default)]
    pub money: Option<String>,
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

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
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

/// A run, with the competition its steps name and where its money stands.
const RUN_COLUMNS: &str = "SELECT test_runs.*, \
     (SELECT CASE WHEN json_valid(details_json) THEN json_extract(details_json, '$.competition_id') END FROM test_steps \
      WHERE test_steps.run_id = test_runs.id \
      AND CASE WHEN json_valid(details_json) THEN json_extract(details_json, '$.competition_id') END IS NOT NULL LIMIT 1) AS competition_id, \
     (SELECT money FROM money_trails WHERE money_trails.run_id = test_runs.id) AS money \
     FROM test_runs";

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

        // Hourly runs add a few dozen steps an hour. Without these, listing runs and reading a
        // run's steps scan every step ever recorded.
        sqlx::query("CREATE INDEX IF NOT EXISTS test_steps_by_run ON test_steps (run_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS test_runs_by_start ON test_runs (started_at)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS test_runs_by_scenario_start \
             ON test_runs (scenario, started_at DESC, status)",
        )
        .execute(&self.pool)
        .await?;

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
            "SELECT * FROM ({RUN_COLUMNS}) WHERE competition_id IS NOT NULL \
             AND (money IS NULL OR money = 'following') ORDER BY started_at"
        ))
        .fetch_all(&self.pool)
        .await
        .context("list runs whose money is still being followed")
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
        let run = sqlx::query_as::<_, TestRun>(&format!("{RUN_COLUMNS} WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(run)
    }

    /// Mark the runs a restart cut short, which would otherwise read as running forever. Call
    /// before any run starts. Returns how many there were.
    pub async fn interrupt_unfinished_runs(&self) -> Result<u64> {
        let done = sqlx::query(
            "UPDATE test_runs SET status = 'interrupted', completed_at = ?, \
             error_message = 'synth restarted before the run finished' WHERE status = 'running'",
        )
        .bind(now_rfc3339()?)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// Fail a run that passed its steps but whose money did not end where it should. A run that
    /// already failed keeps its own error.
    pub async fn fail_passed_run(&self, id: &str, error: &str) -> Result<()> {
        sqlx::query(
            "UPDATE test_runs SET status = 'failed', error_message = ? \
             WHERE id = ? AND status = 'passed'",
        )
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // --- Money trails ---

    pub async fn save_trail(&self, run_id: &str, trail: &crate::trail::Trail) -> Result<()> {
        sqlx::query(
            "INSERT INTO money_trails (run_id, money, trail_json, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(run_id) DO UPDATE SET money = excluded.money, \
             trail_json = excluded.trail_json, updated_at = excluded.updated_at",
        )
        .bind(run_id)
        .bind(trail.money.label())
        .bind(serde_json::to_string(trail)?)
        .bind(now_rfc3339()?)
        .execute(&self.pool)
        .await?;
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
mod tests {
    use super::*;

    /// Even an exclusive backfill transaction leaves pages reading the last committed state.
    #[tokio::test]
    async fn dashboard_reads_continue_while_a_tracker_write_is_open() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let run_id = db.create_run("full_lifecycle", None).await.unwrap();
        db.complete_run(&run_id, None).await.unwrap();
        let details = serde_json::json!({"competition_id": Uuid::now_v7()}).to_string();
        db.add_step(&run_id, "create_competition", 1, None, Some(&details))
            .await
            .unwrap();

        let mut writer = db.pool.acquire().await.unwrap();
        // Open the reader before taking the writer lock, as production's pool does.
        let reader = db.pool.acquire().await.unwrap();
        sqlx::query("BEGIN EXCLUSIVE")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("UPDATE test_runs SET status = 'failed' WHERE id = ?")
            .bind(&run_id)
            .execute(&mut *writer)
            .await
            .unwrap();
        drop(reader);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            assert_eq!(db.list_runs(10).await.unwrap()[0].status, "passed");
            assert_eq!(db.get_run(&run_id).await.unwrap().unwrap().status, "passed");
            assert_eq!(db.scenario_health(20).await.unwrap()[0].passed, 1);
            assert_eq!(db.runs_to_follow().await.unwrap().len(), 1);
        })
        .await
        .expect("page reads must not wait for the writer to commit");

        sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
        assert_eq!(db.get_run(&run_id).await.unwrap().unwrap().status, "failed");
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

    /// A run a restart cut short says so, rather than running forever.
    #[tokio::test]
    async fn runs_cut_short_by_a_restart_are_marked_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.sqlite");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        let finished = db.create_run("full_lifecycle", None).await.unwrap();
        db.complete_run(&finished, None).await.unwrap();
        let cut_short = db.create_run("full_lifecycle", None).await.unwrap();

        assert_eq!(db.interrupt_unfinished_runs().await.unwrap(), 1);
        let run = db.get_run(&cut_short).await.unwrap().unwrap();
        assert_eq!(run.status, "interrupted");
        assert!(run.completed_at.is_some());
        assert_eq!(
            db.get_run(&finished).await.unwrap().unwrap().status,
            "passed"
        );
    }

    /// A run's trail is saved over as the tracker learns more, and its money shows on the run.
    #[tokio::test]
    async fn a_runs_money_trail_is_saved_and_listed_with_it() {
        use crate::trail::{Money, Trail};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.sqlite");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        let run = db.create_run("full_lifecycle", None).await.unwrap();
        db.complete_run(&run, None).await.unwrap();
        assert!(db.get_trail(&run).await.unwrap().is_none());

        let mut trail = Trail {
            refreshed_at: OffsetDateTime::now_utc(),
            competition_id: Uuid::now_v7(),
            competition: None,
            settlement: None,
            swaps: Vec::new(),
            payouts: Vec::new(),
            refunds: Vec::new(),
            funding_tx: None,
            outcome_tx: None,
            money: Money::Following,
            gaps: Vec::new(),
        };
        db.save_trail(&run, &trail).await.unwrap();
        trail.money = Money::Stuck {
            reason: "0 of 3 payouts sent".into(),
        };
        db.save_trail(&run, &trail).await.unwrap();

        assert_eq!(
            db.get_trail(&run).await.unwrap().unwrap().money,
            trail.money
        );
        assert_eq!(
            db.list_runs(10).await.unwrap()[0].money.as_deref(),
            Some("stuck")
        );

        db.fail_passed_run(&run, "its payouts never went out")
            .await
            .unwrap();
        let failed = db.get_run(&run).await.unwrap().unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.error_message.as_deref(),
            Some("its payouts never went out")
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
