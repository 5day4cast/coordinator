//! `synth run` and `synth runs`: start and follow runs on a running synth, as
//! a client of its HTTP API. Nothing here runs a scenario itself.

use crate::db::{TestRun, TestStep};
use crate::runner::SCENARIOS;
use crate::scenarios::manual::MANUAL_COMPETITION;
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
    Run(Box<RunArgs>),
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
    /// The synth to talk to. Read access is restricted by the operator network;
    /// writes additionally require --operator-token-file.
    #[arg(
        long,
        global = true,
        env = "SYNTH_URL",
        default_value = "http://127.0.0.1:9980"
    )]
    pub url: String,

    /// File with the bearer token configured at server.operator_token_file.
    #[arg(long, global = true, env = "SYNTH_OPERATOR_TOKEN_FILE")]
    pub operator_token_file: Option<std::path::PathBuf>,

    /// Print JSON instead of tables.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Scenario name (hyphens or underscores); see the supported cases in --help errors.
    #[arg(value_parser = parse_kind)]
    pub kind: String,
    /// Synthetic players; synth's configured number if unset.
    #[arg(long)]
    pub users: Option<usize>,
    #[command(flatten)]
    pub timing: EntryTimingArgs,
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
    pub competition: CompetitionArgs,
    #[command(flatten)]
    pub api: ApiArgs,
}

/// What `manual-competition` creates: the dashboard's "Create a competition" form, from the
/// command line. Synth checks it as the form is checked.
#[derive(Debug, Clone, Default, Args)]
pub struct CompetitionArgs {
    /// manual-competition: its stations, separated by commas.
    #[arg(long, value_delimiter = ',')]
    pub stations: Vec<String>,
    /// manual-competition: how long it takes entries, such as 90m or 1h; an hour if unset.
    #[arg(long, value_parser = parse_duration)]
    pub entry_window: Option<Duration>,
    /// manual-competition: its observation window, one of synth's configured ones, such as 1d
    /// or 12h; the first the oracle attests if unset.
    #[arg(long, value_parser = parse_duration)]
    pub window: Option<Duration>,
    /// manual-competition: synth players who enter it over its entry window. None leaves it
    /// open for people.
    #[arg(long)]
    pub players: Option<usize>,
    /// manual-competition: the entry fee in sats; synth's configured one if unset.
    #[arg(long)]
    pub entry_fee: Option<u64>,
    /// manual-competition: its seats; the pool cap if unset.
    #[arg(long)]
    pub seats: Option<usize>,
    /// manual-competition: put it on the oracle's public list.
    #[arg(long)]
    pub listed: bool,
    /// manual-competition: picks each entry makes; every station and metric the window scores
    /// if unset.
    #[arg(long)]
    pub picks: Option<usize>,
}

impl CompetitionArgs {
    fn is_set(&self) -> bool {
        !self.stations.is_empty()
            || self.entry_window.is_some()
            || self.window.is_some()
            || self.players.is_some()
            || self.entry_fee.is_some()
            || self.seats.is_some()
            || self.listed
            || self.picks.is_some()
    }

    /// The form's fields, as the dashboard sends them.
    fn form(&self) -> Vec<(&'static str, String)> {
        let mut fields: Vec<(&'static str, String)> = self
            .stations
            .iter()
            .map(|station| ("stations", station.clone()))
            .collect();
        let optional = [
            ("entry_window_secs", self.entry_window.map(|d| d.as_secs())),
            ("window_secs", self.window.map(|d| d.as_secs())),
            ("players", self.players.map(|n| n as u64)),
            ("entry_fee", self.entry_fee),
            ("seats", self.seats.map(|n| n as u64)),
            ("picks", self.picks.map(|n| n as u64)),
        ];
        fields.extend(
            optional
                .into_iter()
                .filter_map(|(key, value)| Some((key, value?.to_string()))),
        );
        if self.listed {
            fields.push(("listed", "listed".into()));
        }
        fields
    }
}

/// Optional per-run overrides shared by the remote and direct operator CLIs.
#[derive(Debug, Clone, Default, Args)]
pub struct EntryTimingArgs {
    /// Reproduce this run's randomized timing and picks.
    #[arg(long)]
    pub seed: Option<u64>,
    #[arg(long)]
    pub entry_window_secs: Option<u64>,
    /// Fix the observation window when replaying a recorded run.
    #[arg(long)]
    pub observation_window_secs: Option<u64>,
    #[arg(long)]
    pub arrival_min_secs: Option<u64>,
    #[arg(long)]
    pub arrival_max_secs: Option<u64>,
    #[arg(long)]
    pub before_payment_min_secs: Option<u64>,
    #[arg(long)]
    pub before_payment_max_secs: Option<u64>,
    #[arg(long)]
    pub before_submit_min_secs: Option<u64>,
    #[arg(long)]
    pub before_submit_max_secs: Option<u64>,
    #[arg(long)]
    pub deadline_margin_secs: Option<u64>,
    /// Players who enter a queued scenario completely, instead of its own number (27 for
    /// queued-split).
    #[arg(long)]
    pub queue_players: Option<usize>,
    /// The largest pool of a queued scenario, instead of 25.
    #[arg(long)]
    pub max_pool_players: Option<usize>,
}

impl EntryTimingArgs {
    pub fn apply(&self, config: &mut crate::scenarios::ScenarioConfig) {
        if let Some(window) = self.observation_window_secs {
            config.observation_window_secs = window;
            config.observation_window_choices.clear();
        }
        if let Some(seed) = self.seed {
            config.seed = Some(seed);
        }
        if self.queue_players.is_some() {
            config.queue_players = self.queue_players;
        }
        if self.max_pool_players.is_some() {
            config.max_pool_players = self.max_pool_players;
        }
        for (target, value) in [
            (&mut config.entry_window_secs, self.entry_window_secs),
            (
                &mut config.entry_timing.arrival.min_secs,
                self.arrival_min_secs,
            ),
            (
                &mut config.entry_timing.arrival.max_secs,
                self.arrival_max_secs,
            ),
            (
                &mut config.entry_timing.before_payment.min_secs,
                self.before_payment_min_secs,
            ),
            (
                &mut config.entry_timing.before_payment.max_secs,
                self.before_payment_max_secs,
            ),
            (
                &mut config.entry_timing.before_submit.min_secs,
                self.before_submit_min_secs,
            ),
            (
                &mut config.entry_timing.before_submit.max_secs,
                self.before_submit_max_secs,
            ),
            (
                &mut config.entry_timing.deadline_margin_secs,
                self.deadline_margin_secs,
            ),
        ] {
            if let Some(value) = value {
                *target = value;
            }
        }
    }

