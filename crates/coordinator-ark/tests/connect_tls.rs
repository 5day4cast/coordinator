//! Connecting to an Arkade server needs no rustls crypto provider from the caller.
//!
//! This file runs in a process of its own, so nothing has installed a provider before the test,
//! as at the coordinator's and ark-swapd's startup.

use coordinator_ark::ArkServer;

/// Nothing listens here, so the connect fails, but it fails with an error rather than a panic
/// over the missing provider.
#[tokio::test]
async fn a_tls_connect_in_a_process_without_a_crypto_provider_returns_an_error() {
    assert!(ArkServer::connect("https://127.0.0.1:1").await.is_err());
}
