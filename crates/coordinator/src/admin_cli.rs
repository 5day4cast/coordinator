//! `coordinator admin`: drive competitions from a terminal or a script, through the operator
//! listener and its bearer token. A client only; everything it does, the operator listener's
//! HTTP API does.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use std::{
    fmt::Write as _,
    io::{BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    api::routes::OperatorCompetition,
    domain::{CoordinatorFee, CreateEvent, CreateQueuedCompetition},
    infra::oracle::ScoringRules,
};

#[derive(Debug, Args)]
pub struct AdminArgs {
    /// The coordinator's operator listener.
    #[arg(
        long,
        global = true,
        env = "COORDINATOR_ADMIN_URL",
        default_value = "http://127.0.0.1:9991"
    )]
    pub url: String,

    /// File holding the operator token, sent as a bearer token. The token is never taken on
    /// the command line, where the process list would show it. Leave unset only for a
    /// listener that allows unauthenticated development access.
    #[arg(long, global = true, env = "COORDINATOR_ADMIN_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,

    /// Print JSON instead of a table.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// List, show, create and cancel competitions.
    Competitions {
        #[command(subcommand)]
        action: CompetitionCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum CompetitionCommand {
    /// List competitions, newest first.
    List {
        /// Only competitions in this state; repeat for several. `active` means any state but
        /// completed, failed and cancelled.
        #[arg(long = "state", value_name = "STATE", value_parser = parse_state_filter)]
        states: Vec<String>,
    },
    /// Show one competition: its terms, how far it has settled, the errors kept on it, and its
    /// escrow refunds.
    Show { id: Uuid },
    /// Create a competition, with the admin page's fields and defaults.
    Create(CreateArgs),
    /// Cancel a competition nobody has paid into, by deleting it, as the admin page's delete
    /// button does. The coordinator refuses once any entry is paid.
    #[command(visible_alias = "delete")]
    Cancel {
        id: Uuid,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Station codes, comma-separated (e.g. KDEN,KJFK).
    #[arg(long, value_delimiter = ',', required = true)]
    pub stations: Vec<String>,
    /// When observation starts: RFC 3339, or relative to now such as +6h, +30m or +1d.
    #[arg(long, default_value = "+6h", value_parser = parse_when)]
    pub start: OffsetDateTime,
    /// When observation ends.
    #[arg(long, default_value = "+24h", value_parser = parse_when)]
    pub end: OffsetDateTime,
    /// When signing must be done by.
    #[arg(long, default_value = "+33h", value_parser = parse_when)]
    pub signing: OffsetDateTime,
    /// Values each entry picks.
    #[arg(long, default_value_t = 1)]
    pub values_per_entry: usize,
    /// Most entries allowed.
    #[arg(long, default_value_t = 3)]
    pub max_entries: usize,
    /// Entry fee in sats.
    #[arg(long, default_value_t = 5000)]
    pub entry_fee: usize,
    /// The coordinator's fee on each entry, in percent with up to two decimals (2.5).
    #[arg(long, default_value = "5", value_parser = parse_coordinator_fee)]
    pub coordinator_fee_percentage: CoordinatorFee,
    /// How many places win.
    #[arg(long, default_value_t = 1)]
    pub places_win: usize,
    /// Blocks between the outcome and delta transactions; the coordinator's default if unset.
    #[arg(long)]
    pub locktime_delta: Option<u16>,
    /// Keep the event off the oracle's public events list.
    #[arg(long)]
    pub unlisted: bool,
    /// How picks score: `lines` (a Par band per station fitted on recent forecast misses,
    /// 10 points a right pick) or `fixed` (exact Par 20, Over or Under 10).
    #[arg(long, default_value = "lines", value_parser = parse_scoring_rules)]
    pub scoring_rules: ScoringRules,
    /// Queue entries without a seat count, and split them into pools when observation starts.
    /// Each pool pays one winner and scores lines, so --max-entries, --places-win and
    /// --scoring-rules do not apply.
    #[arg(long)]
    pub queued: bool,
    /// A queued competition's smallest pool.
    #[arg(long, default_value_t = crate::domain::DEFAULT_MIN_PLAYERS)]
    pub min_players: usize,
    /// A queued competition's largest pool.
    #[arg(long, default_value_t = coordinator_escrow::pools::MAX_POOL_PLAYERS)]
    pub max_pool_size: usize,
    /// A queued competition's entry cap; the coordinator's default if unset.
    #[arg(long)]
    pub entry_cap: Option<u32>,
    /// The competition's id; a new one if unset.
    #[arg(long)]
    pub id: Option<Uuid>,
    /// Print what would be sent, and send nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// The states a competition can be in, as the coordinator names them.
pub const STATES: &[&str] = &[
    "created",
    "entries_collected",
    "escrow_funds_confirmed",
    "event_created",
    "entries_submitted",
    "contract_created",
    "awaiting_signatures",
    "signing_complete",
    "funding_broadcasted",
    "funding_confirmed",
    "funding_settled",
    "awaiting_attestation",
    "attested",
    "expiry_broadcasted",
    "outcome_broadcasted",
    "delta_broadcasted",
    "completed",
    "failed",
    "cancelled",
    "pools_formed",
];

const FINISHED: &[&str] = &["completed", "failed", "cancelled"];

fn parse_state_filter(value: &str) -> Result<String, String> {
    let value = value.trim().to_ascii_lowercase().replace('-', "_");
    if value == "active" || STATES.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(format!("expected active or one of: {}", STATES.join(", ")))
    }
}

/// Whether `state` passes the `--state` filters; no filters pass everything.
pub fn state_matches(filters: &[String], state: &str) -> bool {
    filters.is_empty()
        || filters
            .iter()
            .any(|filter| filter == state || (filter == "active" && !FINISHED.contains(&state)))
}

/// An RFC 3339 time, or one relative to now: `+90s`, `+30m`, `+6h`, `+2d`.
pub fn parse_when(value: &str) -> Result<OffsetDateTime, String> {
    parse_when_at(value, OffsetDateTime::now_utc())
}

fn parse_when_at(value: &str, now: OffsetDateTime) -> Result<OffsetDateTime, String> {
    if let Some(relative) = value.strip_prefix('+') {
        let split = relative
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(relative.len());
        let (amount, unit) = relative.split_at(split);
        let amount: i64 = amount
            .parse()
            .map_err(|_| format!("{value}: expected a number after +"))?;
        let seconds = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => return Err(format!("{value}: the unit must be s, m, h or d")),
        };
        return Ok(now + time::Duration::seconds(amount * seconds));
    }
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|at| at.to_offset(time::UtcOffset::UTC))
        .map_err(|e| format!("{value}: expected RFC 3339 or +<n>[smhd]: {e}"))
}

