use anyhow::anyhow;
use bdk_wallet::bitcoin::Network;
use clap::{command, Parser};
use fern::colors::{Color, ColoredLevelConfig};
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
};
use time::{format_description::well_known::Iso8601, OffsetDateTime};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to Settings.toml file holding configuration options
    #[arg(short, long)]
    pub config: Option<String>,

    /// Log level to run with the service (default: info)
    #[arg(short, long)]
    pub level: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub config: Option<String>,
    pub level: Option<String>,
    pub db_settings: DBSettings,
    pub api_settings: APISettings,
    pub ui_settings: UISettings,
    pub coordinator_settings: CoordinatorSettings,
    pub bitcoin_settings: BitcoinSettings,
    pub ln_settings: LnSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DBSettings {
    pub data_folder: String,
}

impl Default for DBSettings {
    fn default() -> Self {
        DBSettings {
            data_folder: String::from("./data"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LnSettings {
    /// Url to find the lnd lightning node's REST api
    pub base_url: String,
    /// File path to the lnd macaroon that has the needed permissions
    pub macaroon_file_path: String,
    /// Optional file path to the lnd tls cert (typically only used in local development, with self signed certs)
    pub tls_cert_path: Option<String>,
}

impl Default for LnSettings {
    fn default() -> Self {
        LnSettings {
            base_url: String::from("https://localhost:9095"),
            macaroon_file_path: String::from("./creds/admin.macaroon"),
            tls_cert_path: Some(String::from("./creds/tls.cert")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BitcoinSettings {
    /// On-chain network to use
    pub network: Network,
    /// Url to grab on-chain data from as an esplora client
    pub esplora_url: String,
    /// Path to the raw seed to use for the wallet (can be the same as the nostr private key file)
    /// The service will generate a private the bitcoin wallet if one is not provided
    /// By default this key will also be used to sign nostr events/auth
    pub seed_path: String,
    /// Path to sqlite file that will store the onchain data related to the wallet
    pub storage_file: String,
    /// Frequency in seconds for how often to refresh block data with on-chain
    /// (usually want to set to half as often as a block on average will come in, 10min block time -> refresh every 5min)
    pub refresh_blocks_secs: u64,
}

impl Default for BitcoinSettings {
    fn default() -> Self {
        BitcoinSettings {
            network: Network::Regtest,
            esplora_url: String::from("http://localhost:9102"),
            storage_file: String::from("./data/bitcoin.db"),
            seed_path: String::from("./creds/coordinator_private_key.pem"),
            refresh_blocks_secs: 15,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoordinatorSettings {
    pub oracle_url: String,
    /// Key to use to sign nostr notes and auth, may also be used for the bitcoin private key
    /// The service will generate one for the bitcoin wallet and use as the signing key for nostr by default
    pub private_key_file: String,

    /// A reasonable number of blocks within which a transaction can confirm.
    /// Used for enforcing relative locktime timeout spending conditions.
    /// We keep this the same for all competitions so it is a known behavior to the users/players
    /// Default is 144 blocks or about 24 hours on mainnet
    /// Reasonable values are:
    ///
    /// - `72`:  ~12 hours
    /// - `144`: ~24 hours
    /// - `432`: ~72 hours
    /// - `1008`: ~1 week
    pub relative_locktime_block_delta: u16,
}

impl Default for CoordinatorSettings {
    fn default() -> Self {
        CoordinatorSettings {
            oracle_url: String::from("https://www.4casttruth.win"),
            private_key_file: String::from("./coordinator_private_key.pem"),
            relative_locktime_block_delta: 144,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UISettings {
    pub private_url: String,
    pub remote_url: String,
    pub ui_dir: String,
    pub admin_ui_dir: String,
}

impl Default for UISettings {
    fn default() -> Self {
        UISettings {
            private_url: String::from("http://127.0.0.1:9100"),
            remote_url: String::from("http://127.0.0.1:9100"),
            ui_dir: String::from("./crates/public_ui"),
            admin_ui_dir: String::from("./crates/admin_ui"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct APISettings {
    pub domain: String,
    pub port: String,
}

impl Default for APISettings {
    fn default() -> Self {
        APISettings {
            domain: String::from("127.0.0.1"),
            port: String::from("8900"),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            config: None,
            level: None,
            db_settings: DBSettings::default(),
            api_settings: APISettings::default(),
            ui_settings: UISettings::default(),
            coordinator_settings: CoordinatorSettings::default(),
            bitcoin_settings: BitcoinSettings::default(),
            ln_settings: LnSettings::default(),
        }
    }
}

pub fn get_settings() -> Result<Settings, anyhow::Error> {
    let cli = Cli::parse();

    let mut settings = if let Some(config_path) = cli.config {
        let path = PathBuf::from(config_path);

        let absolute_path = if path.is_absolute() {
            path
        } else {
            env::current_dir()?.join(path)
        };

        let file_settings = match File::open(absolute_path) {
            Ok(mut file) => {
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .map_err(|e| anyhow!("Failed to read config: {}", e))?;
                toml::from_str(&content)
                    .map_err(|e| anyhow!("Failed to map config to settings: {}", e))?
            }
            Err(err) => return Err(anyhow!("Failed to find file: {}", err)),
        };
        file_settings
    } else {
        let default_path = PathBuf::from("./config/local.toml");
        match File::open(&default_path) {
            Ok(mut file) => {
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .map_err(|e| anyhow!("Failed to read default config: {}", e))?;
                toml::from_str(&content)
                    .map_err(|e| anyhow!("Failed to parse default config: {}", e))?
            }
            Err(_) => {
                // Create default settings
                let default_settings = Settings::default();

                // Create config directory if it doesn't exist
                fs::create_dir_all("./config")
                    .map_err(|e| anyhow!("Failed to create config directory: {}", e))?;

                let toml_content = toml::to_string(&default_settings)
                    .map_err(|e| anyhow!("Failed to serialize default settings: {}", e))?;

                let mut file = fs::File::create(&default_path)
                    .map_err(|e| anyhow!("Failed to create config file: {}", e))?;
                file.write_all(toml_content.as_bytes())
                    .map_err(|e| anyhow!("Failed to write default config: {}", e))?;

                default_settings
            }
        }
    };

    if let Some(cli_level) = cli.level {
        settings.level = Some(cli_level);
    }

    Ok(settings)
}

pub fn setup_logger(level: Option<String>) -> Result<(), fern::InitError> {
    let rust_log = get_log_level(level);
    let colors = ColoredLevelConfig::new()
        .trace(Color::White)
        .debug(Color::Cyan)
        .info(Color::Blue)
        .warn(Color::Yellow)
        .error(Color::Magenta);

    fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "[{} {}] {}: {}",
                OffsetDateTime::now_utc().format(&Iso8601::DEFAULT).unwrap(),
                colors.color(record.level()),
                record.target(),
                message
            ));
        })
        .level(rust_log)
        .filter(|metadata| !metadata.target().starts_with("hyper"))
        .chain(std::io::stdout())
        .apply()?;
    Ok(())
}

pub fn get_log_level(level: Option<String>) -> LevelFilter {
    if level.is_some() {
        let level = level.as_ref().unwrap();
        match level.as_ref() {
            "trace" => LevelFilter::Trace,
            "debug" => LevelFilter::Debug,
            "info" => LevelFilter::Info,
            "warn" => LevelFilter::Warn,
            "error" => LevelFilter::Error,
            _ => LevelFilter::Info,
        }
    } else {
        let rust_log = env::var("RUST_LOG").unwrap_or_else(|_| String::from(""));
        match rust_log.to_lowercase().as_str() {
            "trace" => LevelFilter::Trace,
            "debug" => LevelFilter::Debug,
            "info" => LevelFilter::Info,
            "warn" => LevelFilter::Warn,
            "error" => LevelFilter::Error,
            _ => LevelFilter::Info,
        }
    }
}
