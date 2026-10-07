//! The coordinator recovery keys and relays this build knows, so a player with only their nsec
//! needs no flags.
//!
//! They are committed in `crates/coordinator-recover/recovery-defaults.json` and compiled in, as
//! the browser wallet's Keymeld measurements are: anyone can audit them in source, and every
//! release records the file's digest in its `RELEASE.json`. The CLI and the recovery page both
//! read them.
//!
//! ```json
//! {
//!   "coordinator_pubkeys": { "signet": "<hex or npub>" },
//!   "relays": ["wss://relay.example.org"]
//! }
//! ```
//!
//! - `coordinator_pubkeys`: the coordinator's recovery key by network (`bitcoin`, `signet`,
//!   `testnet4`, …), as `GET /api/v1/recovery/info` serves it. A key is listed once that
//!   coordinator publishes records with it.
//! - `relays`: where records are read from when neither the recovery file nor `--relays`
//!   names any: the relays the coordinator publishes to.
//!
//! `--coordinator-pubkey` and `--relays` override them, and a recovery file brings its own.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use bitcoin::Network;
use nostr::PublicKey;
use serde::Deserialize;

use crate::{Error, Result};

const DEFAULTS_JSON: &str = include_str!("../recovery-defaults.json");

/// Recovery keys and relays, as checked and compiled in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Defaults {
    /// The coordinator's recovery key for each network it has one on, at most one each.
    pub coordinators: Vec<(Network, PublicKey)>,
    /// `wss://` relays, without duplicates.
    pub relays: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultsFile {
    #[serde(default)]
    coordinator_pubkeys: BTreeMap<String, String>,
    #[serde(default)]
    relays: Vec<String>,
}

impl Defaults {
    /// Parse and check a defaults file.
    pub fn parse(json: &str) -> Result<Self> {
        let invalid = |reason: String| Error::Invalid(format!("recovery defaults: {reason}"));
        let file: DefaultsFile = serde_json::from_str(json).map_err(|e| invalid(e.to_string()))?;
        let mut coordinators: Vec<(Network, PublicKey)> = Vec::new();
        for (name, key) in &file.coordinator_pubkeys {
            let network = crate::parse_network(name).map_err(|e| invalid(e.to_string()))?;
            let key = crate::spec::parse_pubkey(key, &format!("coordinator pubkey for {name}"))
                .map_err(|e| invalid(e.to_string()))?;
            if coordinators.iter().any(|(known, _)| *known == network) {
                return Err(invalid(format!("two coordinator pubkeys for {network}")));
            }
            coordinators.push((network, key));
        }
        let mut relays: Vec<String> = Vec::new();
        for relay in file.relays {
            let relay = relay.trim().to_owned();
            if nostr::RelayUrl::parse(&relay).is_err() || !relay.starts_with("wss://") {
                return Err(invalid(format!("relay {relay} must be a wss:// URL")));
            }
            if !relays.contains(&relay) {
                relays.push(relay);
            }
        }
        Ok(Self {
            coordinators,
            relays,
        })
    }

    /// The coordinator's recovery key on `network`, if this build knows one.
    pub fn coordinator(&self, network: Network) -> Option<PublicKey> {
        self.coordinators
            .iter()
            .find(|(known, _)| *known == network)
            .map(|(_, key)| *key)
    }
}

/// This build's defaults. The crate's tests parse the committed file, so a release never ships
/// one that does not parse; should it anyway, there are no defaults rather than a panic.
pub fn built_in() -> &'static Defaults {
    static DEFAULTS: OnceLock<Defaults> = OnceLock::new();
    DEFAULTS.get_or_init(|| Defaults::parse(DEFAULTS_JSON).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    #[test]
    fn the_committed_defaults_parse() {
        let defaults = Defaults::parse(DEFAULTS_JSON).expect("recovery-defaults.json must parse");
        assert_eq!(&defaults, built_in());
        assert!(
            !defaults.relays.is_empty(),
            "a player with only an nsec needs relays to read from"
        );
    }

    #[test]
    fn reads_keys_by_network_and_relays() {
        let defaults = Defaults::parse(&format!(
            r#"{{"coordinator_pubkeys": {{"signet": "{KEY}"}},
                "relays": ["wss://relay.example.org", " wss://relay.example.org "]}}"#
        ))
        .unwrap();
        let key = PublicKey::parse(KEY).unwrap();
        assert_eq!(defaults.coordinator(Network::Signet), Some(key));
        assert_eq!(defaults.coordinator(Network::Bitcoin), None);
        assert_eq!(defaults.relays, ["wss://relay.example.org"]);
        // "mutinynet" names signet, as --network does.
        let defaults = Defaults::parse(&format!(
            r#"{{"coordinator_pubkeys": {{"mutinynet": "{KEY}"}}}}"#
        ))
        .unwrap();
        assert_eq!(defaults.coordinators, [(Network::Signet, key)]);
        assert!(defaults.relays.is_empty());
    }

    #[test]
    fn refuses_what_a_release_must_not_ship() {
        for json in [
            // A network the tool does not know, or a key that is not one.
            format!(r#"{{"coordinator_pubkeys": {{"litecoin": "{KEY}"}}}}"#),
            r#"{"coordinator_pubkeys": {"bitcoin": "not a key"}}"#.to_owned(),
            // Two keys for one network.
            format!(r#"{{"coordinator_pubkeys": {{"signet": "{KEY}", "mutinynet": "{KEY}"}}}}"#),
            // Relays must be wss:// URLs.
            r#"{"relays": ["ws://relay.example.org"]}"#.to_owned(),
            r#"{"relays": ["https://relay.example.org"]}"#.to_owned(),
            // A misspelt field would silently drop what it holds.
            r#"{"relay": ["wss://relay.example.org"]}"#.to_owned(),
        ] {
            assert!(Defaults::parse(&json).is_err(), "{json}");
        }
    }
}
