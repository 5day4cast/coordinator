//! `synth run` and `synth runs`: start and follow runs on a running synth, as
//! a client of its HTTP API. Nothing here runs a scenario itself.

use crate::db::{TestRun, TestStep};
use crate::runner::SCENARIOS;
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write};
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "synth",
    about = "Synthetic competitions against a coordinator. Without a command, serve the \
             dashboard and API with the given config file.",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Config file to serve with.
    pub config: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a run on a running synth. It pays for entries from synth's node, so it asks first
    /// unless --yes is given.
    Run(RunArgs),
    /// Runs synth has recorded.
    Runs {
        #[command(flatten)]
        api: ApiArgs,
        #[command(subcommand)]
        action: RunsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum RunsCommand {
    /// The latest runs, newest first.
    List {
        /// How many runs.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// One run: its steps and its money trail.
    Show { id: String },
}

#[derive(Debug, Args)]
pub struct ApiArgs {
    /// The synth to talk to. Its API has no authentication of its own, so reach it only over
    /// a network that admits operators alone.
    #[arg(
        long,
        global = true,
        env = "SYNTH_URL",
        default_value = "http://127.0.0.1:9980"
    )]
    pub url: String,

    /// Print JSON instead of tables.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// What to run: full-lifecycle or escrow-refund.
    #[arg(value_parser = parse_kind)]
    pub kind: String,
    /// Synthetic players; synth's configured number if unset.
    #[arg(long)]
    pub users: Option<usize>,
    /// Follow the run: print each step as it changes, then wait for its money to settle. Exits
    /// 1 if the run fails, 3 if its money is stuck, 4 on --timeout.
    #[arg(long)]
    pub wait: bool,
    /// Seconds between looks at the run while waiting.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
    pub interval: u64,
    /// Give up waiting after this long: 90s, 30m, 6h, or plain seconds.
    #[arg(long, value_parser = parse_duration)]
    pub timeout: Option<Duration>,
    /// Do not ask for confirmation.
    #[arg(long)]
    pub yes: bool,
    #[command(flatten)]
    pub api: ApiArgs,
}

/// A scenario name as synth knows it, from either spelling.
fn parse_kind(value: &str) -> Result<String, String> {
    let kind = value.trim().to_ascii_lowercase().replace('-', "_");
    if SCENARIOS.contains(&kind.as_str()) {
        Ok(kind)
    } else {
        Err(format!(
            "expected one of: {}",
            SCENARIOS
                .iter()
                .map(|s| s.replace('_', "-"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, unit) = value.split_at(split);
    let amount: u64 = amount
        .parse()
        .map_err(|_| format!("{value}: expected a number, such as 90s, 30m or 6h"))?;
    let seconds = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("{value}: the unit must be s, m, h or d")),
    };
    Ok(Duration::from_secs(amount * seconds))
}

/// A run and its steps, as `/api/runs/{id}` gives them.
#[derive(Debug, Clone, Deserialize)]
pub struct RunView {
    pub run: TestRun,
    pub steps: Vec<TestStep>,
    #[serde(default)]
    pub current_step: Option<String>,
}

/// What `/api/run` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct Started {
    pub run_id: String,
    pub scenario: String,
}

/// A client of synth's HTTP API.
pub struct SynthApi {
    http: reqwest::Client,
    url: String,
}

impl SynthApi {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(60))
                .build()?,
            url: url.trim_end_matches('/').to_string(),
        })
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let response = self
            .http
            .get(format!("{}{path}", self.url))
            .send()
            .await
            .with_context(|| format!("reach synth at {}", self.url))?;
        checked(response).await
    }

    async fn post(&self, path: &str) -> Result<reqwest::Response> {
        let response = self
            .http
            .post(format!("{}{path}", self.url))
            .send()
            .await
            .with_context(|| format!("reach synth at {}", self.url))?;
        checked(response).await
    }

    pub async fn start(&self, kind: &str, users: Option<usize>) -> Result<Started> {
        let mut path = format!("/api/run?scenario={kind}");
        if let Some(users) = users {
            let _ = write!(path, "&users={users}");
        }
        let started: Value = self.post(&path).await?.json().await?;
        if started.get("run_id").is_none() {
            bail!("synth started the run but did not say its id; it needs a newer synth");
        }
        Ok(serde_json::from_value(started)?)
    }

    pub async fn run(&self, id: &str) -> Result<RunView> {
        Ok(self.get(&format!("/api/runs/{id}")).await?.json().await?)
    }

    pub async fn runs(&self, limit: i64) -> Result<Vec<TestRun>> {
        #[derive(Deserialize)]
        struct History {
            runs: Vec<TestRun>,
        }
        Ok(self
            .get(&format!("/api/history?limit={limit}"))
            .await?
            .json::<History>()
            .await?
            .runs)
    }

    /// The run's money trail: where its money stands, the ledger, and every hop.
    pub async fn trail(&self, id: &str) -> Result<Value> {
        Ok(self
            .get(&format!("/runs/{id}/trail.json"))
            .await?
            .json()
            .await?)
    }
}

