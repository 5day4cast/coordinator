//! Operator-owned configuration. Requests cannot enable network access or add TLS roots.
use crate::CoordinatorVerifier;
use anyhow::Result;
#[cfg(feature = "lnurl")]
use anyhow::{ensure, Context};

pub fn from_env() -> Result<CoordinatorVerifier> {
    let enabled = parse_enabled(
        std::env::var("COORDINATOR_ESCROW_LNURL_ENABLED")
            .ok()
            .as_deref(),
    )?;
    configured_verifier(enabled)
}

#[cfg(feature = "lnurl")]
fn configured_verifier(enabled: bool) -> Result<CoordinatorVerifier> {
    use coordinator_escrow::payout_witness::AuthenticationKey;
    use std::sync::Arc;
    let url = std::env::var("COORDINATOR_PAYOUT_WITNESS_URL")
        .context("Durable payout witness URL is required")?
        .parse()?;
    let ledger_id = std::env::var("COORDINATOR_PAYOUT_WITNESS_LEDGER_ID")
        .context("Pinned payout ledger identity is required")?
        .parse()?;
    let key_path = std::env::var("COORDINATOR_PAYOUT_WITNESS_KEY_FILE")
        .context("Confidentially provisioned payout witness key file is required")?;
    let key_bytes = zeroize::Zeroizing::new(std::fs::read_to_string(key_path)?);
    let key = AuthenticationKey::from_hex(&key_bytes)?;
    let transport = crate::lnurl_transport::LnurlPayClient::new(relay_connector()?)?;
    let verifier = if enabled {
        CoordinatorVerifier::with_lnurl(transport.clone())
    } else {
        CoordinatorVerifier::default()
    };
    Ok(
        verifier.with_witness(Arc::new(crate::witness::HttpsWitness::new(
            transport, url, ledger_id, key,
        )?)),
    )
}

#[cfg(not(feature = "lnurl"))]
fn configured_verifier(_enabled: bool) -> Result<CoordinatorVerifier> {
    anyhow::bail!("Durable payout witness requires the custom enclave's lnurl HTTPS feature")
}

fn parse_enabled(value: Option<&str>) -> Result<bool> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        _ => anyhow::bail!("COORDINATOR_ESCROW_LNURL_ENABLED must be true or false"),
    }
}
#[cfg(feature = "lnurl")]
fn relay_connector() -> Result<keymeld_core::managed_socket::SocketConnector> {
    use keymeld_core::managed_socket::SocketConnector;
    let port = std::env::var("COORDINATOR_LNURL_RELAY_PORT")
        .ok()
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(8101);
    ensure!(port != 0, "COORDINATOR_LNURL_RELAY_PORT must be nonzero");
    if matches!(
        keymeld_enclave::server::TransportMode::from_env(),
        keymeld_enclave::server::TransportMode::Tcp
    ) {
        ensure!(
            std::env::var("KEYMELD_DANGEROUS_TRUST_UNATTESTED_ENCLAVES").as_deref() == Ok("true"),
            "TCP LNURL relay requires explicit enclave development mode"
        );
        Ok(SocketConnector::tcp("127.0.0.1", u16::try_from(port)?))
    } else {
        Ok(SocketConnector::vsock(3, port))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_opt_in_is_disabled_by_default_and_strict() {
        assert!(!parse_enabled(None).unwrap());
        assert!(!parse_enabled(Some("false")).unwrap());
        assert!(parse_enabled(Some("true")).unwrap());
        assert!(parse_enabled(Some("yes")).is_err());
        assert!(parse_enabled(Some("")).is_err());
        assert!(!CoordinatorVerifier::default().lnurl_enabled());
    }
}
