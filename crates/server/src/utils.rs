use std::{
    env,
    fs::{self, File},
    io::Read,
    path::Path,
};

use clap::{command, Parser};
use fern::colors::{Color, ColoredLevelConfig};
use log::{error, info, LevelFilter};
use reqwest_middleware::reqwest::Url;
use time::{format_description::well_known::Iso8601, OffsetDateTime};

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

    /// Host to listen at (default: 120.0.0.1)
    #[arg(short, long)]
    pub domain: Option<String>,

    /// Port to listen on (default: 9100)
    #[arg(short, long)]
    pub port: Option<String>,

    /// Url UI should hit for the backend (default: http://127.0.0.1:9100)
    #[arg(short, long)]
    pub remote_url: Option<String>,

    /// Path to files used to make the browser UI (default: ./ui)
    #[arg(short, long)]
    pub ui_dir: Option<String>,

    /// Url to hit for oracle weather data (default: https://www.4casttruth.win)
    #[arg(short, long)]
    pub oracle_url: Option<String>,

    /// Path to db holding dlc data (default: competition_data/)
    #[arg(long)]
    pub competition_db: Option<String>,
}

pub struct Settings {
    pub level: Option<String>,
    pub domain: String,
    pub port: String,
    pub remote_url: String,
    pub ui_dir: String,
    pub oracle_url: Url,
    pub competition_db: String,
}

impl From<Cli> for Settings {
    fn from(value: Cli) -> Self {
        Self {
            level: value.level,
            domain: value.domain.unwrap_or(String::from("127.0.0.1")),
            port: value.port.unwrap_or(String::from("9100")),
            remote_url: value
                .remote_url
                .unwrap_or(String::from("http://127.0.0.1:9990")),
            ui_dir: value.ui_dir.unwrap_or(String::from("./ui")),
            oracle_url: Url::parse(
                &value
                    .oracle_url
                    .unwrap_or(String::from("https://www.4casttruth.win")),
            )
            .unwrap(),
            competition_db: value
                .competition_db
                .unwrap_or(String::from("./competition_data")),
        }
    }
}
pub fn get_config_info() -> Settings {
    let mut cli = Cli::parse();

    if let Some(config_path) = cli.config.clone() {
        if let Ok(mut file) = File::open(config_path) {
            let mut content = String::new();
            file.read_to_string(&mut content)
                .expect("Failed to read config file");
            cli = toml::from_str(&content).expect("Failed to deserialize config")
        };
    };
    cli.into()
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