async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|json| json.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or(body);
    bail!("synth answered {status}: {message}")
}

/// Ask on the terminal before doing `what`, unless `yes`. Without a terminal, refuse: a script
/// must say --yes.
fn confirm(what: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("{what}: not confirmed; pass --yes to run without a terminal");
    }
    eprint!("{what}? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("not confirmed; nothing was done")
    }
}

/// How a followed run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its steps passed and its money ended where it should.
    Passed,
    /// Its steps passed; synth stopped following the money before it could verify it.
    Unverified,
    Failed(String),
    Stuck,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Outcome::Passed | Outcome::Unverified => 0,
            Outcome::Failed(_) => 1,
            Outcome::Stuck => 3,
        }
    }
}

/// Exit code for a wait that ran out of time.
pub const TIMED_OUT: i32 = 4;

/// How the run has ended, or None while it, or its money, is still moving.
pub fn outcome(run: &TestRun) -> Option<Outcome> {
    if run.money.as_deref() == Some("stuck") {
        return Some(Outcome::Stuck);
    }
    match run.status.as_str() {
        "running" => None,
        "passed" => match (run.competition_id.as_deref(), run.money.as_deref()) {
            (None, _) => Some(Outcome::Passed),
            (_, Some("paid_out" | "refunded" | "nothing_paid")) => Some(Outcome::Passed),
            (_, Some("unverified")) => Some(Outcome::Unverified),
            _ => None,
        },
        _ => Some(Outcome::Failed(
            run.error_message
                .clone()
                .unwrap_or_else(|| run.status.clone()),
        )),
    }
}

/// Remembers what was last printed about a run, to print only what changed.
#[derive(Debug, Default)]
pub struct Follower {
    steps: HashMap<String, String>,
    current: Option<String>,
    status: Option<String>,
    money: Option<String>,
}

impl Follower {
    /// Lines saying what changed since the last look.
    pub fn changes(&mut self, view: &RunView) -> Vec<String> {
        let mut lines = Vec::new();
        for step in &view.steps {
            if self.steps.get(&step.id) == Some(&step.status) {
                continue;
            }
            let mut line = format!("step {:<32} {}", step.step_name, step.status);
            if let Some(ms) = step.duration_ms {
                let _ = write!(line, " ({})", millis(ms));
            }
            if let Some(error) = &step.error_message {
                let _ = write!(line, ": {error}");
            }
            lines.push(line);
            self.steps.insert(step.id.clone(), step.status.clone());
        }
        if view.current_step != self.current {
            if let Some(step) = &view.current_step {
                if !view
                    .steps
                    .iter()
                    .any(|s| &s.step_name == step && s.status == "running")
                {
                    lines.push(format!("step {step:<32} running"));
                }
            }
            self.current = view.current_step.clone();
        }
        if self.status.as_deref() != Some(view.run.status.as_str()) {
            if self.status.is_some() {
                lines.push(format!("run {}", view.run.status));
            }
            self.status = Some(view.run.status.clone());
        }
        if view.run.money != self.money {
            if let Some(money) = &view.run.money {
                lines.push(format!("money {money}"));
            }
            self.money = view.run.money.clone();
        }
        lines
    }
}

