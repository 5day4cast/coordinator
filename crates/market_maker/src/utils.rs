use std::{
    fs::{self, File},
    io::Read,
    path::Path,
};
use clap::{command, Parser};
use log::{error, info};
use secrecy::Secret;

pub fn create_folder(root_path: &str) {
    let path = Path::new(root_path);

    if !path.exists() || !path.is_dir() {
        // Create the folder if it doesn't exist
        if let Err(err) = fs::create_dir(path) {
            error!("error creating folder: {}", err);
        } else {
            info!("folder created: {}", root_path);
        }
    } else {
        info!("folder already exists: {}", root_path);
    }
}

pub fn subfolder_exists(subfolder_path: &str) -> bool {
    fs::metadata(subfolder_path).is_ok()
}

#[derive(Parser, Clone, Debug, serde::Deserialize)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to Settings.toml file holding the rest of the cli options
    #[arg(short, long)]
    pub config: Option<String>,

    /// Set the log level (default: info)
    #[arg(short, long)]
    pub level: Option<String>,

    /// Url to LND node used for invoice generation
    pub lnd_url: Option<String>,

    /// Macaroon to use when communicating with LND node
    pub macaroon: Option<Secret<String>>,

    /// Connect to BTC node used for on-chain data
    pub btc_url: Option<String>,

    /// Url UI should hit for oracle weather data (default: https://www.4casttruth.win)
    #[arg(short, long)]
    pub oracle_url: Option<String>
}

pub fn get_config_info() -> Cli {
    let mut cli = Cli::parse();

    if let Some(config_path) = cli.config.clone() {
        if let Ok(mut file) = File::open(config_path) {
            let mut content = String::new();
            file.read_to_string(&mut content)
                .expect("Failed to read config file");
            cli = toml::from_str(&content).expect("Failed to deserialize config")
        };
    };
    cli
}