pub mod live;
pub mod metrics;
pub mod routes;
pub mod run_detail;

use crate::config::SynthConfig;
use crate::rebalance::Rebalancer;
use crate::runner::Runner;
use axum::Router;
use log::info;
use std::net::SocketAddr;

pub async fn start_server(
    config: &SynthConfig,
    runner: Runner,
    rebalancer: Option<Rebalancer>,
) -> anyhow::Result<()> {
    let dashboard = routes::Dashboard {
        runner,
        scenario_config: config.scenario_config(),
        rebalancer,
        live: live::Live::new(),
    };
    // Pages are pushed their live part as things change, rendered once however many watch.
    tokio::spawn(live::render_changes(dashboard.clone()));
    let app = Router::new()
        .merge(routes::router(dashboard))
        .merge(metrics::router());

    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port).parse()?;
    info!("Synth server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