fn millis(ms: i64) -> String {
    if ms >= 60_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Run a `synth` command, returning the process's exit code.
pub async fn run(command: Command) -> Result<i32> {
    match command {
        Command::Run(args) => {
            let api = SynthApi::new(&args.api.url)?;
            let players = args
                .users
                .map_or("synth's configured".to_string(), |u| u.to_string());
            confirm(
                &format!(
                    "Start a {} run with {players} players, paying for their entries from \
                     synth's node",
                    args.kind.replace('_', "-")
                ),
                args.yes,
            )?;
            let started = api.start(&args.kind, args.users).await?;
            if args.api.json && !args.wait {
                println!(
                    "{}",
                    serde_json::json!({ "run_id": started.run_id, "scenario": started.scenario })
                );
                return Ok(0);
            }
            eprintln!("Started {} run {}", started.scenario, started.run_id);
            if !args.wait {
                println!("{}", started.run_id);
                return Ok(0);
            }
            wait(
                &api,
                &started.run_id,
                Duration::from_secs(args.interval),
                args.timeout,
                args.api.json,
            )
            .await
        }
        Command::Runs { api: flags, action } => {
            let api = SynthApi::new(&flags.url)?;
            match action {
                RunsCommand::List { limit } => {
                    let runs = api.runs(limit).await?;
                    if flags.json {
                        println!("{}", serde_json::to_string_pretty(&runs)?);
                    } else {
                        print!("{}", runs_table(&runs));
                    }
                }
                RunsCommand::Show { id } => {
                    let view = api.run(&id).await?;
                    let trail = api.trail(&id).await?;
                    if flags.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "run": view.run,
                                "steps": view.steps,
                                "current_step": view.current_step,
                                "trail": trail,
                            }))?
                        );
                    } else {
                        print!("{}", show_run(&view, &trail));
                    }
                }
            }
            Ok(0)
        }
    }
}

/// Follow a run until it ends and its money settles, printing what changes.
async fn wait(
    api: &SynthApi,
    id: &str,
    interval: Duration,
    timeout: Option<Duration>,
    json: bool,
) -> Result<i32> {
    let began = Instant::now();
    let mut follower = Follower::default();
    let mut failures = 0;
    loop {
        match api.run(id).await {
            Ok(view) => {
                failures = 0;
                for line in follower.changes(&view) {
                    eprintln!("{line}");
                }
                if let Some(outcome) = outcome(&view.run) {
                    let code = outcome.exit_code();
                    if json {
                        let trail = api.trail(id).await.unwrap_or(Value::Null);
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "run": view.run,
                                "steps": view.steps,
                                "trail": trail,
                                "exit_code": code,
                            }))?
                        );
                    }
                    match &outcome {
                        Outcome::Passed => eprintln!("Run {id} passed"),
                        Outcome::Unverified => eprintln!(
                            "Run {id} passed; synth could not verify where its money went"
                        ),
                        Outcome::Failed(error) => eprintln!("Run {id} failed: {error}"),
                        Outcome::Stuck => {
                            eprintln!("Run {id}'s money is stuck; see synth runs show {id}")
                        }
                    }
                    return Ok(code);
                }
            }
            Err(e) => {
                failures += 1;
                eprintln!("warning: {e:#}");
                if failures >= 10 {
                    return Err(e.context("synth did not answer ten times running"));
                }
            }
        }
        if timeout.is_some_and(|timeout| began.elapsed() >= timeout) {
            eprintln!(
                "Gave up waiting for run {id}; it goes on. Follow it with synth runs show {id}"
            );
            return Ok(TIMED_OUT);
        }
        tokio::time::sleep(interval).await;
    }
}

