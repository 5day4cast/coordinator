//! Keymeld enclave measurements the browser trusts.
//!
//! The browser encrypts entry keys to a Keymeld enclave only after verifying
//! the enclave's Nitro attestation. The measurements it verifies against must
//! not come from the coordinator: a coordinator that chose them could point the
//! browser at its own enclave and collect entry keys. They are committed in
//! `keymeld-trusted-pcrs.json` and compiled into this WASM, so anyone can audit
//! them in source and compare the served WASM with the release build.
//!
//! With no pins compiled in, mainnet registration is refused. Test networks
//! fall back to the coordinator-supplied trust, which the UI reports as such.

use super::WalletError;
use coordinator_core::RegistrationAssignment;
use dlctix::bitcoin::Network;
use serde::Serialize;
use std::collections::BTreeMap;

const PINNED_PCRS_JSON: &str = include_str!("../../keymeld-trusted-pcrs.json");
/// A PCR is a SHA-384 measurement.
const PCR_HEX_LEN: usize = 96;

/// Where the enclave trust for a registration comes from, for display.
#[derive(Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum EnclaveTrust {
    /// Measurements compiled into this WASM build.
    Pinned { pcrs: BTreeMap<u16, String> },
    /// No pins in this build; the coordinator's assignment decides (test networks only).
    Coordinator,
}

pub fn pinned_pcrs() -> Result<BTreeMap<u16, String>, WalletError> {
    parse_pins(PINNED_PCRS_JSON)
}

fn parse_pins(json: &str) -> Result<BTreeMap<u16, String>, WalletError> {
    let pins: BTreeMap<u16, String> = serde_json::from_str(json)
        .map_err(|e| WalletError::Keymeld(format!("invalid pinned PCR file: {e}")))?;
    let valid = pins.iter().all(|(index, value)| {
        (*index == 0 || *index == 8)
            && value.len() == PCR_HEX_LEN
            && value.bytes().all(|b| b.is_ascii_hexdigit())
            && value.bytes().any(|b| b != b'0')
    });
    if valid {
        Ok(pins)
    } else {
        Err(WalletError::Keymeld(
            "pinned PCRs must be nonzero SHA-384 PCR0/PCR8 hex values".into(),
        ))
    }
}

pub fn enclave_trust() -> Result<EnclaveTrust, WalletError> {
    let pcrs = pinned_pcrs()?;
    Ok(if pcrs.is_empty() {
        EnclaveTrust::Coordinator
    } else {
        EnclaveTrust::Pinned { pcrs }
    })
}

/// The assignment to register with: the coordinator's slot and session data,
/// with the attestation policy replaced by this build's pins.
pub fn trusted_assignment(
    assignment: &RegistrationAssignment,
    network: Network,
) -> Result<RegistrationAssignment, WalletError> {
    apply_pins(assignment, network, pinned_pcrs()?)
}

fn apply_pins(
    assignment: &RegistrationAssignment,
    network: Network,
    pins: BTreeMap<u16, String>,
) -> Result<RegistrationAssignment, WalletError> {
    let mut trusted = assignment.clone();
    if !pins.is_empty() {
        trusted.trusted_pcrs = pins;
        trusted.dangerous_trust_unattested_enclaves = false;
        return Ok(trusted);
    }
    if network == Network::Bitcoin {
        return Err(WalletError::Keymeld(
            "this build pins no keymeld enclave measurements; refusing on mainnet".into(),
        ));
    }
    Ok(trusted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn assignment(dangerous: bool, pcrs: BTreeMap<u16, String>) -> RegistrationAssignment {
        RegistrationAssignment {
            session_id: "session".into(),
            user_id: Uuid::now_v7(),
            manifest_hash: vec![1; 32],
            enclave_id: 1,
            enclave_key_epoch: 1,
            enclave_public_key: "key".into(),
            gateway_url: "https://keymeld.example".into(),
            trusted_pcrs: pcrs,
            dangerous_trust_unattested_enclaves: dangerous,
            payout_policy: None,
        }
    }

    fn pcr(byte: char) -> String {
        byte.to_string().repeat(PCR_HEX_LEN)
    }

    #[test]
    fn committed_pin_file_parses() {
        pinned_pcrs().unwrap();
    }

    #[test]
    fn pins_override_coordinator_trust() {
        let pins = BTreeMap::from([(8, pcr('a'))]);
        let from_coordinator = assignment(true, BTreeMap::new());
        let trusted = apply_pins(&from_coordinator, Network::Signet, pins.clone()).unwrap();
        assert_eq!(trusted.trusted_pcrs, pins);
        assert!(!trusted.dangerous_trust_unattested_enclaves);

        let evil_pins = assignment(false, BTreeMap::from([(8, pcr('b'))]));
        let trusted = apply_pins(&evil_pins, Network::Bitcoin, pins.clone()).unwrap();
        assert_eq!(trusted.trusted_pcrs, pins);
    }

    #[test]
    fn mainnet_without_pins_is_refused() {
        for from_coordinator in [
            assignment(true, BTreeMap::new()),
            assignment(false, BTreeMap::from([(8, pcr('b'))])),
        ] {
            assert!(apply_pins(&from_coordinator, Network::Bitcoin, BTreeMap::new()).is_err());
        }
    }

    #[test]
    fn test_networks_without_pins_use_coordinator_trust() {
        let from_coordinator = assignment(true, BTreeMap::new());
        let trusted = apply_pins(&from_coordinator, Network::Signet, BTreeMap::new()).unwrap();
        assert!(trusted.dangerous_trust_unattested_enclaves);
    }

    #[test]
    fn rejects_malformed_pins() {
        for json in [
            r#"{"8": "abc"}"#,
            &format!(r#"{{"3": "{}"}}"#, pcr('a')),
            &format!(r#"{{"8": "{}"}}"#, pcr('0')),
            &format!(r#"{{"8": "{}"}}"#, pcr('z')),
            "not json",
        ] {
            assert!(parse_pins(json).is_err(), "{json}");
        }
        assert_eq!(
            parse_pins(&format!(r#"{{"8": "{}"}}"#, pcr('a'))).unwrap(),
            BTreeMap::from([(8, pcr('a'))])
        );
    }
}
