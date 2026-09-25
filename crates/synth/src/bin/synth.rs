use coordinator_synth::ark_swap::ArkSwap;
use coordinator_synth::client::CoordinatorClient;
use coordinator_synth::config::load_config;
use coordinator_synth::db::SynthDb;
use coordinator_synth::events::Events;
use coordinator_synth::rebalance::Rebalancer;
use coordinator_synth::runner::Runner;
use coordinator_synth::server;
use coordinator_synth::trail::tracker::{self, Tracker};
use log::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_logging();

    let config_path = std::env::args().nth(1);
    let config = load_config(config_path.as_deref())?;

    info!("Synth starting up");
    info!("  Coordinator: {}", config.coordinator.url);
    info!("  Oracle: {}", config.oracle.url);

    let db = SynthDb::new(&config.db.path).await?;
    // Nothing runs yet, so a run still marked running was cut short by the last shutdown.
    let interrupted = db.interrupt_unfinished_runs().await?;
    if interrupted > 0 {
        warn!("Marked {interrupted} run(s) the last shutdown cut short as interrupted");
    }
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
    // Payouts and refunds happen hours after a run ends; follow each run's money until then,
    // looking payments up on the nodes synth can reach and swaps up in ark-swapd.
    let payer = config
        .lnd
        .as_ref()
        .and_then(|payer| tracker::nodes(&[payer]).pop());
    let others = tracker::nodes(
        &config
            .rebalance
            .as_ref()
            .map(|rebalance| &rebalance.source)
            .into_iter()
            .collect::<Vec<_>>(),
    );
    let payee = config.trail.payee.as_ref().map(|payee| tracker::Payee {
        pubkey: payee.pubkey.clone(),
        lnd: payee
            .lnd
            .as_ref()
            .and_then(|lnd| tracker::nodes(&[lnd]).pop()),
    });
    if payee.is_none() {
        warn!("trail.payee is not set, so payouts are confirmed without checking who they paid");
    }
    let ark_swap = match config
        .rebalance
        .as_ref()
        .and_then(|rebalance| rebalance.arkade.as_ref())
    {
        Some(arkade) => Some(ArkSwap::new(&arkade.ark_swap)?),
        None => None,
    };
    let tracker = Tracker::new(
        client.clone(),
        db.clone(),
        events.clone(),
        payer,
        others,
        payee,
        ark_swap,
        config.trail.clone(),
    )?;
    let runner = Runner::new(client, db, events);
    let following = tracker.clone();
    tokio::spawn(async move { following.run().await });

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
    server::start_server(&config, runner, rebalancer, tracker).await?;

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
