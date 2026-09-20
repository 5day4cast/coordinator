//! Separate host process carrying TLS records for the custom enclave.
use anyhow::{ensure, Result};
use coordinator_lnurl_relay::{run_relay, RelayListener};
use tokio_vsock::{VsockAddr, VsockListener};

#[tokio::main]
async fn main() -> Result<()> {
    let port = std::env::var("COORDINATOR_LNURL_RELAY_PORT")
        .ok()
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(8101);
    ensure!(port != 0, "COORDINATOR_LNURL_RELAY_PORT must be nonzero");
    let listener = match std::env::var("TRANSPORT_MODE").as_deref() {
        Ok("tcp") => {
            ensure!(
                std::env::var("KEYMELD_DANGEROUS_TRUST_UNATTESTED_ENCLAVES").as_deref()
                    == Ok("true"),
                "TCP relay requires explicit enclave development mode"
            );
            RelayListener::Tcp(
                tokio::net::TcpListener::bind(("127.0.0.1", u16::try_from(port)?)).await?,
            )
        }
        _ => RelayListener::Vsock(VsockListener::bind(VsockAddr::new(u32::MAX, port))?),
    };
    tokio::select! {
        result = run_relay(listener) => result,
        result = tokio::signal::ctrl_c() => Ok(result?),
    }
}
