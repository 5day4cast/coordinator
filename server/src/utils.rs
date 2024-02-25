use std::{
    env,
    fs::{self, File},
    io::Read,
    path::Path,
};

use clap::{command, Parser};
use slog::{error, info, o, Drain, Level, Logger};

pub fn create_folder(logger: &Logger, root_path: &str) {
    let path = Path::new(root_path);

    if !path.exists() || !path.is_dir() {
        // Create the folder if it doesn't exist
        if let Err(err) = fs::create_dir(path) {
            error!(logger, "error creating folder: {}", err);
        } else {
            info!(logger, "folder created: {}", root_path);
        }
    } else {
        info!(logger, "folder already exists: {}", root_path);
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

    // Url UI should hit for oracle weather data (default: https://www.4casttruth.win)
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

pub fn setup_logger(cli: &Cli) -> Logger {
    let log_level = if cli.level.is_some() {
        let level = cli.level.as_ref().unwrap();
        match level.as_ref() {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" => Level::Warning,
            "error" => Level::Error,
            _ => Level::Info,
        }
    } else {
        let rust_log = env::var("RUST_LOG").unwrap_or_else(|_| String::from(""));
        match rust_log.to_lowercase().as_str() {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" => Level::Warning,
            "error" => Level::Error,
            _ => Level::Info,
        }
    };

    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::CompactFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let drain = drain.filter_level(log_level).fuse();
    slog::Logger::root(drain, o!("version" => "0.5"))
}
