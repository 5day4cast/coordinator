//! Untrusted LNURL egress relay. It never terminates TLS or sees HTTP bodies.

use anyhow::{ensure, Result};
use coordinator_escrow::lnurl_relay::{
    read_control, validate_addresses, validate_dns_host, write_control, RelayRequest,
    RelayResponse, MAX_DNS_ADDRESSES,
};
use keymeld_core::managed_socket::SocketStream;
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_vsock::VsockListener;

const MAX_CONNECTIONS: usize = 64;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_TUNNEL_BYTES: u64 = 1024 * 1024;

/// Bind the listener before spawning, so startup reports configuration errors.
pub enum RelayListener {
    Tcp(TcpListener),
    Vsock(VsockListener),
}

impl RelayListener {
    async fn accept(&mut self) -> std::io::Result<SocketStream> {
        match self {
            Self::Tcp(listener) => listener.accept().await.map(|(s, _)| SocketStream::Tcp(s)),
            Self::Vsock(listener) => listener.accept().await.map(|(s, _)| SocketStream::Vsock(s)),
        }
    }
}

pub async fn run_relay(mut listener: RelayListener) -> Result<()> {
    // JoinSet aborts every child when the listener task is stopped, so shutting
    // down or disabling the relay cannot leave detached egress streams alive.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = connections.join_next(), if !connections.is_empty() => {},
            socket = listener.accept() => {
                let socket = socket?;
                if connections.len() >= MAX_CONNECTIONS {
                    // Drop excess connections instead of allocating an unbounded queue.
                    continue;
                }
                connections.spawn(async move {
                    if !matches!(tokio::time::timeout(CONNECTION_TIMEOUT, relay_connection(socket)).await, Ok(Ok(()))) {
                        // Do not log provider hostnames, callback tokens, or raw errors.
                        tracing::debug!("LNURL relay connection closed without completion");
                    }
                });
            }
        }
    }
}

async fn relay_connection<S: AsyncRead + AsyncWrite + Unpin>(mut socket: S) -> Result<()> {
    let request = tokio::time::timeout(CONTROL_TIMEOUT, read_control(&mut socket)).await??;
    match request {
        RelayRequest::Resolve { host, port } => {
            let result = async {
                validate_dns_host(&host)?;
                ensure!(port != 0, "Invalid LNURL relay port");
                // Include an extra answer to detect and reject oversized sets.
                let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), port))
                    .await?
                    .take(MAX_DNS_ADDRESSES + 1)
                    .collect();
                validate_addresses(&addresses, port)?;
                Ok::<_, anyhow::Error>(addresses)
            };
            let response = match tokio::time::timeout(CONTROL_TIMEOUT, result).await {
                Ok(Ok(addresses)) => RelayResponse::Addresses(addresses),
                _ => RelayResponse::Rejected,
            };
            write_control(&mut socket, &response).await?;
        }
        RelayRequest::Connect { address } => {
            // This uses a SocketAddr throughout: no DNS lookup occurs here.
            let result = async {
                validate_addresses(&[address], address.port())?;
                Ok::<_, anyhow::Error>(TcpStream::connect(address).await?)
            };
            let mut upstream = match tokio::time::timeout(CONTROL_TIMEOUT, result).await {
                Ok(Ok(stream)) => stream,
                _ => {
                    write_control(&mut socket, &RelayResponse::Rejected).await?;
                    return Ok(());
                }
            };
            write_control(&mut socket, &RelayResponse::Connected).await?;
            let (from_enclave, to_enclave) = tokio::io::split(&mut socket);
            let (from_provider, to_provider) = upstream.split();
            tokio::try_join!(
                copy_bounded(from_enclave, to_provider),
                copy_bounded(from_provider, to_enclave)
            )?;
        }
    }
    Ok(())
}

async fn copy_bounded<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: R,
    mut writer: W,
) -> Result<()> {
    let count = tokio::io::copy(&mut reader.take(MAX_TUNNEL_BYTES + 1), &mut writer).await?;
    ensure!(
        count <= MAX_TUNNEL_BYTES,
        "LNURL tunnel byte limit exceeded"
    );
    writer.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_rejects_private_connect_targets_before_connecting() {
        for address in [
            "127.0.0.1:443",
            "169.254.169.254:80",
            "[::1]:443",
            "8.8.8.8:0",
        ] {
            let (mut client, server) = tokio::io::duplex(4096);
            let task = tokio::spawn(relay_connection(server));
            write_control(
                &mut client,
                &RelayRequest::Connect {
                    address: address.parse().unwrap(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_control::<_, RelayResponse>(&mut client).await.unwrap(),
                RelayResponse::Rejected
            ));
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn relay_rejects_unsafe_resolution_requests() {
        for host in [
            "localhost",
            "provider.onion",
            "host/path.example",
            "user@wallet.example",
        ] {
            let (mut client, server) = tokio::io::duplex(4096);
            let task = tokio::spawn(relay_connection(server));
            write_control(
                &mut client,
                &RelayRequest::Resolve {
                    host: host.into(),
                    port: 443,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_control::<_, RelayResponse>(&mut client).await.unwrap(),
                RelayResponse::Rejected
            ));
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn tunnel_forwarding_has_a_byte_limit() {
        let input = vec![0; (MAX_TUNNEL_BYTES + 2) as usize];
        assert!(copy_bounded(&input[..], tokio::io::sink()).await.is_err());
        assert!(
            copy_bounded(&input[..MAX_TUNNEL_BYTES as usize], tokio::io::sink())
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn stopping_listener_closes_accepted_child_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(run_relay(RelayListener::Tcp(listener)));
        let mut client = TcpStream::connect(address).await.unwrap();
        // A partial control frame keeps the accepted child blocked on reading.
        client.write_u32(64).await.unwrap();
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), client.read(&mut byte))
                .await
                .is_err()
        );
        task.abort();
        let _ = task.await;
        let closed = tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0)) || closed.is_err());
    }
}
