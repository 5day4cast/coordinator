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
    /// Following each run's money after its steps, and where to point people to look it up.
    #[serde(default)]
    pub trail: crate::trail::tracker::TrailConfig,
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
    #[serde(default = "default_scenario")]
    pub scenario: String,
    /// Rotate these cases when configured; otherwise keep the legacy single scenario.
    #[serde(default)]
    pub scenarios: Option<Vec<String>>,
    /// Streams of competitions run side by side, each on its own cadence, their runs
    /// overlapping. When set, `interval_secs` and the scenarios above are not used.
    #[serde(default)]
    pub lanes: Vec<crate::runner::lanes::LaneConfig>,
}

fn default_scenario() -> String {
    "full_lifecycle".into()
}

impl SchedulerConfig {
    pub fn scenario_names(&self) -> Vec<&str> {
        self.scenarios.as_ref().map_or_else(
            || vec![self.scenario.as_str()],
            |names| names.iter().map(String::as_str).collect(),
        )
    }
    /// Check the cadence and every duration before creating or paying for a run.
    pub fn validate(&self, windows: &[u64]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.interval_secs > 0,
            "scheduler.interval_secs must be positive"
        );
        let scenarios = self.scenario_names();
        anyhow::ensure!(
            !scenarios.is_empty()
                && scenarios
                    .iter()
                    .all(|name| crate::runner::SCENARIOS.contains(name)),
            "scheduler.scenarios must contain supported scenario names"
        );
        validate_windows(windows)
    }
}

/// A list for varied runs, or the scalar accepted by older configurations.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ObservationWindows {
    Fixed(#[serde(deserialize_with = "window_seconds")] u64),
    Varying(Vec<u64>),
}

// Environment sources supply strings; preserve the old scalar env override too.
fn window_seconds<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Seconds {
        Number(u64),
        Text(String),
    }
    match Seconds::deserialize(deserializer)? {
        Seconds::Number(seconds) => Ok(seconds),
        Seconds::Text(seconds) => seconds.parse().map_err(serde::de::Error::custom),
    }
}

impl Default for ObservationWindows {
    fn default() -> Self {
        Self::Varying(crate::scenarios::default_observation_windows())
    }
}

impl ObservationWindows {
    pub fn values(&self) -> &[u64] {
        match self {
            Self::Fixed(seconds) => std::slice::from_ref(seconds),
            Self::Varying(windows) => windows,
        }
    }
}

