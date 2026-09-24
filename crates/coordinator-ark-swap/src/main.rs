//! `ark-swapd`: swaps Lightning payments into Arkade escrow VTXOs.
//!
//! The coordinator asks for a swap into an entry's escrow address and shows the player its invoice.
//! This service holds the payment for seconds, pays the escrow from its own Ark wallet, then settles.
//! It needs no third-party swap provider. See `swap.rs` for the steps and `api.rs` for the routes.

mod api;
mod config;
mod lnd;
mod refund;
mod store;
mod swap;
mod wallet;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;

use crate::config::Config;
use crate::lnd::Lnd;
use crate::store::Store;
use crate::swap::Swapper;
use crate::wallet::ArkWallet;

#[derive(Parser)]
#[command(
    name = "ark-swapd",
    about = "Swap Lightning payments into Arkade escrow VTXOs"
)]
struct Cli {
    /// The TOML settings file.
    #[arg(long, env = "ARK_SWAPD_CONFIG")]
    config: PathBuf,
}

/// How often unfinished swaps advance. An HTLC is held for about this long before the escrow is paid.
const TICK: Duration = Duration::from_secs(1);
/// How often the wallet's holder boards what has confirmed at its boarding address.
const BOARD_EVERY: Duration = Duration::from_secs(60);

/// How long the worker lease outlives its holder. Another instance takes over a stopped one's
/// swaps after this, or at once when it shuts down cleanly.
const LEASE_TTL: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} {} {}: {}",
                time::OffsetDateTime::now_utc(),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info)
        .chain(std::io::stdout())
        .apply()?;

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("create {}", config.data_dir.display()))?;
    let token = config.api_token()?;

    let swapper = Arc::new(Swapper {
        store: Store::open(&config.data_dir.join("swaps.sqlite")).await?,
        lnd: Lnd::new(&config.lnd)?,
        wallet: ArkWallet::open(&config).await?,
        invoice_expiry_secs: config.invoice_expiry_secs,
        invoice_cltv_expiry: config.invoice_cltv_expiry,
    });
    let view = swapper.wallet.view().await?;
    log::info!(
        "Ark wallet {} holds {} sat confirmed and {} sat preconfirmed; boarding address {}",
        view.ark_address,
        view.confirmed_sat,
        view.pre_confirmed_sat,
        view.boarding_address
    );

    // Two instances can share the database during a blue/green deploy. Only the lease holder
    // advances swaps and moves the wallet's coins; both serve the API.
    let holder = format!("ark-swapd-{}", uuid::Uuid::now_v7());
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let worker = swapper.clone();
    let worker_holder = holder.clone();
    let mut worker_stopped = stopped.clone();
    let worker_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut holding = false;
        let mut boarded_at = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = worker_stopped.changed() => break,
            }
            match worker.store.take_lease(&worker_holder, LEASE_TTL).await {
                Ok(true) => {
                    if !holding {
                        log::info!("{worker_holder} runs the swaps");
                        holding = true;
                    }
                    // A tick always finishes, so a payment in flight records its result.
                    worker.tick().await;
                    worker.refund_tick().await;
                    if boarded_at.elapsed() >= BOARD_EVERY {
                        worker.board_tick().await;
                        boarded_at = tokio::time::Instant::now();
                    }
                }
                Ok(false) => {
                    if holding {
                        log::warn!("another ark-swapd instance took over the swaps");
                        holding = false;
                    }
                }
                Err(error) => log::warn!("cannot take the worker lease: {error:#}"),
            }
        }
        if let Err(error) = worker.store.release_lease(&worker_holder).await {
            log::warn!("cannot release the worker lease: {error:#}");
        }
    });

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    log::info!("listening on {} as {holder}", config.listen);
    axum::serve(listener, api::router(swapper, token, holder))
        .with_graceful_shutdown(async move {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            log::info!("stopping: finishing the current swap tick, then handing over");
            let _ = stop.send(true);
        })
        .await?;
    worker_task.await?;
    Ok(())
}
