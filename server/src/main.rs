use axum::Server;
use server::{app, get_config_info, setup_logger};
use slog::info;
use std::{net::SocketAddr, str::FromStr};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli: server::Cli = get_config_info();
    let logger = setup_logger(&cli);
    let address = SocketAddr::from_str(&format!(
        "{}:{}",
        cli.domain.unwrap_or(String::from("127.0.0.1")),
        cli.port.unwrap_or(String::from("9990"))
    ))
    .unwrap();

    info!(logger, "listening on http://{}", address);

    let app = app(
        logger,
        cli.remote_url
            .unwrap_or(String::from("http://127.0.0.1:9990")),
        cli.oracle_url
            .unwrap_or(String::from("https://www.4casttruth.win")),
        cli.ui_dir.unwrap_or(String::from("./server/ui")),
    );
    Server::bind(&address)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}