fn validate_windows(windows: &[u64]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !windows.is_empty()
            && windows
                .iter()
                .all(|seconds| (1..=604_800).contains(seconds)),
        "defaults.observation_windows_secs must contain at least one positive duration"
    );
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub entry_timing: crate::scenarios::EntryTiming,
    /// A fixed number of players for every run. Unset, each run draws its count from `players`.
    #[serde(default)]
    pub users: Option<usize>,
    /// How many players a run draws when `users` is unset.
    #[serde(default)]
    pub players: crate::scenarios::PlayerMix,
    /// NOAA stations to use for competitions
    pub stations: Vec<String>,
    /// Entry fee in sats
    pub entry_fee: usize,
    /// Time before observation starts (entry window) in seconds
    pub entry_window_secs: u64,
    /// Observation durations in seconds. Manual runs choose one; scheduled runs rotate.
    #[serde(default, alias = "observation_window_secs")]
    pub observation_windows_secs: ObservationWindows,
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
                scenarios: None,
                lanes: Vec::new(),
            },
            trail: Default::default(),
            defaults: DefaultsConfig {
                seed: None,
                entry_timing: Default::default(),
                users: None,
                players: Default::default(),
                stations: vec!["KDEN".to_string(), "KJFK".to_string(), "KORD".to_string()],
                entry_fee: 1000,
                entry_window_secs: 120,
                observation_windows_secs: ObservationWindows::default(),
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
            seed: self.defaults.seed,
            entry_timing: self.defaults.entry_timing.clone(),
            observation_window_choices: self.defaults.observation_windows_secs.values().to_vec(),
            users: self.defaults.users.unwrap_or(3),
            player_mix: match self.defaults.users {
                Some(_) => None,
                None => Some(self.defaults.players.clone()),
            },
            stations: self.defaults.stations.clone(),
            entry_fee: self.defaults.entry_fee,
            entry_window_secs: self.defaults.entry_window_secs,
            observation_window_secs: self.defaults.observation_windows_secs.values()[0],
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
        .set_default("defaults.entry_fee", 1000)?
        .set_default("defaults.entry_window_secs", 120)?
        .set_default("defaults.signing_delay_secs", 60)?
        .set_default("defaults.refund_timeout_secs", 30 * 60)?;

    let config: SynthConfig = builder.build()?.try_deserialize()?;
    validate_windows(config.defaults.observation_windows_secs.values())?;
    config.defaults.players.validate()?;
    config
        .defaults
        .entry_timing
        .validate(config.defaults.entry_window_secs)?;
    if config.scheduler.enabled {
        config
            .scheduler
            .validate(config.defaults.observation_windows_secs.values())?;
        let base = config.scenario_config();
        for lane in &config.scheduler.lanes {
            lane.validate(&base)?;
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // load_config reads process environment; serialize these tests with the env override test.
    static CONFIG_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn list_only_scheduler_and_timing_profile_load_without_enabling_new_defaults() {
        let _environment = CONFIG_ENV.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.toml");
        std::fs::write(&path, r#"
[scheduler]
enabled = true
scenarios = ["full_lifecycle", "abandoned_unpaid", "paid_abandonment", "duplicate_submission", "late_submission", "escrow_refund"]
[defaults]
stations = ["KDEN"]
entry_window_secs = 1200
[defaults.entry_timing]
deadline_margin_secs = 60
arrival = { min_secs = 0, max_secs = 90 }
before_payment = { min_secs = 5, max_secs = 60 }
before_submit = { min_secs = 10, max_secs = 120 }
"#).unwrap();
        let config = load_config(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(config.scheduler.scenario_names().len(), 6);
        assert_eq!(config.defaults.entry_timing.before_payment.min_secs, 5);
        assert_eq!(
            SynthConfig::default().scheduler.scenario_names(),
            ["full_lifecycle"]
        );
        assert_eq!(
            SynthConfig::default().defaults.entry_timing,
            Default::default()
        );
        for cases in ["[]", "[\"not_a_scenario\"]"] {
            std::fs::write(&path, format!("[scheduler]\nenabled = true\nscenarios = {cases}\n[defaults]\nstations = [\"KDEN\"]\n")).unwrap();
            assert!(load_config(Some(path.to_str().unwrap())).is_err());
        }
    }

    #[test]
    fn lanes_load_and_are_checked_against_the_oracles_windows() {
        let _environment = CONFIG_ENV.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.toml");
        let lanes = |half_window: &str| {
            format!(
                r#"
[scheduler]
enabled = true

[[scheduler.lanes]]
name = "open"
interval_secs = 3600
entry_window_secs = 3600
scenarios = ["full_lifecycle", "duplicate_submission"]
observation_windows_secs = [86400, 172800]
stations_per_run = 2

[[scheduler.lanes]]
name = "halves"
align = "utc_half"
entry_window_secs = 3600
scenarios = ["full_lifecycle"]
observation_windows_secs = [{half_window}]

[defaults]
stations = ["KDEN", "KJFK", "KORD"]
observation_windows_secs = [86400]
entry_window_secs = 3600
[defaults.entry_timing]
arrival_pattern = "spread"
before_payment = {{ min_secs = 5, max_secs = 60 }}
before_submit = {{ min_secs = 10, max_secs = 120 }}
deadline_margin_secs = 60
"#
            )
        };
        std::fs::write(&path, lanes("43200")).unwrap();
        let config = load_config(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(config.scheduler.lanes.len(), 2);
        assert_eq!(
            config.scheduler.lanes[1].align,
            crate::runner::lanes::Align::UtcHalf
        );
        assert_eq!(config.defaults.users, None, "counts are drawn");
        // A 12-hour window starting whenever a run happens to is refused.
        std::fs::write(&path, lanes("43200").replace("align = \"utc_half\"\n", "")).unwrap();
        assert!(load_config(Some(path.to_str().unwrap())).is_err());
    }

    #[test]
    fn omitted_windows_use_varied_defaults_including_the_short_case() {
        let _environment = CONFIG_ENV.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.toml");
        std::fs::write(&path, "[defaults]\nstations = [\"KDEN\"]\n").unwrap();
        let config = load_config(Some(path.to_str().unwrap())).unwrap();
        let expected = [7200, 10800, 14400, 600];
        assert_eq!(config.defaults.observation_windows_secs.values(), expected);
        assert_eq!(
            SynthConfig::default()
                .defaults
                .observation_windows_secs
                .values(),
            expected
        );
        assert!(expected.contains(&config.scenario_config().observation_window_secs));
        assert!(
            expected.contains(&crate::scenarios::ScenarioConfig::default().observation_window_secs)
        );
    }

    #[test]
    fn custom_windows_and_legacy_scalar_load_while_empty_and_zero_windows_fail() {
        let _environment = CONFIG_ENV.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.toml");
        for (setting, expected) in [
            ("observation_windows_secs = []", None),
            ("observation_windows_secs = [7200, 0]", None),
            (
                "observation_windows_secs = [7200, 10800, 14400, 600]",
                Some(vec![7200, 10800, 14400, 600]),
            ),
            (
                "observation_windows_secs = [1800, 5400]",
                Some(vec![1800, 5400]),
            ),
            ("observation_window_secs = 300", Some(vec![300])),
        ] {
            std::fs::write(
                &path,
                format!(
                    "[scheduler]\nenabled = true\n[defaults]\nstations = [\"KDEN\"]\n{setting}\n"
                ),
            )
            .unwrap();
            let result = load_config(Some(path.to_str().unwrap()));
            match expected {
                Some(expected) => {
                    let config = result.unwrap();
                    assert_eq!(config.defaults.observation_windows_secs.values(), expected);
                    assert!(expected.contains(&config.scenario_config().observation_window_secs));
                }
                None => assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("at least one positive")),
            }
        }
    }

    /// The payee checks and the Arkade lookups are configured under `trail`, from the config file
    /// or from `SYNTH__`-prefixed environment variables.
    #[test]
    fn the_payee_and_arkd_settings_are_read() {
        let _environment = CONFIG_ENV.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.toml");
        std::fs::write(
            &path,
            r#"
[defaults]
stations = ["KDEN"]

[trail]
arkd_url = "https://arkd.example"

[trail.payee]
pubkey = "02aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[trail.payee.lnd]
rest_url = "https://payee.example:8080"
macaroon_file = "/run/secrets/payee-invoices-read.macaroon"
tls_cert_file = "/run/secrets/payee.tls.cert"
"#,
        )
        .unwrap();
        let config = load_config(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(
            config.trail.arkd_url.as_deref(),
            Some("https://arkd.example")
        );
        let payee = config.trail.payee.expect("trail.payee");
        assert_eq!(
            payee.pubkey.as_deref(),
            Some("02aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        let lnd = payee.lnd.expect("trail.payee.lnd");
        assert_eq!(lnd.rest_url, "https://payee.example:8080");
        assert_eq!(
            lnd.macaroon_file,
            Path::new("/run/secrets/payee-invoices-read.macaroon")
        );
        assert_eq!(
            lnd.tls_cert_file.as_deref(),
            Some(Path::new("/run/secrets/payee.tls.cert"))
        );

        // The same settings from the environment, with nothing about them in the file.
        let bare = directory.path().join("bare.toml");
        std::fs::write(&bare, "[defaults]\nstations = [\"KDEN\"]\n").unwrap();
        let variables = [
            ("SYNTH__DEFAULTS__OBSERVATION_WINDOW_SECS", "300"),
            ("SYNTH__TRAIL__ARKD_URL", "https://arkd.example"),
            ("SYNTH__TRAIL__PAYEE__PUBKEY", "02ab"),
            (
                "SYNTH__TRAIL__PAYEE__LND__REST_URL",
                "https://payee.example:8080",
            ),
            ("SYNTH__TRAIL__PAYEE__LND__MACAROON_FILE", "/run/secrets/m"),
            ("SYNTH__TRAIL__PAYEE__LND__TLS_CERT_FILE", "/run/secrets/c"),
        ];
        for (name, value) in variables {
            std::env::set_var(name, value);
        }
        let config = load_config(Some(bare.to_str().unwrap()));
        for (name, _) in variables {
            std::env::remove_var(name);
        }
        let config = config.unwrap();
        assert_eq!(config.defaults.observation_windows_secs.values(), [300]);
        assert_eq!(
            config.trail.arkd_url.as_deref(),
            Some("https://arkd.example")
        );
        let payee = config.trail.payee.expect("trail.payee");
        assert_eq!(payee.pubkey.as_deref(), Some("02ab"));
        let lnd = payee.lnd.expect("trail.payee.lnd");
        assert_eq!(lnd.rest_url, "https://payee.example:8080");
        assert_eq!(lnd.macaroon_file, Path::new("/run/secrets/m"));
        assert_eq!(
            lnd.tls_cert_file.as_deref(),
            Some(Path::new("/run/secrets/c"))
        );
    }
}
