//! `ark-swapd`: swaps Lightning payments into Arkade escrow VTXOs.
//!
//! The coordinator asks for a swap into an entry's escrow address and shows the player its invoice.
//! This service holds the payment for seconds, pays the escrow from its own Ark wallet, then settles.
//! It needs no third-party swap provider. See `swap.rs` for the steps and `api.rs` for the routes.

mod api;
mod config;
mod lnd;
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

    let worker = swapper.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            worker.tick().await;
        }
    });

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    log::info!("listening on {}", config.listen);
    axum::serve(listener, api::router(swapper, token)).await?;
    Ok(())
}
