use anyhow::Result;
use bdk_wallet::bitcoin::{OutPoint, Txid};
use clap::{Parser, Subcommand};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use server::SendOptions;
use server::{
    get_settings_with_cli, setup_logger, Bitcoin, BitcoinClient, BitcoinSettings, CliSettings,
    ConfigurableSettings,
};
use std::str::FromStr;

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
    /// List unspent transaction outputs (UTXOs)
    ListUtxos,
    /// Send a transaction
    Send(SendCommandArgs),
}

#[derive(Parser, Clone)]
struct SendCommandArgs {
    /// Destination address
    #[arg(long)]
    to: String,

    /// Amount to send in satoshis
    #[arg(long)]
    amount: u64,

    /// Maximum fee in satoshis (optional)
    #[arg(long)]
    max_fee: Option<u64>,

    /// Source address to spend from (optional)
    #[arg(long)]
    from: Option<String>,

    /// Specific UTXOs to spend (format: "txid:vout") (optional, can be used multiple times)
    #[arg(long)]
    utxo: Option<Vec<String>>,
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
        Commands::ListUtxos => {
            let utxos = client.list_utxos().await;
            match serde_json::to_string_pretty(&utxos) {
                Ok(json) => info!("UTXOs: \n{}", json),
                Err(e) => error!("Failed to serialize UTXOs: {}", e),
            }
        }
        Commands::Send(args) => {
            let send_options = SendOptions {
                address_to: args.to.clone(),
                address_from: args.from, // Optional source address
                amount: Some(args.amount),
                max_fee: args.max_fee,
            };

            let mut selected_utxos = Vec::new();
            if let Some(utxo_strings) = args.utxo {
                for utxo_str in utxo_strings {
                    // Parse "txid:vout" format
                    let parts: Vec<&str> = utxo_str.split(':').collect();
                    if parts.len() != 2 {
                        error!("Invalid UTXO format: {}. Expected 'txid:vout'", utxo_str);
                        return Ok(());
                    }

                    // Parse txid
                    let txid = match Txid::from_str(parts[0]) {
                        Ok(txid) => txid,
                        Err(e) => {
                            error!("Invalid transaction ID: {}", e);
                            return Ok(());
                        }
                    };

                    // Parse vout
                    let vout = match parts[1].parse::<u32>() {
                        Ok(vout) => vout,
                        Err(e) => {
                            error!("Invalid vout: {}", e);
                            return Ok(());
                        }
                    };

                    selected_utxos.push(OutPoint { txid, vout });
                }
            }

            // Send the transaction
            info!("Sending {} sats to {}", args.amount, args.to);

            if !selected_utxos.is_empty() {
                info!("Using {} specific UTXOs", selected_utxos.len());
            }

            match client.send_to_address(send_options, selected_utxos).await {
                Ok(txid) => info!("Transaction sent! TXID: {}", txid),
                Err(e) => error!("Failed to send transaction: {}", e),
            }
        }
    }

    Ok(())
}