impl CreateArgs {
    /// The request the operator listener's create endpoint takes.
    pub fn to_event(&self) -> Result<CreateEvent> {
        let locations: Vec<String> = self
            .stations
            .iter()
            .map(|station| station.trim().to_ascii_uppercase())
            .filter(|station| !station.is_empty())
            .collect();
        if locations.is_empty() {
            bail!("give at least one station");
        }
        let total_competition_pool = self
            .entry_fee
            .checked_mul(self.max_entries)
            .ok_or_else(|| anyhow!("the pool (entry fee x max entries) is too large"))?;
        Ok(CreateEvent {
            id: self.id.unwrap_or_else(Uuid::now_v7),
            signing_date: self.signing,
            start_observation_date: self.start,
            end_observation_date: self.end,
            locations,
            number_of_values_per_entry: self.values_per_entry,
            number_of_places_win: self.places_win,
            total_allowed_entries: self.max_entries,
            entry_fee: self.entry_fee,
            coordinator_fee: self.coordinator_fee_percentage,
            total_competition_pool,
            relative_locktime_block_delta: self.locktime_delta,
            unlisted: self.unlisted,
            scoring_rules: Some(self.scoring_rules),
            scoring_fields: None,
        })
    }
}

impl CreateArgs {
    /// The request the operator listener's queued create endpoint takes.
    pub fn to_queued(&self) -> Result<CreateQueuedCompetition> {
        let event = self.to_event()?;
        Ok(CreateQueuedCompetition {
            id: event.id,
            signing_date: event.signing_date,
            start_observation_date: event.start_observation_date,
            end_observation_date: event.end_observation_date,
            locations: event.locations,
            number_of_values_per_entry: event.number_of_values_per_entry,
            entry_fee: event.entry_fee,
            coordinator_fee: event.coordinator_fee,
            relative_locktime_block_delta: event.relative_locktime_block_delta,
            min_players: self.min_players,
            max_pool_size: self.max_pool_size,
            max_entries: self.entry_cap,
        })
    }
}

