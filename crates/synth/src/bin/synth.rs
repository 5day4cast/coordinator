use coordinator_synth::client::CoordinatorClient;
use coordinator_synth::config::load_config;
use coordinator_synth::db::SynthDb;
use coordinator_synth::runner::Runner;
use coordinator_synth::scenarios::ScenarioConfig;
use coordinator_synth::server;
use log::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_logging();

    let config_path = std::env::args().nth(1);
    let config = load_config(config_path.as_deref())?;

    info!("Synth starting up");
    info!("  Coordinator: {}", config.coordinator.url);
    info!("  Oracle: {}", config.oracle.url);

    let db = SynthDb::new(&config.db.path).await?;
    let client = CoordinatorClient::new(
        &config.coordinator.url,
        config.coordinator.admin_url.as_deref(),
    );
    let runner = Runner::new(client, db);

    // Start scheduled runner if enabled
    if config.scheduler.enabled {
        let scheduler_runner = runner.clone();
        let interval = config.scheduler.interval_secs;
        let scenario = config.scheduler.scenario.clone();
        let scenario_config = ScenarioConfig {
            users: config.defaults.users,
            stations: config.defaults.stations.clone(),
            entry_fee: config.defaults.entry_fee,
            observation_window_secs: config.defaults.observation_window_secs,
            ..ScenarioConfig::default()
        };
        tokio::spawn(async move {
            scheduler_runner
                .run_scheduled(interval, &scenario, scenario_config)
                .await;
        });
    }

    // Start HTTP server
    server::start_server(&config, runner).await?;

    Ok(())
}

fn setup_logging() {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}] {}",
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info)
        .level_for("coordinator_synth", log::LevelFilter::Debug)
        .chain(std::io::stdout())
        .apply()
        .expect("Failed to initialize logging");
}