/// The runs as a table, one per line.
pub fn runs_table(runs: &[TestRun]) -> String {
    let mut out = String::new();
    if runs.is_empty() {
        out.push_str("No runs\n");
        return out;
    }
    let _ = writeln!(
        out,
        "{:<36}  {:<15}  {:<11}  {:<13}  {:<25}  ERROR",
        "ID", "SCENARIO", "STATUS", "MONEY", "STARTED"
    );
    for run in runs {
        let _ = writeln!(
            out,
            "{:<36}  {:<15}  {:<11}  {:<13}  {:<25}  {}",
            run.id,
            run.scenario,
            run.status,
            run.money.as_deref().unwrap_or("-"),
            run.started_at,
            run.error_message.as_deref().unwrap_or(""),
        );
    }
    out
}

/// The ledger's lines, in the order money moves.
const LEDGER: &[(&str, &str)] = &[
    ("entries_paid", "Entries paid"),
    ("paid_in", "Paid in"),
    ("escrowed", "Escrowed"),
    ("swap_fees", "Swap fees"),
    ("pot", "Pot"),
    ("coordinator_fee", "Coordinator fee"),
    ("owed", "Owed to winners"),
    ("paid_out", "Paid out"),
    ("confirmed_payouts", "Confirmed payouts"),
    ("unpaid", "Unpaid"),
    ("rounding", "Rounding"),
    ("refunded", "Refunded"),
    ("refund_fees", "Refund fees"),
    ("entry_routing_fee_msat", "Entry routing fee (msat)"),
    ("funding_batch_fee", "Funding batch fee"),
    ("outcome_fee", "Outcome fee"),
];

