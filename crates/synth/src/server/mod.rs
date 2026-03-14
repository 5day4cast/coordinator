pub mod metrics;
pub mod routes;

use crate::config::SynthConfig;
use crate::runner::Runner;
use axum::Router;
use log::info;
use std::net::SocketAddr;

pub async fn start_server(config: &SynthConfig, runner: Runner) -> anyhow::Result<()> {
    let app = Router::new()
        .merge(routes::router(runner.clone()))
        .merge(metrics::router());

    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port).parse()?;
    info!("Synth server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
