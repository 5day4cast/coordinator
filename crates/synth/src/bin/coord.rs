use anyhow::Result;
use clap::{Parser, Subcommand};
use coordinator_synth::client::CoordinatorClient;
use coordinator_synth::client::competitions::CreateCompetition;
use coordinator_synth::db::SynthDb;
use coordinator_synth::runner::Runner;
use coordinator_synth::scenarios::ScenarioConfig;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "coord", about = "CLI for coordinator and synthetic testing")]
struct Cli {
    /// Coordinator URL
    #[arg(long, env = "COORDINATOR_URL", default_value = "http://localhost:9990")]
    url: String,

    /// Coordinator admin URL (defaults to same as url)
    #[arg(long, env = "COORDINATOR_ADMIN_URL")]
    admin_url: Option<String>,

    /// SQLite database path for synth data
    #[arg(long, env = "SYNTH_DB_PATH", default_value = "./data/synth.db")]
    db_path: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Competition management
    Competitions {
        #[command(subcommand)]
        action: CompetitionCommands,
    },
    /// Wallet operations
    Wallet {
        #[command(subcommand)]
        action: WalletCommands,
    },
    /// Synthetic test operations
    Synth {
        #[command(subcommand)]
        action: SynthCommands,
    },
}

#[derive(Subcommand)]
enum CompetitionCommands {
    /// List all competitions
    List,
    /// Get competition details
    Get {
        /// Competition ID
        id: Uuid,
    },
    /// Create a new competition
    Create {
        /// NOAA station codes (comma-separated)
        #[arg(long, default_value = "KDEN,KJFK,KORD")]
        stations: String,
        /// Entry fee in sats
        #[arg(long, default_value = "1000")]
        entry_fee: usize,
        /// Max entries
        #[arg(long, default_value = "5")]
        max_entries: usize,
        /// Observation window in minutes
        #[arg(long, default_value = "5")]
        observation_window: u64,
    },
}

#[derive(Subcommand)]
enum WalletCommands {
    /// Show wallet balance
    Balance,
    /// Get a new wallet address
    Address,
}

#[derive(Subcommand)]
enum SynthCommands {
    /// Run a test scenario
    Run {
        /// Scenario name
        #[arg(default_value = "full_lifecycle")]
        scenario: String,
        /// Number of synthetic users
        #[arg(long, default_value = "3")]
        users: usize,
        /// Observation window in minutes
        #[arg(long, default_value = "5")]
        observation_window: u64,
    },
    /// Show status of last run
    Status,
    /// Show run history
    History {
        /// Number of runs to show
        #[arg(long, default_value = "20")]
        limit: i64,
    },
    /// List synthetic users
    Users,
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging();

    let cli = Cli::parse();
    let client = CoordinatorClient::new(&cli.url, cli.admin_url.as_deref());