/// One run: its steps, then its money trail.
pub fn show_run(view: &RunView, trail: &Value) -> String {
    let run = &view.run;
    let mut out = String::new();
    let _ = writeln!(out, "Run           {}", run.id);
    let _ = writeln!(out, "Scenario      {}", run.scenario);
    let _ = writeln!(out, "Status        {}", run.status);
    let _ = writeln!(out, "Started       {}", run.started_at);
    if let Some(at) = &run.completed_at {
        let _ = writeln!(out, "Completed     {at}");
    }
    if let Some(id) = &run.competition_id {
        let _ = writeln!(out, "Competition   {id}");
    }
    if let Some(error) = &run.error_message {
        let _ = writeln!(out, "Error         {error}");
    }

    let _ = writeln!(out, "\nSteps");
    for step in &view.steps {
        let _ = writeln!(
            out,
            "  {:<32}  {:<11}  {:>8}  {}",
            step.step_name,
            step.status,
            step.duration_ms.map(millis).unwrap_or_default(),
            step.error_message.as_deref().unwrap_or(""),
        );
    }
    if let Some(step) = &view.current_step {
        if !view.steps.iter().any(|s| &s.step_name == step) {
            let _ = writeln!(out, "  {step:<32}  running");
        }
    }

    let money = &trail["money"];
    let _ = writeln!(out, "\nMoney");
    match money.get("status").and_then(Value::as_str) {
        Some(status) => {
            let _ = write!(out, "  {status}");
            if let Some(reason) = money.get("reason").and_then(Value::as_str) {
                let _ = write!(out, ": {reason}");
            }
            let _ = writeln!(out);
        }
        None => {
            let _ = writeln!(out, "  not followed yet");
        }
    }
    if let Some(ledger) = trail.get("ledger").filter(|l| l.is_object()) {
        for (key, label) in LEDGER {
            if let Some(value) = ledger.get(*key).filter(|v| !v.is_null()) {
                let _ = writeln!(out, "  {label:<26}  {value}");
            }
        }
    }
    let hops = trail.get("hops").and_then(Value::as_array);
    if let Some(hops) = hops.filter(|hops| !hops.is_empty()) {
        let _ = writeln!(out, "\nMoney trail");
        let text = |hop: &Value, key: &str| {
            hop.get(key)
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    v => v.to_string(),
                })
                .unwrap_or_default()
        };
        for hop in hops {
            let _ = writeln!(
                out,
                "  {:<28}  {:<8}  {:<18} -> {:<18}  {:>9}  {:>9}  {}",
                text(hop, "step"),
                text(hop, "status"),
                text(hop, "from"),
                text(hop, "to"),
                text(hop, "amount_sats"),
                text(hop, "fee_sats"),
                text(hop, "id"),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str, competition: Option<&str>, money: Option<&str>) -> TestRun {
        TestRun {
            id: "run-1".into(),
            scenario: "full_lifecycle".into(),
            status: status.into(),
            started_at: "2030-01-01T00:00:00Z".into(),
            completed_at: None,
            error_message: (status == "failed").then(|| "the oracle never attested".into()),
            config_json: None,
            competition_id: competition.map(String::from),
            money: money.map(String::from),
        }
    }

    fn step(id: &str, name: &str, status: &str) -> TestStep {
        TestStep {
            id: id.into(),
            run_id: "run-1".into(),
            step_name: name.into(),
            status: status.into(),
            started_at: "2030-01-01T00:00:00Z".into(),
            completed_at: None,
            duration_ms: (status != "running").then_some(1500),
            details_json: None,
            error_message: None,
        }
    }

    #[test]
    fn serving_still_takes_a_config_file() {
        let cli = Cli::try_parse_from(["synth", "synth.toml"]).unwrap();
        assert_eq!(cli.config.as_deref(), Some("synth.toml"));
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from(["synth"]).unwrap();
        assert!(cli.config.is_none() && cli.command.is_none());
    }

    #[test]
    fn run_takes_either_spelling_of_a_known_kind() {
        let cli = Cli::try_parse_from([
            "synth",
            "run",
            "escrow-refund",
            "--wait",
            "--timeout",
            "2h",
            "--yes",
            "--url",
            "http://synth.example:9980",
        ])
        .unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.kind, "escrow_refund");
        assert!(args.wait && args.yes);
        assert_eq!(args.timeout, Some(Duration::from_secs(7200)));
        assert_eq!(args.api.url, "http://synth.example:9980");
        assert!(Cli::try_parse_from(["synth", "run", "full_lifecycle"]).is_ok());
        assert!(Cli::try_parse_from(["synth", "run", "sideways"]).is_err());
        assert!(Cli::try_parse_from(["synth", "run"]).is_err());
        assert!(
            Cli::try_parse_from(["synth", "run", "full-lifecycle", "--interval", "0"]).is_err()
        );
    }

    #[test]
    fn runs_flags_go_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "synth", "runs", "show", "abc", "--json", "--url", "http://x",
        ])
        .unwrap();
        let Some(Command::Runs { api, action }) = cli.command else {
            panic!("expected runs");
        };
        assert!(api.json);
        assert_eq!(api.url, "http://x");
        assert!(matches!(action, RunsCommand::Show { id } if id == "abc"));
    }

    #[test]
    fn a_run_ends_when_its_steps_and_its_money_do() {
        assert_eq!(outcome(&run("running", None, None)), None);
        assert_eq!(outcome(&run("passed", Some("c"), None)), None);
        assert_eq!(outcome(&run("passed", Some("c"), Some("following"))), None);
        assert_eq!(
            outcome(&run("passed", Some("c"), Some("paid_out"))),
            Some(Outcome::Passed)
        );
        assert_eq!(
            outcome(&run("passed", Some("c"), Some("unverified"))).map(|o| o.exit_code()),
            Some(0)
        );
        assert_eq!(outcome(&run("passed", None, None)), Some(Outcome::Passed));
        assert_eq!(
            outcome(&run("failed", Some("c"), Some("following"))).map(|o| o.exit_code()),
            Some(1)
        );
        assert_eq!(
            outcome(&run("interrupted", None, None)).map(|o| o.exit_code()),
            Some(1)
        );
        // Stuck money wins over a run marked passed or failed.
        assert_eq!(
            outcome(&run("passed", Some("c"), Some("stuck"))),
            Some(Outcome::Stuck)
        );
        assert_eq!(Outcome::Stuck.exit_code(), 3);
    }

    #[test]
    fn following_prints_only_what_changed() {
        let mut follower = Follower::default();
        let mut view = RunView {
            run: run("running", None, None),
            steps: vec![step("s1", "create_competition", "passed")],
            current_step: Some("enter_players".into()),
        };
        let first = follower.changes(&view);
        assert_eq!(first.len(), 2, "{first:?}");
        assert!(first[0].contains("create_competition") && first[0].contains("passed"));
        assert!(first[1].contains("enter_players") && first[1].contains("running"));
        assert!(follower.changes(&view).is_empty());

        view.steps.push(step("s2", "enter_players", "failed"));
        view.current_step = None;
        view.run = run("failed", Some("c"), Some("following"));
        let next = follower.changes(&view);
        assert_eq!(
            next,
            [
                format!("step {:<32} failed (1.5s)", "enter_players"),
                "run failed".to_string(),
                "money following".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn the_client_starts_follows_and_shows_runs_over_synths_api() {
        use axum::{
            extract::{Path, Query},
            routing::{get, post},
            Json, Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let looks = Arc::new(AtomicUsize::new(0));
        let counter = looks.clone();
        let router = Router::new()
            .route(
                "/api/run",
                post(|Query(q): Query<HashMap<String, String>>| async move {
                    assert_eq!(q["scenario"], "escrow_refund");
                    assert_eq!(q["users"], "2");
                    Json(serde_json::json!({
                        "status": "started", "scenario": "escrow_refund", "run_id": "run-1"
                    }))
                }),
            )
            .route(
                "/api/runs/{id}",
                get(move |Path(id): Path<String>| {
                    let counter = counter.clone();
                    async move {
                        assert_eq!(id, "run-1");
                        // Running on the first look, refunded on the second.
                        let (status, money) = match counter.fetch_add(1, Ordering::SeqCst) {
                            0 => ("running", Value::Null),
                            _ => ("passed", Value::from("refunded")),
                        };
                        Json(serde_json::json!({
                            "run": {
                                "id": "run-1", "scenario": "escrow_refund", "status": status,
                                "started_at": "2030-01-01T00:00:00Z", "completed_at": null,
                                "error_message": null, "config_json": null,
                                "competition_id": "c-1", "money": money
                            },
                            "steps": [],
                            "current_step": null
                        }))
                    }
                }),
            )
            .route(
                "/api/history",
                get(|| async {
                    Json(serde_json::json!({ "runs": [{
                        "id": "run-1", "scenario": "escrow_refund", "status": "passed",
                        "started_at": "2030-01-01T00:00:00Z", "completed_at": null,
                        "error_message": null, "config_json": null,
                        "competition_id": "c-1", "money": "refunded"
                    }]}))
                }),
            )
            .route(
                "/runs/{id}/trail.json",
                get(|| async {
                    Json(serde_json::json!({
                        "run_id": "run-1",
                        "money": { "status": "refunded" },
                        "ledger": { "entries_paid": 2, "paid_in": 2200, "refunded": 2100 },
                        "hops": [{
                            "step": "Refund", "status": "done", "from": "escrow",
                            "to": "player", "amount_sats": 1050, "fee_sats": "0.5",
                            "id": "abcd"
                        }]
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let api = SynthApi::new(&url).unwrap();
        let started = api.start("escrow_refund", Some(2)).await.unwrap();
        assert_eq!(started.run_id, "run-1");

        let code = wait(&api, "run-1", Duration::from_millis(10), None, false)
            .await
            .unwrap();
        assert_eq!(code, 0);
        assert_eq!(looks.load(Ordering::SeqCst), 2);

        let runs = api.runs(5).await.unwrap();
        assert!(runs_table(&runs).contains("refunded"));
        let view = api.run("run-1").await.unwrap();
        let trail = api.trail("run-1").await.unwrap();
        let shown = show_run(&view, &trail);
        assert!(
            shown.contains(&format!("  {:<26}  2100", "Refunded")),
            "{shown}"
        );
        assert!(
            shown.contains("escrow") && shown.contains("abcd"),
            "{shown}"
        );

        server.abort();
        let _ = server.await;
    }
}
