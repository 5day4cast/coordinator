//! Operator-owned configuration. Requests cannot enable network access or add TLS roots.
use crate::CoordinatorVerifier;
#[cfg(feature = "lnurl")]
use anyhow::ensure;
use anyhow::Result;

pub fn from_env() -> Result<CoordinatorVerifier> {
    let enabled = parse_enabled(
        std::env::var("COORDINATOR_ESCROW_LNURL_ENABLED")
            .ok()
            .as_deref(),
    )?;
    if !enabled {
        return Ok(CoordinatorVerifier::default());
    }
    #[cfg(feature = "lnurl")]
    {
        Ok(CoordinatorVerifier::with_lnurl(
            crate::lnurl_transport::LnurlPayClient::new(relay_connector()?),
        ))
    }
    #[cfg(not(feature = "lnurl"))]
    {
        anyhow::bail!(
            "COORDINATOR_ESCROW_LNURL_ENABLED requires the custom enclave's lnurl feature"
        );
    }
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
