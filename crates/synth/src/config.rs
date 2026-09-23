use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SynthConfig {
    pub coordinator: CoordinatorConfig,
    /// The node scenarios pay entries from, for the ones that need real payments.
    #[serde(default)]
    pub lnd: Option<crate::lnd::LndConfig>,
    /// Paying `lnd` back as scenarios drain it.
    #[serde(default)]
    pub rebalance: Option<crate::rebalance::RebalanceConfig>,
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
    /// Coordinator operator listener URL (competition creation, test settlement).
    /// Set this separately from the participant API, including for local development.
    pub admin_url: Option<String>,
    /// File holding the coordinator's operator token, sent as a bearer token to
    /// `admin_url`. Required unless the coordinator allows unauthenticated
    /// operator access.
    #[serde(default)]
    pub admin_token_file: Option<String>,
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
    /// Time before observation starts (entry window) in seconds
    pub entry_window_secs: u64,
    /// Observation window duration in seconds
    pub observation_window_secs: u64,
    /// Delay after observation ends before signing deadline, in seconds
    pub signing_delay_secs: u64,
    /// The players' Lightning Address, where payouts and refunds go. It must resolve publicly:
    /// the enclave checks a refund's invoice against it before signing.
    #[serde(default)]
    pub lightning_address: Option<String>,
    /// Max time to wait for a refund to settle, which includes its escrow's locktime.
    #[serde(default = "default_refund_timeout_secs")]
    pub refund_timeout_secs: u64,
}

fn default_refund_timeout_secs() -> u64 {
    30 * 60
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            lnd: None,
            rebalance: None,
            coordinator: CoordinatorConfig {
                url: "http://coordinator.coordinator.svc.cluster.local:9990".to_string(),
                admin_url: Some(
                    "http://coordinator-admin.coordinator.svc.cluster.local:9991".to_string(),
                ),
                admin_token_file: None,
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
                entry_window_secs: 120,
                observation_window_secs: 300,
                signing_delay_secs: 60,
                lightning_address: None,
                refund_timeout_secs: default_refund_timeout_secs(),
            },
        }
    }
}

impl SynthConfig {
    /// What a scenario run starts from, whether scheduled or triggered from the dashboard.
    pub fn scenario_config(&self) -> crate::scenarios::ScenarioConfig {
        crate::scenarios::ScenarioConfig {
            users: self.defaults.users,
            stations: self.defaults.stations.clone(),
            entry_fee: self.defaults.entry_fee,
            entry_window_secs: self.defaults.entry_window_secs,
            observation_window_secs: self.defaults.observation_window_secs,
            signing_delay_secs: self.defaults.signing_delay_secs,
            lightning_address: self.defaults.lightning_address.clone(),
            refund_timeout_secs: self.defaults.refund_timeout_secs,
            lnd: self.lnd.clone(),
            ..Default::default()
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
            "coordinator.admin_url",
            "http://coordinator-admin.coordinator.svc.cluster.local:9991",
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
        .set_default("defaults.entry_window_secs", 120)?
        .set_default("defaults.observation_window_secs", 300)?
        .set_default("defaults.signing_delay_secs", 60)?
        .set_default("defaults.refund_timeout_secs", 30 * 60)?;

    let config = builder.build()?;
    Ok(config.try_deserialize()?)
}