fn parse_scoring_rules(text: &str) -> Result<ScoringRules, String> {
    ScoringRules::parse(text).ok_or_else(|| format!("{text:?} is not `lines` or `fixed`"))
}

fn parse_coordinator_fee(text: &str) -> Result<CoordinatorFee, String> {
    CoordinatorFee::parse_percent(text).map_err(|error| error.to_string())
}

/// A client of the operator listener.
pub struct AdminClient {
    http: reqwest::Client,
    url: String,
    token: Option<Zeroizing<String>>,
}

impl AdminClient {
    pub fn new(url: &str, token: Option<Zeroizing<String>>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            url: url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// A client carrying the token read from `path`, surrounding whitespace ignored.
    pub fn with_token_file(url: &str, path: Option<&Path>) -> Result<Self> {
        let token = match path {
            Some(path) => {
                let contents = Zeroizing::new(
                    std::fs::read_to_string(path)
                        .with_context(|| format!("read the token file {}", path.display()))?,
                );
                let token = contents.trim();
                if token.is_empty() {
                    bail!("the token file {} is empty", path.display());
                }
                Some(Zeroizing::new(token.to_owned()))
            }
            None => None,
        };
        Self::new(url, token)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self.http.request(method, format!("{}{path}", self.url));
        match &self.token {
            Some(token) => request.bearer_auth(token.as_str()),
            None => request,
        }
    }

    pub async fn competitions(&self) -> Result<Vec<OperatorCompetition>> {
        let response = self
            .request(reqwest::Method::GET, "/api/v1/admin/competitions")
            .send()
            .await
            .context("reach the operator listener")?;
        Ok(checked(response).await?.json().await?)
    }

    pub async fn competition(&self, id: Uuid) -> Result<OperatorCompetition> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/api/v1/admin/competitions/{id}"),
            )
            .send()
            .await
            .context("reach the operator listener")?;
        Ok(checked(response).await?.json().await?)
    }

    /// Create a competition, returning its id.
    pub async fn create(&self, event: &CreateEvent) -> Result<Uuid> {
        self.post_create("/api/v1/competitions", event).await
    }

    /// Create a queued competition, returning its id.
    pub async fn create_queued(&self, request: &CreateQueuedCompetition) -> Result<Uuid> {
        self.post_create("/api/v1/competitions/queued", request)
            .await
    }

    async fn post_create<T: serde::Serialize>(&self, path: &str, body: &T) -> Result<Uuid> {
        #[derive(serde::Deserialize)]
        struct Created {
            id: Uuid,
        }
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await
            .context("reach the operator listener")?;
        Ok(checked(response).await?.json::<Created>().await?.id)
    }

    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/api/v1/admin/competitions/{id}"),
            )
            .send()
            .await
            .context("reach the operator listener")?;
        checked(response).await?;
        Ok(())
    }
}

/// The response, or an error saying what the coordinator said was wrong.
async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!("the operator listener rejected the token (401); check --token-file");
    }
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| json.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or(body);
    bail!("the coordinator answered {status}: {message}")
}

