//! Operator-owned configuration. Requests cannot enable network access or add TLS roots.
use crate::CoordinatorVerifier;
use anyhow::{ensure, Context, Result};

pub async fn from_env() -> Result<CoordinatorVerifier> {
    let enabled = parse_enabled(optional_env("COORDINATOR_ESCROW_LNURL_ENABLED")?.as_deref())?;
    let mode = parse_mode(optional_env("COORDINATOR_PAYOUT_WITNESS_MODE")?.as_deref())?;
    validate_mode_settings(
        mode,
        std::env::var_os("COORDINATOR_PAYOUT_WITNESS_DATABASE").is_some(),
        std::env::var_os("COORDINATOR_PAYOUT_WITNESS_URL").is_some(),
        std::env::var_os("COORDINATOR_PAYOUT_WITNESS_KEY_FILE").is_some(),
    )?;
    match mode {
        WitnessMode::Https => configured_https_verifier(enabled),
        WitnessMode::LocalSimulation => configured_local_verifier(enabled).await,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WitnessMode {
    Https,
    LocalSimulation,
}

fn optional_env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Invalid environment value for {name}")),
    }
}

fn parse_mode(value: Option<&str>) -> Result<WitnessMode> {
    match value {
        None | Some("https") => Ok(WitnessMode::Https),
        Some("local-simulation") => Ok(WitnessMode::LocalSimulation),
        _ => anyhow::bail!("COORDINATOR_PAYOUT_WITNESS_MODE must be https or local-simulation"),
    }
}

fn validate_mode_settings(
    mode: WitnessMode,
    has_database: bool,
    has_url: bool,
    has_key_file: bool,
) -> Result<()> {
    match mode {
        WitnessMode::Https => ensure!(
            !has_database,
            "Local witness database requires explicit local-simulation mode"
        ),
        WitnessMode::LocalSimulation => ensure!(
            has_database && !has_url && !has_key_file,
            "Local simulation requires a witness database and refuses HTTPS witness settings"
        ),
    }
    Ok(())
}

#[cfg(feature = "lnurl")]
fn configured_https_verifier(enabled: bool) -> Result<CoordinatorVerifier> {
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
fn configured_https_verifier(_enabled: bool) -> Result<CoordinatorVerifier> {
    anyhow::bail!("Durable payout witness requires the custom enclave's lnurl HTTPS feature")
}

#[cfg(feature = "payout-witness-local")]
async fn configured_local_verifier(enabled: bool) -> Result<CoordinatorVerifier> {
    let path = std::env::var("COORDINATOR_PAYOUT_WITNESS_DATABASE")
        .context("Explicit local witness database is required")?;
    let ledger_id = std::env::var("COORDINATOR_PAYOUT_WITNESS_LEDGER_ID")
        .context("Pinned payout ledger identity is required")?
        .parse()?;
    let witness =
        crate::witness::LocalSimulationWitness::open(std::path::Path::new(&path), ledger_id)
            .await?;
    let verifier = if enabled {
        local_lnurl_verifier()?
    } else {
        CoordinatorVerifier::default()
    };
    Ok(verifier.with_witness(std::sync::Arc::new(witness)))
}

#[cfg(not(feature = "payout-witness-local"))]
async fn configured_local_verifier(_enabled: bool) -> Result<CoordinatorVerifier> {
    anyhow::bail!("Local witness mode requires the payout-witness-local simulation feature")
}

#[cfg(all(feature = "payout-witness-local", feature = "lnurl"))]
fn local_lnurl_verifier() -> Result<CoordinatorVerifier> {
    Ok(CoordinatorVerifier::with_lnurl(
        crate::lnurl_transport::LnurlPayClient::new(relay_connector()?)?,
    ))
}

#[cfg(all(feature = "payout-witness-local", not(feature = "lnurl")))]
fn local_lnurl_verifier() -> Result<CoordinatorVerifier> {
    anyhow::bail!("Automatic Lightning Address payouts require the lnurl feature")
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
    fn witness_mode_requires_explicit_local_selection() {
        assert_eq!(parse_mode(None).unwrap(), WitnessMode::Https);
        assert_eq!(parse_mode(Some("https")).unwrap(), WitnessMode::Https);
        assert_eq!(
            parse_mode(Some("local-simulation")).unwrap(),
            WitnessMode::LocalSimulation
        );
        for value in ["", "local", "memory", "LOCAL-SIMULATION"] {
            assert!(parse_mode(Some(value)).is_err());
        }
    }

    #[test]
    fn witness_modes_reject_ambiguous_or_incomplete_local_settings() {
        assert!(validate_mode_settings(WitnessMode::Https, false, true, true).is_ok());
        assert!(validate_mode_settings(WitnessMode::Https, true, true, true).is_err());
        assert!(validate_mode_settings(WitnessMode::LocalSimulation, true, false, false).is_ok());
        for (database, url, key_file) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
            (true, true, true),
        ] {
            assert!(
                validate_mode_settings(WitnessMode::LocalSimulation, database, url, key_file)
                    .is_err()
            );
        }
    }

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
