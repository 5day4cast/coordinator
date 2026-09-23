use coordinator_synth::client::CoordinatorClient;
use coordinator_synth::config::load_config;
use coordinator_synth::db::SynthDb;
use coordinator_synth::events::Events;
use coordinator_synth::rebalance::Rebalancer;
use coordinator_synth::runner::Runner;
use coordinator_synth::server;
use log::info;

/// How often the runs' competitions are checked for changes after the runs end.
const COMPETITION_WATCH_SECS: u64 = 20;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_logging();

    let config_path = std::env::args().nth(1);
    let config = load_config(config_path.as_deref())?;

    info!("Synth starting up");
    info!("  Coordinator: {}", config.coordinator.url);
    info!("  Oracle: {}", config.oracle.url);

    let db = SynthDb::new(&config.db.path).await?;
    let mut client = CoordinatorClient::new(
        &config.coordinator.url,
        config.coordinator.admin_url.as_deref(),
    );
    if let Some(path) = &config.coordinator.admin_token_file {
        client = client.with_admin_token_file(path)?;
    }
    let events = Events::new();
    let rebalancer = match (&config.lnd, &config.rebalance) {
        (Some(payer), Some(rebalance)) => Some(Rebalancer::new(
            payer,
            rebalance.clone(),
            db.clone(),
            events.clone(),
        )?),
        (None, Some(_)) => anyhow::bail!("rebalance needs lnd, the node it pays back"),
        _ => None,
    };
    let runner = Runner::new(client, db, events);

    // Payouts and refunds happen after a run ends; watch its competition for them.
    let watcher = runner.clone();
    tokio::spawn(async move { watcher.watch_competitions(COMPETITION_WATCH_SECS).await });

    // Start scheduled runner if enabled
    if config.scheduler.enabled {
        let scheduler_runner = runner.clone();
        let interval = config.scheduler.interval_secs;
        let scenario = config.scheduler.scenario.clone();
        let scenario_config = config.scenario_config();
        tokio::spawn(async move {
            scheduler_runner
                .run_scheduled(interval, &scenario, scenario_config)
                .await;
        });
    }

    if let Some(rebalancer) = rebalancer.clone().filter(|r| r.config().enabled) {
        tokio::spawn(async move { rebalancer.run_scheduled().await });
    }

    // Start HTTP server
    server::start_server(&config, runner, rebalancer).await?;

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