    match cli.command {
        Commands::Competitions { action } => match action {
            CompetitionCommands::List => {
                let competitions = client.list_competitions().await?;
                if competitions.is_empty() {
                    println!("No competitions found");
                    return Ok(());
                }
                println!("{:<38} {:<22} {:<8} {:<8}", "ID", "STATUS", "ENTRIES", "PAID");
                println!("{}", "-".repeat(80));
                for comp in &competitions {
                    println!(
                        "{:<38} {:<22} {:<8} {:<8}",
                        comp.id,
                        comp.inferred_status(),
                        comp.total_entries,
                        comp.total_paid_entries,
                    );
                }
            }
            CompetitionCommands::Get { id } => {
                let comp = client.get_competition(&id).await?;
                println!("Competition: {}", comp.id);
                println!("Status:      {}", comp.inferred_status());
                println!("Entries:     {}/{}", comp.total_entries, "?");
                println!("Paid:        {}", comp.total_paid_entries);
                println!("Paid Out:    {}", comp.total_paid_out_entries);
                println!("Created:     {}", comp.created_at);
                if let Some(t) = comp.completed_at {
                    println!("Completed:   {}", t);
                }
                if let Some(t) = comp.failed_at {
                    println!("Failed:      {}", t);
                }
            }
            CompetitionCommands::Create {
                stations,
                entry_fee,
                max_entries,
                observation_window,
            } => {
                let station_list: Vec<String> =
                    stations.split(',').map(|s| s.trim().to_string()).collect();
                let now = OffsetDateTime::now_utc();
                let window = time::Duration::minutes(observation_window as i64);

                let competition = CreateCompetition {
                    id: Uuid::now_v7(),
                    signing_date: now + window + time::Duration::seconds(60),
                    start_observation_date: now,
                    end_observation_date: now + window,
                    locations: station_list.clone(),
                    number_of_values_per_entry: station_list.len() * 3,
                    number_of_places_win: 1,
                    total_allowed_entries: max_entries,
                    entry_fee,
                    coordinator_fee_percentage: 10,
                    total_competition_pool: entry_fee * max_entries,
                };

                let resp = client.create_competition(&competition).await?;
                println!("Created competition: {}", resp.id);
                println!("Status: {}", resp.inferred_status());
            }
        },
        Commands::Wallet { action } => match action {
            WalletCommands::Balance => {
                let balance = client.wallet_balance().await?;
                println!("Wallet Balance:");
                println!("  Confirmed:         {} sats", balance.confirmed);
                println!("  Trusted Pending:   {} sats", balance.trusted_pending);
                println!("  Untrusted Pending: {} sats", balance.untrusted_pending);
                println!("  Immature:          {} sats", balance.immature);
            }
            WalletCommands::Address => {
                let addr = client.wallet_address().await?;
                println!("{}", addr.address);
            }
        },
        Commands::Synth { action } => {
            let db = SynthDb::new(&cli.db_path).await?;
            let runner = Runner::new(client, db);

            match action {
                SynthCommands::Run {
                    scenario,
                    users,
                    observation_window,
                } => {
                    let config = ScenarioConfig {
                        users,
                        observation_window_secs: observation_window * 60,
                        ..Default::default()
                    };

                    println!("Running scenario: {}", scenario);
                    println!("  Users: {}", users);
                    println!(
                        "  Observation window: {} min",
                        observation_window
                    );
                    println!();

                    let result = runner.run_scenario(&scenario, config).await?;

                    println!();
                    println!("Result: {:?}", result.status);
                    println!("Duration: {}ms", result.total_duration_ms);
                    println!();
                    println!("{:<30} {:<10} {:<12} {}", "STEP", "STATUS", "DURATION", "ERROR");
                    println!("{}", "-".repeat(80));
                    for step in &result.steps {
                        println!(
                            "{:<30} {:<10} {:<12} {}",
                            step.name,
                            format!("{:?}", step.status),
                            format!("{}ms", step.duration_ms),
                            step.error.as_deref().unwrap_or("-"),
                        );
                    }

                    if let Some(ref err) = result.error {
                        println!();
                        println!("Error: {}", err);
                        std::process::exit(1);
                    }
                }
                SynthCommands::Status => {
                    let last = runner.last_result().await;
                    match last {
                        Some(result) => {
                            println!("Last run: {} ({:?})", result.scenario, result.status);
                            println!("Duration: {}ms", result.total_duration_ms);
                            if let Some(ref err) = result.error {
                                println!("Error: {}", err);
                            }
                        }
                        None => println!("No runs recorded in this session"),
                    }
                }
                SynthCommands::History { limit } => {
                    let runs = runner.db().list_runs(limit).await?;
                    if runs.is_empty() {
                        println!("No run history");
                        return Ok(());
                    }
                    println!(
                        "{:<12} {:<20} {:<10} {}",
                        "ID", "SCENARIO", "STATUS", "STARTED"
                    );
                    println!("{}", "-".repeat(60));
                    for run in &runs {
                        println!(
                            "{:<12} {:<20} {:<10} {}",
                            &run.id[..8],
                            run.scenario,
                            run.status,
                            run.started_at,
                        );
                    }
                }
                SynthCommands::Users => {
                    let users = runner.db().list_users().await?;
                    if users.is_empty() {
                        println!("No synthetic users (they'll be created on first run)");
                        return Ok(());
                    }
                    println!("{:<12} {:<12} {}", "NAME", "CREATED", "NOSTR PUBKEY");
                    println!("{}", "-".repeat(80));
                    for user in &users {
                        println!(
                            "{:<12} {:<12} {}",
                            user.name,
                            &user.created_at[..10],
                            &user.nostr_pubkey[..16],
                        );
                    }
                }
            }
        },
    }

    Ok(())
}

fn setup_logging() {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}] {}",
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Warn)
        .level_for("coordinator_synth", log::LevelFilter::Info)
        .chain(std::io::stderr())
        .apply()
        .expect("Failed to initialize logging");
}
