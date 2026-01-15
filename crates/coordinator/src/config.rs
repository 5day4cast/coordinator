use anyhow::anyhow;
use bdk_wallet::bitcoin::Network;
use clap::Parser;
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

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Settings {
    pub config: Option<String>,
    pub level: Option<String>,
    pub db_settings: DBSettings,
    pub api_settings: APISettings,
    pub ui_settings: UISettings,
    pub coordinator_settings: CoordinatorSettings,
    pub bitcoin_settings: BitcoinSettings,
    pub ln_settings: LnSettings,
    pub keymeld_settings: KeymeldSettings,
}

impl ConfigurableSettings for Settings {
    fn apply_cli_overrides(&mut self, cli_settings: &CliSettings) {
        if let Some(level) = &cli_settings.level {
            self.level = Some(level.clone());
        }
    }

    fn default_config_path() -> PathBuf {
        PathBuf::from("./config/local.toml")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DBSettings {
    pub data_folder: String,
    pub read_max_connections: u32,
    pub read_min_connections: u32,
    pub write_max_connections: u32,
    pub write_min_connections: u32,
    pub idle_timeout_secs: u64,
    pub acquire_timeout_secs: u64,
    pub sqlite_config: SqliteConfigSerde,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqliteConfigSerde {
    pub mode: String,
    pub cache: String,
    pub busy_timeout_ms: u32,
    pub journal_mode: String,
    pub synchronous: String,
    pub cache_size: i32,
    pub foreign_keys: bool,
    pub wal_autocheckpoint: Option<u32>,
    pub temp_store: String,
    pub mmap_size: Option<u64>,
    pub page_size: Option<u32>,
}

impl Default for DBSettings {
    fn default() -> Self {
        DBSettings {
            data_folder: String::from("./data"),
            read_max_connections: 12,
            read_min_connections: 2,
            write_max_connections: 5,
            write_min_connections: 1,
            idle_timeout_secs: 600,   // 10 minutes
            acquire_timeout_secs: 15, // 15 seconds
            sqlite_config: SqliteConfigSerde::default(),
        }
    }
}

impl Default for SqliteConfigSerde {
    fn default() -> Self {
        Self {
            mode: "ReadWriteCreate".to_string(),
            cache: "Shared".to_string(),
            busy_timeout_ms: 5000,
            journal_mode: "WAL".to_string(),
            synchronous: "NORMAL".to_string(),
            cache_size: 1000000,
            foreign_keys: true,
            wal_autocheckpoint: Some(1000),
            temp_store: "Memory".to_string(),
            mmap_size: Some(268435456), // 256MB
            page_size: Some(4096),
        }
    }
}

impl SqliteConfigSerde {
    pub fn development() -> Self {
        Self {
            busy_timeout_ms: 10000,
            cache_size: 100000,
            ..Default::default()
        }
    }

    pub fn production() -> Self {
        Self {
            synchronous: "FULL".to_string(),
            cache_size: 2000000,
            wal_autocheckpoint: Some(10000),
            mmap_size: Some(1073741824), // 1GB
            ..Default::default()
        }
    }

    pub fn testing() -> Self {
        Self {
            mode: "Memory".to_string(),
            journal_mode: "MEMORY".to_string(),
            synchronous: "OFF".to_string(),
            temp_store: "Memory".to_string(),
            busy_timeout_ms: 1000,
            cache_size: 10000,
            wal_autocheckpoint: None,
            mmap_size: None,
            page_size: None,
            ..Default::default()
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
    /// Interval in seconds to check for new invoices
    pub invoice_watch_interval: u64,
    /// Interval in seconds to check for new payouts
    pub payout_watch_interval: u64,
}

impl Default for LnSettings {
    fn default() -> Self {
        LnSettings {
            base_url: String::from("https://localhost:9095"),
            macaroon_file_path: String::from("./creds/admin.macaroon"),
            tls_cert_path: Some(String::from("./creds/tls.cert")),
            invoice_watch_interval: 5,
            payout_watch_interval: 5,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeymeldSettings {
    /// URL of the Keymeld gateway server
    pub gateway_url: String,
    /// Whether Keymeld integration is enabled
    pub enabled: bool,
    /// Timeout in seconds for keygen sessions
    pub keygen_timeout_secs: u64,
    /// Timeout in seconds for signing sessions
    pub signing_timeout_secs: u64,
    /// Maximum polling attempts for session completion
    pub max_polling_attempts: u32,
    /// Initial polling delay in milliseconds
    pub initial_polling_delay_ms: u64,
    /// Maximum polling delay in milliseconds
    pub max_polling_delay_ms: u64,
    /// Polling backoff multiplier
    pub polling_backoff_multiplier: f64,
}

impl Default for KeymeldSettings {
    fn default() -> Self {
        KeymeldSettings {
            gateway_url: String::from("http://localhost:8080"),
            enabled: false,
            keygen_timeout_secs: 3600,
            signing_timeout_secs: 300,
            max_polling_attempts: 60,
            initial_polling_delay_ms: 500,
            max_polling_delay_ms: 5000,
            polling_backoff_multiplier: 1.5,
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
    pub name: String,
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

    /// The number of confirmations required for a transaction to be considered confirmed
    /// by the coordinator system
    pub required_confirmations: u32,
    pub sync_interval_secs: u64,

    /// Enable on-chain escrow transactions (default: false)
    /// When disabled, only HODL invoices protect against non-completion.
    /// With keymeld signing, escrow is typically not needed since signing is fast.
    /// Enable this as a safety net if HODL invoice timing becomes an issue.
    #[serde(default)]
    pub escrow_enabled: bool,
}

impl Default for CoordinatorSettings {
    fn default() -> Self {
        CoordinatorSettings {
            name: String::from("coordinator"),
            oracle_url: String::from("http://127.0.0.1:9800"),
            private_key_file: String::from("./creds/coordinator_private_key.pem"),
            relative_locktime_block_delta: 144,
            required_confirmations: 1,
            sync_interval_secs: 15,
            escrow_enabled: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UISettings {
    pub private_url: String,
    pub remote_url: String,
    pub ui_dir: String,
}

impl Default for UISettings {
    fn default() -> Self {
        UISettings {
            private_url: String::from("http://127.0.0.1:9990"),
            remote_url: String::from("http://127.0.0.1:9990"),
            ui_dir: String::from("./crates/public_ui"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct APISettings {
    pub domain: String,
    pub port: String,
    pub origins: Vec<String>,
}

impl Default for APISettings {
    fn default() -> Self {
        APISettings {
            domain: String::from("127.0.0.1"),
            port: String::from("9990"),
            origins: vec![String::from("http://localhost:9990")],
        }
    }
}

pub fn get_settings() -> Result<Settings, anyhow::Error> {
    get_settings_with_cli(Cli::parse().into())
}

pub struct CliSettings {
    pub config: Option<String>,
    pub level: Option<String>,
}

impl From<Cli> for CliSettings {
    fn from(cli: Cli) -> Self {
        Self {
            config: cli.config,
            level: cli.level,
        }
    }
}
pub trait ConfigurableSettings: Serialize + for<'de> Deserialize<'de> + Default {
    /// Apply CLI settings after loading from file
    fn apply_cli_overrides(&mut self, cli_settings: &CliSettings);

    /// Get the default config file path
    fn default_config_path() -> PathBuf {
        PathBuf::from("./config/settings.toml")
    }

    /// Get the config directory path
    fn config_directory() -> PathBuf {
        PathBuf::from("./config")
    }
}

pub fn get_settings_with_cli<T: ConfigurableSettings>(
    cli_settings: CliSettings,
) -> Result<T, anyhow::Error> {
    let mut settings = if let Some(config_path) = cli_settings.config.clone() {
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
        let default_path = T::default_config_path();
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
                let default_settings = T::default();

                // Create config directory if it doesn't exist
                fs::create_dir_all(T::config_directory())
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

    settings.apply_cli_overrides(&cli_settings);

    Ok(settings)
}

pub fn setup_logger(
    level: Option<String>,
    filter_targets: Vec<String>,
) -> Result<(), fern::InitError> {
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
        .filter(move |metadata| {
            !filter_targets
                .iter()
                .any(|filter| metadata.target().starts_with(filter))
        })
        .chain(std::io::stdout())
        .apply()?;
    Ok(())
}

pub fn get_log_level(level: Option<String>) -> LevelFilter {
    if let Some(level) = &level {
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
