use anyhow::anyhow;
use axum::serve;
use log::info;
use server::{app, create_folder, get_config_info, setup_logger};
use std::{net::SocketAddr, str::FromStr};
use tokio::{net::TcpListener, signal};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = get_config_info();
    setup_logger(settings.level.clone())?;
    let address = SocketAddr::from_str(&format!("{}:{}", settings.domain, settings.port)).unwrap();
    create_folder(&settings.competition_db.clone());
    let listener = TcpListener::bind(address)
        .await
        .map_err(|e| anyhow!("error binding to IO socket: {}", e.to_string()))?;
    info!("listening on http://{}", address.clone());

    let app = app(settings);

    serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
