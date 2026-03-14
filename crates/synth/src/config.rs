use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SynthConfig {
    pub coordinator: CoordinatorConfig,
    pub oracle: OracleConfig,
    pub server: ServerConfig,
    pub db: DbConfig,
    pub scheduler: SchedulerConfig,
    pub defaults: DefaultsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CoordinatorConfig {
    /// Coordinator API URL (e.g. http://coordinator.coordinator.svc.cluster.local:9990)
    pub url: String,
    /// Coordinator admin URL (defaults to same as url)
    pub admin_url: Option<String>,
}

impl CoordinatorConfig {
    pub fn admin_url(&self) -> &str {
        self.admin_url.as_deref().unwrap_or(&self.url)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OracleConfig {
    /// Oracle API URL for monitoring
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Host to bind to
    pub host: String,
    /// Port to bind to
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DbConfig {
    /// Path to SQLite database file
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// Whether to enable scheduled test runs
    pub enabled: bool,
    /// Interval between scheduled runs in seconds
    pub interval_secs: u64,
    /// Scenario to run on schedule
    pub scenario: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DefaultsConfig {
    /// Number of synthetic users per test
    pub users: usize,
    /// NOAA stations to use for competitions
    pub stations: Vec<String>,
    /// Entry fee in sats
    pub entry_fee: usize,
    /// Observation window duration in seconds
    pub observation_window_secs: u64,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            coordinator: CoordinatorConfig {
                url: "http://coordinator.coordinator.svc.cluster.local:9990".to_string(),
                admin_url: None,
            },
            oracle: OracleConfig {
                url: "http://noaa-oracle.noaa-oracle.svc.cluster.local:9800".to_string(),
            },
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 9980,
            },
            db: DbConfig {
                path: "./data/synth.db".to_string(),
            },
            scheduler: SchedulerConfig {
                enabled: false,
                interval_secs: 3600,
                scenario: "full_lifecycle".to_string(),
            },
            defaults: DefaultsConfig {
                users: 3,
                stations: vec!["KDEN".to_string(), "KJFK".to_string(), "KORD".to_string()],
                entry_fee: 1000,
                observation_window_secs: 300,
            },
        }
    }
}

pub fn load_config(path: Option<&str>) -> anyhow::Result<SynthConfig> {
    let builder = config::Config::builder();

    let builder = if let Some(path) = path {
        builder.add_source(config::File::with_name(path))
    } else {
        builder
    };

    let builder = builder
        .add_source(config::Environment::with_prefix("SYNTH").separator("__"))
        .set_default(
            "coordinator.url",
            "http://coordinator.coordinator.svc.cluster.local:9990",
        )?
        .set_default(
            "oracle.url",
            "http://noaa-oracle.noaa-oracle.svc.cluster.local:9800",
        )?
        .set_default("server.host", "0.0.0.0")?
        .set_default("server.port", 9980)?
        .set_default("db.path", "./data/synth.db")?
        .set_default("scheduler.enabled", false)?
        .set_default("scheduler.interval_secs", 3600)?
        .set_default("scheduler.scenario", "full_lifecycle")?
        .set_default("defaults.users", 3)?
        .set_default("defaults.entry_fee", 1000)?
        .set_default("defaults.observation_window_secs", 300)?;

    let config = builder.build()?;
    Ok(config.try_deserialize()?)
}
