use anyhow::Result;
use clap::{Parser, Subcommand};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use server::{
    get_settings_with_cli, setup_logger, Bitcoin, BitcoinClient, BitcoinSettings, CliSettings,
    ConfigurableSettings,
};

#[derive(Parser, Clone)]
#[command(name = "wallet-cli")]
#[command(about = "Bitcoin wallet CLI", long_about = None)]
struct Cli {
    /// Path to Settings.toml file holding configuration options
    #[arg(short, long)]
    config: Option<String>,

    /// Log level to run with the service (default: info)
    #[arg(short, long)]
    level: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Clone)]
enum Commands {
    /// Get a new address
    Address,
    /// Get wallet balance
    Balance,
    /// Sync wallet
    Sync,
}

impl From<Cli> for CliSettings {
    fn from(cli: Cli) -> Self {
        Self {
            config: cli.config,
            level: cli.level,
        }
    }
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct WalletSettings {
    pub level: Option<String>,
    pub bitcoin: BitcoinSettings,
}

impl ConfigurableSettings for WalletSettings {
    fn apply_cli_overrides(&mut self, cli_settings: &CliSettings) {
        if let Some(level) = &cli_settings.level {
            self.level = Some(level.clone());
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let settings: WalletSettings = get_settings_with_cli(cli.clone().into())
        .inspect_err(|e| eprintln!("Error reading config: {}", e))?;

    setup_logger(
        settings.level.clone(),
        vec![
            String::from("hyper"),
            String::from("server"),
            String::from("reqwest"),
        ],
    )?;

    debug!("Settings: {:?}", settings);

    let client = BitcoinClient::new(&settings.bitcoin).await?;

    match cli.command {
        Commands::Address => {
            let address = client.get_next_address().await?;
            info!("New address: {}", address.address);
        }
        Commands::Balance => {
            let balance = client.get_balance().await?;
            info!("Balance: {} sats", balance.total());
        }
        Commands::Sync => {
            client.sync().await?;
            info!("Wallet synced!");
        }
    }

    Ok(())
}
