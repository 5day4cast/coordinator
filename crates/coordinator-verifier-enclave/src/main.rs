//! Coordinator's measured enclave image installs its own trusted rules.
use anyhow::Result;
use keymeld_core::{managed_socket::config::TimeoutConfig, EnclaveId};
use keymeld_enclave::{
    create_enclave_operator_with_verifiers,
    escrow_verifier::VerifierRegistry,
    server::{EnclaveServer, ServerConfig, TransportMode},
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    keymeld_enclave::init_enclave_logging();
    let port = std::env::var("VSOCK_PORT")
        .ok()
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(5000);
    let enclave_id = std::env::var("ENCLAVE_ID")
        .ok()
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(0);
    let verifier = coordinator_escrow_verifier::config::from_env()?;
    let registry = VerifierRegistry::new(vec![Arc::new(verifier)])?;
    let operator = create_enclave_operator_with_verifiers(EnclaveId::new(enclave_id), registry)?;
    let config = ServerConfig {
        port,
        transport_mode: TransportMode::from_env(),
        tcp_host: std::env::var("TCP_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        ..Default::default()
    };
    EnclaveServer::new(config, operator, TimeoutConfig::default())
        .await?
        .start()
        .await
}