/// Ask on the terminal before doing `what`, unless `yes`. Without a terminal, refuse: a script
/// must say --yes.
pub fn confirm(what: &str, yes: bool) -> Result<()> {
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

/// Run `coordinator admin …`.
pub async fn run(args: AdminArgs) -> Result<()> {
    let client = AdminClient::with_token_file(&args.url, args.token_file.as_deref())?;
    let json = args.json;
    match args.command {
        AdminCommand::Competitions { action } => match action {
            CompetitionCommand::List { states } => {
                let competitions: Vec<_> = client
                    .competitions()
                    .await?
                    .into_iter()
                    .filter(|c| state_matches(&states, &c.state))
                    .collect();
                if json {
                    println!("{}", serde_json::to_string_pretty(&competitions)?);
                } else {
                    print!("{}", list_table(&competitions));
                }
            }
            CompetitionCommand::Show { id } => {
                let competition = client.competition(id).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&competition)?);
                } else {
                    print!("{}", show_text(&competition));
                }
            }
            CompetitionCommand::Create(create) => {
                let id = if create.queued {
                    let request = create.to_queued()?;
                    if create.dry_run {
                        println!("{}", serde_json::to_string_pretty(&request)?);
                        return Ok(());
                    }
                    client.create_queued(&request).await?
                } else {
                    let event = create.to_event()?;
                    if create.dry_run {
                        println!("{}", serde_json::to_string_pretty(&event)?);
                        return Ok(());
                    }
                    client.create(&event).await?
                };
                let competition = client.competition(id).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&competition)?);
                } else {
                    println!("Created competition {id}\n");
                    print!("{}", show_text(&competition));
                }
            }
            CompetitionCommand::Cancel { id, yes } => {
                let competition = client.competition(id).await?;
                if competition.total_paid_entries > 0 {
                    bail!(
                        "competition {id} has {} paid entries, so it cannot be deleted; the \
                         coordinator cancels and refunds it itself if it fails or never fills",
                        competition.total_paid_entries
                    );
                }
                confirm(
                    &format!(
                        "Delete competition {id} ({}, {} entries, none paid)",
                        competition.state, competition.total_entries
                    ),
                    yes,
                )?;
                client.delete(id).await?;
                if json {
                    println!("{}", serde_json::json!({ "deleted": id }));
                } else {
                    println!("Deleted competition {id}");
                }
            }
        },
    }
    Ok(())
}

fn when(at: OffsetDateTime) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]Z");
    at.format(&format).unwrap_or_else(|_| at.to_string())
}

fn refunds(competition: &OperatorCompetition) -> String {
    match competition.refunds {
        Some(progress) => format!("{}/{}", progress.refunded, progress.escrowed),
        None => "-".to_string(),
    }
}

/// The competitions as a table, one per line.
pub fn list_table(competitions: &[OperatorCompetition]) -> String {
    let mut out = String::new();
    if competitions.is_empty() {
        out.push_str("No competitions\n");
        return out;
    }
    let _ = writeln!(
        out,
        "{:<36}  {:<22}  {:>9}  {:>7}  {:<17}  {:<17}  {:>7}  {:>6}",
        "ID", "STATE", "PAID/MAX", "FEE", "START", "END", "REFUNDS", "ERRORS"
    );
    for c in competitions {
        let _ = writeln!(
            out,
            "{:<36}  {:<22}  {:>9}  {:>7}  {:<17}  {:<17}  {:>7}  {:>6}",
            c.id,
            c.state,
            format!(
                "{}/{}",
                c.total_paid_entries, c.event_submission.total_allowed_entries
            ),
            c.event_submission.entry_fee,
            when(c.event_submission.start_observation_date),
            when(c.event_submission.end_observation_date),
            refunds(c),
            c.errors.len(),
        );
    }
    out
}

/// How many of a competition's kept errors `show` prints.
const SHOWN_ERRORS: usize = 5;