    fn append_query(&self, path: &mut String) {
        for (key, value) in [
            ("seed", self.seed),
            ("entry_window_secs", self.entry_window_secs),
            ("observation_window_secs", self.observation_window_secs),
            ("arrival_min_secs", self.arrival_min_secs),
            ("arrival_max_secs", self.arrival_max_secs),
            ("before_payment_min_secs", self.before_payment_min_secs),
            ("before_payment_max_secs", self.before_payment_max_secs),
            ("before_submit_min_secs", self.before_submit_min_secs),
            ("before_submit_max_secs", self.before_submit_max_secs),
            ("deadline_margin_secs", self.deadline_margin_secs),
            ("queue_players", self.queue_players.map(|n| n as u64)),
            ("max_pool_players", self.max_pool_players.map(|n| n as u64)),
        ] {
            if let Some(value) = value {
                let _ = write!(path, "&{key}={value}");
            }
        }
    }
}

/// A scenario name as synth knows it, from either spelling, or `manual-competition`.
fn parse_kind(value: &str) -> Result<String, String> {
    let kind = value.trim().to_ascii_lowercase().replace('-', "_");
    if SCENARIOS.contains(&kind.as_str()) || kind == MANUAL_COMPETITION {
        Ok(kind)
    } else {
        Err(format!(
            "expected one of: {}",
            SCENARIOS
                .iter()
                .chain([&MANUAL_COMPETITION])
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

/// What `/api/run` answers, and `/api/competitions` with the competition it made.
#[derive(Debug, Clone, Deserialize)]
pub struct Started {
    pub run_id: String,
    pub scenario: String,
    #[serde(default)]
    pub competition_id: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
}

/// A client of synth's HTTP API.
pub struct SynthApi {
    http: reqwest::Client,
    url: String,
    operator_token: Option<zeroize::Zeroizing<String>>,
}

impl SynthApi {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(60))
                .build()?,
            url: url.trim_end_matches('/').to_string(),
            operator_token: None,
        })
    }

    pub fn with_operator_token_file(mut self, path: Option<&std::path::Path>) -> Result<Self> {
        if let Some(path) = path {
            let token = zeroize::Zeroizing::new(
                std::fs::read_to_string(path).context("read the synth operator token file")?,
            );
            anyhow::ensure!(!token.trim().is_empty(), "synth operator token is empty");
            self.operator_token = Some(token);
        }
        Ok(self)
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
        self.send(self.http.post(format!("{}{path}", self.url)))
            .await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let request = match self.operator_token.as_ref() {
            Some(token) => request.bearer_auth(token.trim()),
            None => request,
        };
        let response = request
            .send()
            .await
            .with_context(|| format!("reach synth at {}", self.url))?;
        checked(response).await
    }

    pub async fn start(&self, kind: &str, users: Option<usize>) -> Result<Started> {
        self.start_with_options(kind, users, &EntryTimingArgs::default())
            .await
    }

    pub async fn start_with_options(
        &self,
        kind: &str,
        users: Option<usize>,
        timing: &EntryTimingArgs,
    ) -> Result<Started> {
        let mut path = format!("/api/run?scenario={kind}");
        if let Some(users) = users {
            let _ = write!(path, "&users={users}");
        }
        timing.append_query(&mut path);
        let started: Value = self.post(&path).await?.json().await?;
        if started.get("run_id").is_none() {
            bail!("synth started the run but did not say its id; it needs a newer synth");
        }
        Ok(serde_json::from_value(started)?)
    }

    /// Create a competition as the dashboard's form does, recorded as a `manual_competition` run.
    /// Answers once the coordinator has taken it, or has not answered within a minute.
    pub async fn create_competition(&self, competition: &CompetitionArgs) -> Result<Started> {
        let request = self
            .http
            .post(format!(
                "{}{}",
                self.url,
                crate::server::CREATE_COMPETITION_PATH
            ))
            .timeout(Duration::from_secs(90))
            .form(&competition.form());
        Ok(self.send(request).await?.json().await?)
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
    /// Its steps passed; settlement remains unverified while synth continues checking.
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
            // Written off by an operator: not stuck any more, and not where it should be either.
            (_, Some("unverified" | "written_off")) => Some(Outcome::Unverified),
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
            let api = SynthApi::new(&args.api.url)?
                .with_operator_token_file(args.api.operator_token_file.as_deref())?;
            let started = if args.kind == MANUAL_COMPETITION {
                let what = match args.competition.players.unwrap_or(0) {
                    0 => "Create a competition and leave it open for people".to_string(),
                    players => format!(
                        "Create a competition and enter {players} synth players, paying for \
                         their entries from synth's node"
                    ),
                };
                confirm(&what, args.yes)?;
                let started = api.create_competition(&args.competition).await?;
                if let Some(competition) = &started.competition_id {
                    eprintln!(
                        "Created competition {competition}{}",
                        started
                            .link
                            .as_deref()
                            .map(|link| format!(": {link}"))
                            .unwrap_or_default()
                    );
                }
                started
            } else {
                if args.competition.is_set() {
                    bail!(
                        "--stations, --entry-window, --window, --players, --entry-fee, --seats \
                         and --listed are for manual-competition"
                    );
                }
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
                api.start_with_options(&args.kind, args.users, &args.timing)
                    .await?
            };
            if args.api.json && !args.wait {
                println!(
                    "{}",
                    serde_json::json!({
                        "run_id": started.run_id,
                        "scenario": started.scenario,
                        "competition_id": started.competition_id,
                    })
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
            let api = SynthApi::new(&flags.url)?
                .with_operator_token_file(flags.operator_token_file.as_deref())?;
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

    #[test]
    fn new_cases_accept_reproducible_timing_overrides() {
        for scenario in [
            "abandoned-unpaid",
            "paid-abandonment",
            "duplicate-submission",
            "late-submission",
        ] {
            let cli = Cli::try_parse_from([
                "synth",
                "run",
                scenario,
                "--seed",
                "42",
                "--entry-window-secs",
                "1200",
                "--observation-window-secs",
                "10800",
                "--arrival-min-secs",
                "1",
                "--arrival-max-secs",
                "90",
                "--before-payment-min-secs",
                "5",
                "--before-payment-max-secs",
                "60",
                "--before-submit-min-secs",
                "10",
                "--before-submit-max-secs",
                "120",
                "--deadline-margin-secs",
                "60",
                "--yes",
            ])
            .unwrap();
            let Some(Command::Run(args)) = cli.command else {
                panic!("run command expected")
            };
            let mut config = crate::scenarios::ScenarioConfig {
                observation_window_choices: vec![600],
                ..Default::default()
            };
            args.timing.apply(&mut config);
            let planned = config.resolve_plan(&args.kind).unwrap();
            assert_eq!(planned.seed, Some(42));
            assert_eq!(planned.entry_window_secs, 1200);
            assert_eq!(planned.observation_window_secs, 10800);
            assert!(planned.observation_window_choices.is_empty());
            let mut query = String::new();
            args.timing.append_query(&mut query);
            assert!(query.contains("&seed=42"));
            assert!(query.contains("&observation_window_secs=10800"));
            assert!(query.contains("&before_submit_max_secs=120"));
        }
    }

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
    fn manual_competition_takes_the_forms_fields() {
        let cli = Cli::try_parse_from([
            "synth",
            "run",
            "manual-competition",
            "--stations",
            "KDEN,KJFK",
            "--entry-window",
            "1h",
            "--window",
            "1d",
            "--players",
            "5",
            "--listed",
            "--yes",
        ])
        .unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.kind, MANUAL_COMPETITION);
        assert_eq!(
            args.competition.form(),
            [
                ("stations", "KDEN".to_string()),
                ("stations", "KJFK".to_string()),
                ("entry_window_secs", "3600".to_string()),
                ("window_secs", "86400".to_string()),
                ("players", "5".to_string()),
                ("listed", "listed".to_string()),
            ]
        );
        // Without its fields, nothing but the kind.
        let bare = Cli::try_parse_from(["synth", "run", "manual_competition", "--yes"]).unwrap();
        let Some(Command::Run(bare)) = bare.command else {
            panic!("expected run");
        };
        assert!(!bare.competition.is_set());
        assert!(bare.competition.form().is_empty());
        assert!(Cli::try_parse_from(["synth", "run", "stress-full-pool", "--yes"]).is_ok());
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
