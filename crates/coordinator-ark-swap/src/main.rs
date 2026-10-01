//! `ark-swapd`: swaps Lightning payments into Arkade escrow VTXOs.
//!
//! The coordinator asks for a swap into an entry's escrow address and shows the player its invoice.
//! This service holds the payment for seconds, pays the escrow from its own Ark wallet, then settles.
//! It needs no third-party swap provider. See `swap.rs` for the steps and `api.rs` for the routes.

mod api;
mod coins;
mod config;
mod electrum;
mod invoices;
mod lnd;
mod onchain_wallet;
mod refund;
mod store;
mod swap;
mod wallet;
mod worker;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
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
/// How often the wallet's holder boards what has confirmed at its boarding address, and renews
/// the VTXOs that are recoverable or close to expiry.
const BOARD_EVERY: Duration = Duration::from_secs(60);
/// How often settled swaps whose escrow VTXO is unknown are checked for lookups that are due.
/// Each swap waits out its own backoff; see `Swapper::lookup_tick`.
const LOOKUP_EVERY: Duration = Duration::from_secs(10);

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
        invoices: Default::default(),
        errors: Default::default(),
        refund_turn: Default::default(),
        renewed: Default::default(),
    });
    let view = swapper.wallet.view().await?;
    log::info!(
        "Ark wallet {} holds {} sat confirmed and {} sat preconfirmed, {} sat of it too close to \
         expiry to pay an escrow, and {} sat recoverable; {} sat await boarding at {}",
        view.ark_address,
        view.confirmed_sat,
        view.pre_confirmed_sat,
        view.expiring_sat,
        view.recoverable_sat,
        view.boarding_sat,
        view.boarding_address
    );

    // Two instances can share the database during a blue/green deploy. Only the lease holder
    // advances swaps and moves the wallet's coins; both serve the API.
    let holder = format!("ark-swapd-{}", uuid::Uuid::now_v7());
    let (stop, stopped) = tokio::sync::watch::channel(false);

    // The lease is renewed apart from the work, so a slow tick cannot let it lapse. It is released
    // only once the worker has finished its last tick.
    let holding = Arc::new(AtomicBool::new(false));
    let (worker_done, worker_finished) = tokio::sync::watch::channel(false);
    let lease_task = {
        let swapper = swapper.clone();
        let holder = holder.clone();
        let holding = holding.clone();
        tokio::spawn(async move {
            swapper
                .store
                .keep_lease(&holder, LEASE_TTL, TICK, &holding, worker_finished)
                .await
        })
    };

    let session = swapper.wallet.session_duration();
    let cadence = worker::Cadence {
        tick: TICK,
        lookup_every: LOOKUP_EVERY,
        board_every: BOARD_EVERY,
        // ark-client gives a boarding or a renewal two sessions, the rest of one and the batch
        // after, and stops waiting itself. This bounds one that hangs past that.
        board_timeout: 2 * session + Duration::from_secs(60),
    };
    let worker_task = {
        let stopped = stopped.clone();
        let swapper = swapper.clone();
        tokio::spawn(async move {
            worker::run(swapper, cadence, holding, stopped).await;
            let _ = worker_done.send(true);
        })
    };

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
    lease_task.await?;
    Ok(())
}