/// One competition, as labelled lines.
pub fn show_text(c: &OperatorCompetition) -> String {
    let terms = &c.event_submission;
    let mut out = String::new();
    let _ = writeln!(out, "Competition   {}", c.id);
    let _ = writeln!(out, "State         {}", c.state);
    let _ = writeln!(out, "Stations      {}", terms.locations.join(", "));
    let _ = writeln!(
        out,
        "Observation   {} to {}",
        when(terms.start_observation_date),
        when(terms.end_observation_date)
    );
    let _ = writeln!(out, "Signing by    {}", when(terms.signing_date));
    if let Some(queue) = &c.queue {
        let _ = writeln!(
            out,
            "Queue         {} entered of at most {}; pools of {} to {} players",
            queue.entries,
            queue.max_entries,
            queue.pool_rules.min_players(),
            queue.pool_rules.max_players()
        );
        for pool in &queue.pools {
            let _ = writeln!(
                out,
                "Pool {:<8} {} ({} players)",
                pool.pool_index, pool.competition_id, pool.players
            );
        }
    } else {
        let _ = writeln!(
            out,
            "Entries       {} of {} ({} paid, {} paid out)",
            c.total_entries,
            terms.total_allowed_entries,
            c.total_paid_entries,
            c.total_paid_out_entries
        );
    }
    if let Some(parent) = c.parent_id {
        let _ = writeln!(out, "Queued in     {parent}");
    }
    let _ = writeln!(
        out,
        "Entry fee     {} sats; pool {} sats; coordinator fee {}; {} place(s) win",
        terms.entry_fee,
        terms.total_competition_pool,
        terms.coordinator_fee,
        terms.number_of_places_win
    );
    let _ = writeln!(
        out,
        "Listed        {}",
        if terms.unlisted { "no" } else { "yes" }
    );
    let _ = writeln!(out, "Scoring       {}", terms.scoring_rules().as_str());
    let refunds = match c.refunds {
        Some(p) if p.refunded >= p.escrowed => format!("all {} escrows refunded", p.escrowed),
        Some(p) => format!(
            "{} of {} escrows refunded; {} outstanding",
            p.refunded,
            p.escrowed,
            p.escrowed - p.refunded
        ),
        None => "no funded escrows to refund".to_string(),
    };
    let _ = writeln!(out, "Refunds       {refunds}");
    let _ = writeln!(out, "\nSettlement");
    for milestone in &c.milestones {
        let _ = writeln!(out, "  {:<26}  {}", milestone.name, when(milestone.at));
    }
    if c.errors.is_empty() {
        let _ = writeln!(out, "\nErrors        none");
    } else {
        let shown = c.errors.len().min(SHOWN_ERRORS);
        let _ = writeln!(out, "\nErrors (last {shown} of {})", c.errors.len());
        for error in &c.errors[c.errors.len() - shown..] {
            let _ = writeln!(out, "  {error}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Cli, Command};
    use clap::Parser;

    fn parse(args: &[&str]) -> AdminArgs {
        let mut argv = vec!["coordinator", "admin"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Admin(admin)) => admin,
            other => panic!("expected the admin command, got {other:?}"),
        }
    }

    #[test]
    fn the_server_still_starts_without_a_command() {
        let cli = Cli::try_parse_from(["coordinator", "--config", "Settings.toml"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.config.as_deref(), Some("Settings.toml"));
    }

    #[test]
    fn global_flags_go_anywhere_and_states_are_checked() {
        let args = parse(&[
            "competitions",
            "list",
            "--state",
            "awaiting-attestation",
            "--state",
            "active",
            "--json",
            "--url",
            "http://operator.example:9991",
            "--token-file",
            "/run/secrets/token",
        ]);
        assert!(args.json);
        assert_eq!(args.url, "http://operator.example:9991");
        assert_eq!(
            args.token_file.unwrap(),
            PathBuf::from("/run/secrets/token")
        );
        match args.command {
            AdminCommand::Competitions {
                action: CompetitionCommand::List { states },
            } => assert_eq!(states, ["awaiting_attestation", "active"]),
            other => panic!("{other:?}"),
        }
        let unknown = Cli::try_parse_from([
            "coordinator",
            "admin",
            "competitions",
            "list",
            "--state",
            "sideways",
        ]);
        assert!(unknown.is_err());
    }

    #[test]
    fn the_token_is_never_a_command_line_value() {
        for flag in ["--token", "--admin-token", "--bearer"] {
            let parsed = Cli::try_parse_from([
                "coordinator",
                "admin",
                flag,
                "secret",
                "competitions",
                "list",
            ]);
            assert!(parsed.is_err(), "{flag} was accepted");
        }
    }

    #[test]
    fn cancel_needs_an_id_and_takes_yes() {
        let args = parse(&["competitions", "delete", &Uuid::nil().to_string(), "--yes"]);
        match args.command {
            AdminCommand::Competitions {
                action: CompetitionCommand::Cancel { id, yes },
            } => {
                assert_eq!(id, Uuid::nil());
                assert!(yes);
            }
            other => panic!("{other:?}"),
        }
        assert!(Cli::try_parse_from(["coordinator", "admin", "competitions", "cancel"]).is_err());
    }

    #[test]
    fn create_uses_the_admin_forms_defaults() {
        let args = parse(&[
            "competitions",
            "create",
            "--stations",
            "kden, KJFK",
            "--unlisted",
        ]);
        let AdminCommand::Competitions {
            action: CompetitionCommand::Create(create),
        } = args.command
        else {
            panic!("expected create");
        };
        let event = create.to_event().unwrap();
        assert_eq!(event.locations, ["KDEN", "KJFK"]);
        assert_eq!(event.entry_fee, 5000);
        assert_eq!(event.total_allowed_entries, 3);
        assert_eq!(event.total_competition_pool, 15000);
        assert_eq!(event.coordinator_fee.basis_points(), 500);
        assert_eq!(event.number_of_values_per_entry, 1);
        assert_eq!(event.number_of_places_win, 1);
        assert!(event.unlisted);
        assert!(event.start_observation_date < event.end_observation_date);
        assert!(event.end_observation_date < event.signing_date);
        assert!(Cli::try_parse_from(["coordinator", "admin", "competitions", "create"]).is_err());
    }

    #[test]
    fn create_takes_a_decimal_coordinator_fee() {
        let fee = |fee: &str| {
            let args = parse(&[
                "competitions",
                "create",
                "--stations",
                "KDEN",
                "--coordinator-fee-percentage",
                fee,
            ]);
            let AdminCommand::Competitions {
                action: CompetitionCommand::Create(create),
            } = args.command
            else {
                panic!("expected create");
            };
            create.to_event().unwrap().coordinator_fee.basis_points()
        };
        assert_eq!(fee("2.5"), 250);
        assert_eq!(fee("3"), 300);
        for bad in ["2.555", "101", "-1"] {
            assert!(Cli::try_parse_from([
                "coordinator",
                "admin",
                "competitions",
                "create",
                "--stations",
                "KDEN",
                "--coordinator-fee-percentage",
                bad,
            ])
            .is_err());
        }
    }

    #[test]
    fn times_are_rfc3339_or_relative_to_now() {
        let now = OffsetDateTime::parse("2030-01-01T00:00:00Z", &Rfc3339).unwrap();
        assert_eq!(
            parse_when_at("+6h", now).unwrap(),
            now + time::Duration::hours(6)
        );
        assert_eq!(
            parse_when_at("+2d", now).unwrap(),
            now + time::Duration::days(2)
        );
        assert_eq!(
            parse_when_at("2030-01-02T03:04:05+01:00", now).unwrap(),
            OffsetDateTime::parse("2030-01-02T02:04:05Z", &Rfc3339).unwrap()
        );
        assert!(parse_when_at("+6w", now).is_err());
        assert!(parse_when_at("tomorrow", now).is_err());
    }

    #[test]
    fn active_leaves_out_finished_competitions() {
        let active = vec!["active".to_string()];
        assert!(state_matches(&active, "awaiting_attestation"));
        assert!(!state_matches(&active, "completed"));
        assert!(!state_matches(&active, "cancelled"));
        assert!(state_matches(&[], "failed"));
        assert!(state_matches(&["failed".to_string()], "failed"));
    }
}
