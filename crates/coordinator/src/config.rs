use anyhow::anyhow;
use bitcoin::Network;
use clap::Parser;
use fern::colors::{Color, ColoredLevelConfig};
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};
use time::{format_description::well_known::Iso8601, OffsetDateTime};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to Settings.toml file holding configuration options
    #[arg(short, long)]
    pub config: Option<String>,

    /// Log level to run with the service (default: info)
    #[arg(short, long)]
    pub level: Option<String>,

    /// Without a command, run the coordinator.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Drive a running coordinator through its operator listener.
    Admin(crate::admin_cli::AdminArgs),
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Settings {
    pub config: Option<String>,
    pub level: Option<String>,
    pub db_settings: DBSettings,
    pub api_settings: APISettings,
    pub ui_settings: UISettings,
    pub coordinator_settings: CoordinatorSettings,
    pub bitcoin_settings: BitcoinSettings,
    pub ln_settings: LnSettings,
    pub keymeld_settings: KeymeldSettings,
    #[serde(default)]
    pub admin_settings: AdminSettings,
    #[serde(default)]
    pub ark_settings: ArkSettings,
    #[serde(default)]
    pub metrics_settings: MetricsSettings,
    #[serde(default)]
    pub network_fee_settings: NetworkFeeSettings,
    #[serde(default)]
    pub cpfp_settings: CpfpSettings,
    #[serde(default)]
    pub kickoff_check_settings: KickoffCheckSettings,
    #[serde(default, rename = "recovery")]
    pub recovery_settings: RecoverySettings,
    #[serde(default, rename = "pow")]
    pub pow_settings: PowSettings,
    #[serde(default)]
    pub http_context: HttpContextSettings,
    #[serde(default)]
    pub telemetry: TelemetrySettings,
    #[serde(default, rename = "feedback")]
    pub feedback_settings: FeedbackSettings,
}

/// Environment variable that sets `http_context.trusted_proxies`, overriding the file.
pub const TRUSTED_PROXIES_ENV: &str = "COORDINATOR_TRUSTED_PROXIES";
/// Environment variable that sets `http_context.client_ip_header`, overriding the file.
pub const CLIENT_IP_HEADER_ENV: &str = "COORDINATOR_CLIENT_IP_HEADER";

/// Who may vouch for a request's client address and id. See docs/REQUEST_CONTEXT.md.
///
/// A request whose TCP peer is in `trusted_proxies` takes its client address from
/// `client_ip_header` and its request id from `X-Request-Id`. Every other request is logged
/// with the peer address and a fresh id. Nobody is trusted by default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpContextSettings {
    /// Addresses or CIDR ranges of the reverse proxies in front of the listeners.
    pub trusted_proxies: Vec<String>,
    /// The header a trusted proxy puts the client address in.
    pub client_ip_header: String,
}

impl Default for HttpContextSettings {
    fn default() -> Self {
        Self {
            trusted_proxies: Vec::new(),
            client_ip_header: String::from("X-Real-IP"),
        }
    }
}

impl HttpContextSettings {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        for proxy in &self.trusted_proxies {
            crate::api::request_context::Cidr::parse(proxy)
                .map_err(|e| anyhow!("http_context.trusted_proxies: {e}"))?;
        }
        let header = self.client_ip_header.trim();
        axum::http::HeaderName::try_from(header)
            .map_err(|_| anyhow!("http_context.client_ip_header is not a header name"))?;
        // Clients can send these themselves, and the first one may hold a list.
        if ["x-forwarded-for", "cf-connecting-ip", "forwarded"]
            .iter()
            .any(|name| header.eq_ignore_ascii_case(name))
        {
            return Err(anyhow!(
                "http_context.client_ip_header must be a header the proxy overwrites, such as X-Real-IP"
            ));
        }
        Ok(())
    }

    /// Apply `COORDINATOR_TRUSTED_PROXIES` (comma-separated; empty trusts nobody) and
    /// `COORDINATOR_CLIENT_IP_HEADER`, when set.
    pub fn apply_env_overrides(
        &mut self,
        trusted_proxies: Option<String>,
        client_ip_header: Option<String>,
    ) {
        if let Some(value) = trusted_proxies {
            self.trusted_proxies = value
                .split(',')
                .map(str::trim)
                .filter(|proxy| !proxy.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if let Some(value) = client_ip_header.filter(|value| !value.trim().is_empty()) {
            self.client_ip_header = value.trim().to_owned();
        }
    }
}

#[cfg(test)]
mod http_context_settings_tests {
    use super::*;

    #[test]
    fn nobody_is_trusted_unless_configured() {
        let settings = HttpContextSettings::default();
        assert!(settings.trusted_proxies.is_empty());
        assert_eq!(settings.client_ip_header, "X-Real-IP");
        settings.validate().unwrap();

        let text = toml::to_string(&Settings::default()).unwrap();
        let without: String = text.split("[http_context]").next().unwrap().to_string();
        let parsed: Settings = toml::from_str(&without).unwrap();
        assert_eq!(parsed.http_context, HttpContextSettings::default());
    }

    #[test]
    fn environment_overrides_the_proxies_and_header() {
        let mut settings = HttpContextSettings::default();
        settings.apply_env_overrides(
            Some("127.0.0.1, 10.0.0.0/8,".into()),
            Some("X-Client-IP".into()),
        );
        assert_eq!(settings.trusted_proxies, vec!["127.0.0.1", "10.0.0.0/8"]);
        assert_eq!(settings.client_ip_header, "X-Client-IP");
        settings.validate().unwrap();

        settings.apply_env_overrides(Some(String::new()), None);
        assert!(settings.trusted_proxies.is_empty());
        assert_eq!(settings.client_ip_header, "X-Client-IP");

        settings.trusted_proxies = vec!["proxy.local".into()];
        assert!(settings.validate().is_err());
        settings.trusted_proxies.clear();
        settings.client_ip_header = "X-Forwarded-For".into();
        assert!(settings.validate().is_err());
    }
}

/// Environment variable that sets `telemetry.enabled`, overriding the file.
pub const TELEMETRY_ENABLED_ENV: &str = "COORDINATOR_TELEMETRY_ENABLED";

/// Browser telemetry: `shared/telemetry.js` and `POST /api/v1/telemetry`. Off by default.
/// See docs/REQUEST_CONTEXT.md.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetrySettings {
    pub enabled: bool,
}

impl TelemetrySettings {
    /// Apply `COORDINATOR_TELEMETRY_ENABLED` (`true` or `false`), when set.
    pub fn apply_env_override(&mut self, value: Option<String>) -> Result<(), anyhow::Error> {
        let Some(value) = value else {
            return Ok(());
        };
        self.enabled = match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" | "" => false,
            _ => return Err(anyhow!("{TELEMETRY_ENABLED_ENV} must be true or false")),
        };
        Ok(())
    }
}

#[cfg(test)]
mod telemetry_settings_tests {
    use super::*;

    #[test]
    fn telemetry_is_off_unless_configured() {
        assert!(!TelemetrySettings::default().enabled);
        let text = toml::to_string(&Settings::default()).unwrap();
        let without: String = text.split("[telemetry]").next().unwrap().to_string();
        let parsed: Settings = toml::from_str(&without).unwrap();
        assert!(!parsed.telemetry.enabled);

        let mut settings = TelemetrySettings::default();
        settings.apply_env_override(Some("true".into())).unwrap();
        assert!(settings.enabled);
        settings.apply_env_override(None).unwrap();
        assert!(settings.enabled);
        settings.apply_env_override(Some("false".into())).unwrap();
        assert!(!settings.enabled);
        assert!(settings.apply_env_override(Some("yes".into())).is_err());
    }
}

/// Proof of work for new accounts. Off by default. See docs/REQUEST_HARDENING.md.
///
/// When enabled, every account creation (username and password, or a Nostr extension) must
/// carry a solved challenge from `POST /api/v1/users/pow`: a nonce whose SHA-256 with the
/// challenge starts with `base_bits` zero bits, plus one bit for every `step_signups` accounts
/// created in the last hour across the whole service, up to `max_bits`. The difficulty is the
/// same for every client address, so a crowd behind one address pays what anyone else does.
/// 18 bits take a phone about a second; each bit doubles it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowSettings {
    pub enabled: bool,
    pub base_bits: u8,
    pub max_bits: u8,
    pub step_signups: u64,
}

/// The most leading zero bits a proof of work may be asked for: four billion hashes on average.
pub const MAX_POW_BITS: u8 = 32;

impl Default for PowSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            base_bits: 18,
            max_bits: 22,
            step_signups: 200,
        }
    }
}

impl PowSettings {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if !self.enabled {
            return Ok(());
        }
        if self.max_bits > MAX_POW_BITS {
            return Err(anyhow!("pow.max_bits must be at most {MAX_POW_BITS}"));
        }
        if self.base_bits > self.max_bits {
            return Err(anyhow!("pow.base_bits must not exceed pow.max_bits"));
        }
        if self.step_signups == 0 {
            return Err(anyhow!("pow.step_signups must be at least 1"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod pow_settings_tests {
    use super::*;

    #[test]
    fn proof_of_work_is_off_unless_configured_and_settings_are_checked() {
        let defaults = PowSettings::default();
        assert!(!defaults.enabled);
        assert_eq!(
            (defaults.base_bits, defaults.max_bits, defaults.step_signups),
            (18, 22, 200)
        );

        // A config written before the section existed loads with it off.
        let text = toml::to_string(&Settings::default()).unwrap();
        let without: String = text.split("[pow]").next().unwrap().to_string();
        let parsed: Settings = toml::from_str(&without).unwrap();
        assert_eq!(parsed.pow_settings, PowSettings::default());

        let configured: Settings = toml::from_str(&format!(
            "{without}\n[pow]\nenabled = true\nbase_bits = 20\n"
        ))
        .unwrap();
        assert!(configured.pow_settings.enabled);
        assert_eq!(configured.pow_settings.base_bits, 20);
        assert_eq!(configured.pow_settings.max_bits, 22);
        configured.validate().unwrap();

        let enabled = PowSettings {
            enabled: true,
            ..PowSettings::default()
        };
        enabled.validate().unwrap();
        for invalid in [
            PowSettings {
                max_bits: MAX_POW_BITS + 1,
                ..enabled.clone()
            },
            PowSettings {
                base_bits: 23,
                ..enabled.clone()
            },
            PowSettings {
                step_signups: 0,
                ..enabled.clone()
            },
        ] {
            assert!(invalid.validate().is_err(), "{invalid:?}");
            // Nothing is checked while proofs are off.
            PowSettings {
                enabled: false,
                ..invalid.clone()
            }
            .validate()
            .unwrap();
            let settings = Settings {
                pow_settings: invalid,
                ..Settings::default()
            };
            assert!(settings.validate().is_err());
        }
    }
}

/// Recovery records published to Nostr relays, and the recovery file players download. Off by
/// default. See docs/RECOVERY.md.
///
/// The key in `key_file` signs and encrypts the records and is used for nothing else. It is
/// created on first start. Every coordinator of one deployment must use the same file:
/// players find their records by this key.
///
/// Once the money a record describes is settled, the record stays on the relays for
/// `settled_retention_days` and is then deleted with a NIP-09 deletion signed by the same key,
/// so the relays must accept kind 5 from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecoverySettings {
    pub enabled: bool,
    /// `wss://` relays the records go to. Empty keeps them for the recovery file only.
    pub relays: Vec<String>,
    pub key_file: String,
    /// Delete records from the relays once their money is settled. On by default.
    pub delete_settled: bool,
    /// Days a record stays on the relays after its money is settled.
    pub settled_retention_days: u32,
}

/// The longest `settled_retention_days` allowed: ten years.
pub const MAX_SETTLED_RETENTION_DAYS: u32 = 3650;

impl Default for RecoverySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            relays: Vec::new(),
            key_file: String::from("./creds/coordinator_recovery_key.pem"),
            delete_settled: true,
            settled_retention_days: 7,
        }
    }
}

impl RecoverySettings {
    pub fn validate(&self, coordinator: &CoordinatorSettings) -> Result<(), anyhow::Error> {
        if !self.enabled {
            return Ok(());
        }
        if self.key_file == coordinator.private_key_file {
            return Err(anyhow!(
                "recovery.key_file must be its own key, not coordinator_settings.private_key_file"
            ));
        }
        for relay in &self.relays {
            let url = nostr::Url::parse(relay)
                .map_err(|e| anyhow!("recovery relay {relay} is not a URL: {e}"))?;
            if !matches!(url.scheme(), "wss" | "ws") || url.host_str().is_none() {
                return Err(anyhow!(
                    "recovery relay {relay} must be a ws:// or wss:// URL"
                ));
            }
        }
        if self.settled_retention_days > MAX_SETTLED_RETENTION_DAYS {
            return Err(anyhow!(
                "recovery.settled_retention_days must be at most {MAX_SETTLED_RETENTION_DAYS}"
            ));
        }
        Ok(())
    }

    /// When the publisher deletes records whose money is settled.
    pub fn retention(&self) -> crate::domain::recovery::Retention {
        crate::domain::recovery::Retention {
            delete_settled: self.delete_settled,
            grace_secs: i64::from(self.settled_retention_days) * 24 * 60 * 60,
        }
    }
}

/// Environment variable that sets `metrics_settings.listen_addr`, overriding the file.
pub const METRICS_LISTEN_ADDR_ENV: &str = "COORDINATOR_METRICS_LISTEN_ADDR";

/// Prometheus metrics listener, serving only `GET /metrics`.
///
/// Unset by default: no metrics listener runs. Metrics never share the public listener,
/// which a public gateway proxies in full. The listener has no authentication, so bind it
/// to loopback or a private network that only the scraper reaches.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSettings {
    /// Socket address of the metrics listener, for example "127.0.0.1:9992".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_addr: Option<SocketAddr>,
}

impl MetricsSettings {
    /// Apply the value of `COORDINATOR_METRICS_LISTEN_ADDR`, when set. An empty value
    /// turns the listener off.
    pub fn apply_env_override(&mut self, value: Option<String>) -> Result<(), anyhow::Error> {
        let Some(value) = value else {
            return Ok(());
        };
        let value = value.trim();
        self.listen_addr = if value.is_empty() {
            None
        } else {
            Some(value.parse().map_err(|e| {
                anyhow!("{METRICS_LISTEN_ADDR_ENV} must be a socket address such as 127.0.0.1:9992: {e}")
            })?)
        };
        Ok(())
    }
}

#[cfg(test)]
mod metrics_settings_tests {
    use super::*;

    #[test]
    fn metrics_listener_is_off_unless_configured() {
        assert_eq!(MetricsSettings::default().listen_addr, None);

        // A config written before this setting existed loads with the listener off.
        let text = toml::to_string(&Settings::default()).unwrap();
        let without_metrics: String = text.split("[metrics_settings]").next().unwrap().to_string();
        let parsed: Settings = toml::from_str(&without_metrics).unwrap();
        assert_eq!(parsed.metrics_settings.listen_addr, None);

        let configured: MetricsSettings =
            toml::from_str("listen_addr = \"127.0.0.1:9992\"").unwrap();
        assert_eq!(
            configured.listen_addr,
            Some("127.0.0.1:9992".parse().unwrap())
        );
    }

    #[test]
    fn environment_overrides_the_metrics_listener() {
        let mut settings = MetricsSettings::default();
        settings.apply_env_override(None).unwrap();
        assert_eq!(settings.listen_addr, None);

        settings
            .apply_env_override(Some("127.0.0.1:9992".into()))
            .unwrap();
        assert_eq!(
            settings.listen_addr,
            Some("127.0.0.1:9992".parse().unwrap())
        );

        settings.apply_env_override(Some(" ".into())).unwrap();
        assert_eq!(settings.listen_addr, None);

        assert!(settings
            .apply_env_override(Some("not an address".into()))
            .is_err());
    }
}

/// Each ticket's share of the Bitcoin network fees, added to its price as its own line.
///
/// A game's chain cost is `base_vbytes + vbytes_per_player × players` vbytes, plus the anchor
/// output new contracts put on their outcome transaction (its vbytes and value). Each entry pays
/// its share of that for a pool of `pool_players`, at the current estimate for `conf_target`
/// blocks (at least `min_sat_per_vb`) times `multiplier_percent`. The fee is fixed on a ticket
/// when it is issued; the coordinator keeps any surplus and absorbs any shortfall. While the fee
/// would be more than `pause_above_entry_bps` of the entry fee, no ticket is issued (0: never).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkFeeSettings {
    pub enabled: bool,
    pub pool_players: u64,
    pub multiplier_percent: u64,
    pub base_vbytes: u64,
    pub vbytes_per_player: u64,
    pub conf_target: u16,
    pub min_sat_per_vb: u64,
    pub pause_above_entry_bps: u64,
}

impl Default for NetworkFeeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            pool_players: 5,
            multiplier_percent: 150,
            base_vbytes: 342,
            vbytes_per_player: 26,
            conf_target: 2,
            min_sat_per_vb: 1,
            pause_above_entry_bps: 1_000,
        }
    }
}

impl NetworkFeeSettings {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if !self.enabled {
            return Ok(());
        }
        if self.pool_players == 0
            || self.conf_target == 0
            || self.min_sat_per_vb == 0
            || self.multiplier_percent == 0
        {
            return Err(anyhow::anyhow!(
                "network_fee_settings.pool_players, multiplier_percent, conf_target and \
                 min_sat_per_vb must be at least 1"
            ));
        }
        if self.base_vbytes == 0 && self.vbytes_per_player == 0 {
            return Err(anyhow::anyhow!(
                "network_fee_settings needs base_vbytes or vbytes_per_player"
            ));
        }
        // The largest fee it can compute must not overflow, at any rate LND could report.
        crate::domain::network_fee_sats(self, 1_000_000.0)?;
        Ok(())
    }

    /// Whether a ticket with `network_fee_sats` for an `entry_fee_sats` entry is refused: the fee
    /// is more than `pause_above_entry_bps` of the entry.
    pub fn pauses(&self, network_fee_sats: u64, entry_fee_sats: u64) -> bool {
        self.enabled
            && self.pause_above_entry_bps > 0
            && u128::from(network_fee_sats) * 10_000
                > u128::from(entry_fee_sats) * u128::from(self.pause_above_entry_bps)
    }
}

/// Fee-bumping the coordinator's own settlement transactions through their pay-to-anchor
/// outputs: the outcome, expiry and split transactions of contracts built since dlctix 0.2.
/// Contracts without anchors are left as they are. See docs/RECOVERY.md, "Fee bumping".
///
/// A transaction the coordinator broadcast that has waited `after_secs` without confirming, and
/// pays less than the local estimate for `conf_target` blocks, gets a child that spends its
/// anchor and one confirmed coin of the LND wallet, so the two pay the estimate. Near a deadline
/// the target is `urgent_conf_target`: an attested outcome transaction within
/// `urgent_within_secs` of the contract's expiry, after which the pre-signed expiry transaction
/// is valid and could take its place. A transaction is bumped again only once the estimate is a
/// quarter above its last child's rate, and no child pays more than `max_fee_percent` of what
/// its parent spends; past that the child pays what the budget allows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CpfpSettings {
    pub enabled: bool,
    pub after_secs: u64,
    pub conf_target: u16,
    pub urgent_conf_target: u16,
    pub urgent_within_secs: u64,
    pub max_fee_percent: u64,
}

impl Default for CpfpSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            after_secs: 1_800,
            conf_target: 6,
            urgent_conf_target: 1,
            urgent_within_secs: 6 * 3_600,
            max_fee_percent: 10,
        }
    }
}

impl CpfpSettings {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if self.conf_target == 0 || self.urgent_conf_target == 0 {
            return Err(anyhow!(
                "cpfp_settings confirmation targets must be at least one block"
            ));
        }
        if self.urgent_conf_target > self.conf_target {
            return Err(anyhow!(
                "cpfp_settings.urgent_conf_target must not exceed conf_target"
            ));
        }
        if !(1..=100).contains(&self.max_fee_percent) {
            return Err(anyhow!(
                "cpfp_settings.max_fee_percent must be between 1 and 100"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod cpfp_settings_tests {
    use super::*;

    #[test]
    fn defaults_bump_and_settings_are_checked() {
        let defaults = CpfpSettings::default();
        assert!(defaults.enabled);
        defaults.validate().unwrap();
        let parsed: CpfpSettings = toml::from_str("enabled = false\nconf_target = 3").unwrap();
        assert!(!parsed.enabled);
        assert_eq!(parsed.conf_target, 3);
        assert_eq!(parsed.max_fee_percent, defaults.max_fee_percent);
        for invalid in [
            CpfpSettings {
                conf_target: 0,
                ..CpfpSettings::default()
            },
            CpfpSettings {
                urgent_conf_target: 12,
                ..CpfpSettings::default()
            },
            CpfpSettings {
                max_fee_percent: 0,
                ..CpfpSettings::default()
            },
            CpfpSettings {
                max_fee_percent: 101,
                ..CpfpSettings::default()
            },
        ] {
            assert!(invalid.validate().is_err(), "{invalid:?}");
        }
    }
}

/// The check an Arkade pool passes before its contract is built: what its entries paid beyond
/// the pot, service and network fees, must cover the game's chain cost at the kickoff fee rate
/// (the weights in `network_fee_settings`) plus `routing_and_liquidity_bps` of the pot, and the
/// rate must be within the fee ceiling the players consented to. A pool also needs
/// `min_players` unless the kickoff rate is at most `small_pools_max_sat_per_vb`, when its
/// terms' own minimum holds. A pool that fails checks again until `fee_wait_secs` after
/// registration closes, and is then cancelled and every entry refunded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KickoffCheckSettings {
    pub enabled: bool,
    pub routing_and_liquidity_bps: u64,
    pub min_players: u64,
    pub small_pools_max_sat_per_vb: u64,
    /// How long after registration closes a failing pool waits for fees to fall before it is
    /// cancelled. Every way the check fails depends on the fee rate, so a pool checks again until
    /// then.
    pub fee_wait_secs: u64,
}

impl Default for KickoffCheckSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            routing_and_liquidity_bps: 50,
            min_players: 5,
            small_pools_max_sat_per_vb: 2,
            fee_wait_secs: 3600,
        }
    }
}

impl KickoffCheckSettings {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if self.routing_and_liquidity_bps > 10_000 {
            return Err(anyhow::anyhow!(
                "kickoff_check_settings.routing_and_liquidity_bps exceeds the pot"
            ));
        }
        Ok(())
    }

    /// The fewest players a pool may start with at `sat_per_vb`: its terms' `template_min` while
    /// fees are at most `small_pools_max_sat_per_vb`, otherwise at least `min_players`.
    pub fn min_players_at(&self, template_min: u64, sat_per_vb: u64) -> u64 {
        if !self.enabled || sat_per_vb <= self.small_pools_max_sat_per_vb {
            template_min
        } else {
            template_min.max(self.min_players)
        }
    }
}

/// Arkade funding: each entry's buy-in is swapped into an escrow VTXO, and a competition's pool
/// is funded in one Arkade batch. See `docs/QUEUED_COMPETITIONS.md`.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ArkSettings {
    /// Fund new competitions from Arkade escrows. Needs Keymeld with automatic payouts.
    #[serde(default)]
    pub enabled: bool,
    /// arkd's URL, for example `https://mutinynet.arkade.sh`.
    #[serde(default)]
    pub server_url: String,
    /// `ark-swapd`'s URL.
    #[serde(default)]
    pub swap_url: String,
    /// Optional public Arkade explorer for the server's network. A player's entries link their
    /// escrow VTXO and refund transaction to `<url>/tx/<txid>`; without one they show the IDs
    /// to copy.
    #[serde(default)]
    pub explorer_url: Option<String>,
    /// A file holding `ark-swapd`'s bearer token.
    #[serde(default)]
    pub swap_token_file: String,
    /// How long after the observation window starts an escrow's refund leaf opens.
    ///
    /// Short on purpose. An escrow VTXO inherits the expiry of the coins that paid it, and an
    /// expired VTXO cannot be refunded offchain, so the refund must open well inside the coin's
    /// life. It does not need to outlast the kickoff: refunds run only for a competition that
    /// will never kick off (`docs/QUEUED_COMPETITIONS.md`, "Refunds"). It is part of the escrow's
    /// script, so it applies to escrows issued from then on.
    #[serde(default = "default_refund_after_start_secs")]
    pub refund_after_start_secs: u64,
    /// How much longer than its refund locktime an escrow's VTXO must live, as Arkade lists it
    /// when the ticket's payment is confirmed. A ticket paid with a shorter-lived coin is not
    /// counted: its escrow could expire before a refund finishes.
    #[serde(default = "default_escrow_expiry_margin_secs")]
    pub escrow_expiry_margin_secs: u64,
    /// The most the swap service may keep from a refunded escrow for paying the player's
    /// Lightning Address. The player consents to this cap when entering.
    #[serde(default = "default_max_refund_fee_sats")]
    pub max_refund_fee_sats: u64,
    /// How long after the Arkade server fails a batch step entries stay paused when nothing
    /// succeeds after it. The next success lifts the pause sooner.
    #[serde(default = "default_arkade_outage_secs")]
    pub arkade_outage_secs: u64,
}

/// 45 minutes: a kickoff runs within minutes of the start, and a pool waits at most an hour for
/// fees to fall before it is cancelled.
pub const DEFAULT_REFUND_AFTER_START_SECS: u64 = 45 * 60;

/// Six hours: time for the competition to fail, the chain's time to pass the locktime, and the
/// refund to be signed and paid.
pub const DEFAULT_ESCROW_EXPIRY_MARGIN_SECS: u64 = 6 * 60 * 60;

fn default_refund_after_start_secs() -> u64 {
    DEFAULT_REFUND_AFTER_START_SECS
}

fn default_escrow_expiry_margin_secs() -> u64 {
    DEFAULT_ESCROW_EXPIRY_MARGIN_SECS
}

fn default_max_refund_fee_sats() -> u64 {
    100
}

fn default_arkade_outage_secs() -> u64 {
    crate::domain::DEFAULT_ARKADE_OUTAGE_SECS
}

impl ArkSettings {
    pub fn validate(&self, keymeld: &KeymeldSettings) -> Result<(), anyhow::Error> {
        if !self.enabled {
            return Ok(());
        }
        if !keymeld.enabled || !keymeld.automatic_payouts {
            return Err(anyhow::anyhow!(
                "Arkade funding needs Keymeld with automatic payouts: the escrow consent is part of the payout policy"
            ));
        }
        if self.server_url.is_empty() || self.swap_url.is_empty() || self.swap_token_file.is_empty()
        {
            return Err(anyhow::anyhow!(
                "Arkade funding needs server_url, swap_url, and swap_token_file"
            ));
        }
        Ok(())
    }

    /// `ark-swapd`'s bearer token.
    pub fn swap_token(&self) -> Result<String, anyhow::Error> {
        let token = std::fs::read_to_string(&self.swap_token_file)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", self.swap_token_file))?;
        Ok(token.trim().to_owned())
    }
}

impl ConfigurableSettings for Settings {
    fn apply_cli_overrides(&mut self, cli_settings: &CliSettings) {
        if let Some(level) = &cli_settings.level {
            self.level = Some(level.clone());
        }
    }

    fn default_config_path() -> PathBuf {
        PathBuf::from("./config/local.toml")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DBSettings {
    pub data_folder: String,
    pub read_max_connections: u32,
    pub read_min_connections: u32,
    pub write_max_connections: u32,
    pub write_min_connections: u32,
    pub idle_timeout_secs: u64,
    pub acquire_timeout_secs: u64,
    pub sqlite_config: SqliteConfigSerde,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqliteConfigSerde {
    pub mode: String,
    pub cache: String,
    pub busy_timeout_ms: u32,
    pub journal_mode: String,
    pub synchronous: String,
    pub cache_size: i32,
    pub foreign_keys: bool,
    pub wal_autocheckpoint: Option<u32>,
    pub temp_store: String,
    pub mmap_size: Option<u64>,
    pub page_size: Option<u32>,
}

impl Default for DBSettings {
    fn default() -> Self {
        DBSettings {
            data_folder: String::from("./data"),
            read_max_connections: 12,
            read_min_connections: 2,
            write_max_connections: 1,
            write_min_connections: 1,
            idle_timeout_secs: 600,   // 10 minutes
            acquire_timeout_secs: 15, // 15 seconds
            sqlite_config: SqliteConfigSerde::default(),
        }
    }
}

impl Default for SqliteConfigSerde {
    fn default() -> Self {
        Self {
            mode: "ReadWriteCreate".to_string(),
            cache: "Private".to_string(),
            busy_timeout_ms: 5000,
            journal_mode: "WAL".to_string(),
            synchronous: "NORMAL".to_string(),
            cache_size: 1000000,
            foreign_keys: true,
            wal_autocheckpoint: Some(1000),
            temp_store: "Memory".to_string(),
            mmap_size: Some(268435456), // 256MB
            page_size: Some(4096),
        }
    }
}

impl SqliteConfigSerde {
    pub fn development() -> Self {
        Self {
            busy_timeout_ms: 10000,
            cache_size: 100000,
            ..Default::default()
        }
    }

    pub fn production() -> Self {
        Self {
            synchronous: "FULL".to_string(),
            cache_size: 2000000,
            wal_autocheckpoint: Some(10000),
            mmap_size: Some(1073741824), // 1GB
            ..Default::default()
        }
    }

    pub fn testing() -> Self {
        Self {
            mode: "Memory".to_string(),
            journal_mode: "MEMORY".to_string(),
            synchronous: "OFF".to_string(),
            temp_store: "Memory".to_string(),
            busy_timeout_ms: 1000,
            cache_size: 10000,
            wal_autocheckpoint: None,
            mmap_size: None,
            page_size: None,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LnSettings {
    /// Url to find the lnd lightning node's REST api
    pub base_url: String,
    /// File path to the lnd macaroon that has the needed permissions
    pub macaroon_file_path: String,
    /// Optional file path to the lnd tls cert (typically only used in local development, with self signed certs)
    pub tls_cert_path: Option<String>,
    /// Accept any TLS certificate from LND. Only for local development against a
    /// self-signed cert without matching SANs; refused on mainnet, because the
    /// admin macaroon travels on this connection.
    #[serde(default)]
    pub dangerous_accept_invalid_tls: bool,
    /// Interval in seconds to check for new invoices while the invoice subscription is down
    pub invoice_watch_interval: u64,
    /// Interval in seconds to check for new invoices while the invoice subscription is up;
    /// the check only reconciles events the subscription missed
    #[serde(default = "default_watch_interval_subscribed")]
    pub invoice_watch_interval_subscribed: u64,
    /// Interval in seconds to check for new payouts while the payment subscription is down
    pub payout_watch_interval: u64,
    /// Interval in seconds to check for new payouts while the payment subscription is up;
    /// the check only reconciles events the subscription missed
    #[serde(default = "default_watch_interval_subscribed")]
    pub payout_watch_interval_subscribed: u64,
    /// Enable mock LN client for E2E testing (no real LND required)
    #[serde(default)]
    pub mock_enabled: bool,
    /// Auto-accept invoices after this many seconds (only when mock_enabled=true)
    /// If not set, invoices must be manually accepted via test endpoints
    #[serde(default)]
    pub mock_auto_accept_secs: Option<u64>,
}

impl Default for LnSettings {
    fn default() -> Self {
        LnSettings {
            base_url: String::from("https://localhost:9095"),
            macaroon_file_path: String::from("./creds/admin.macaroon"),
            tls_cert_path: Some(String::from("./creds/tls.cert")),
            dangerous_accept_invalid_tls: false,
            invoice_watch_interval: 5,
            invoice_watch_interval_subscribed: default_watch_interval_subscribed(),
            payout_watch_interval: 5,
            payout_watch_interval_subscribed: default_watch_interval_subscribed(),
            mock_enabled: false,
            mock_auto_accept_secs: None,
        }
    }
}

fn default_watch_interval_subscribed() -> u64 {
    60
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeymeldSettings {
    /// Require payout escrow for new competitions. Existing competitions retain their original protocol.
    #[serde(default)]
    pub automatic_payouts: bool,
    /// Fee ceiling signed by entrants before they pay for a ticket.
    #[serde(default = "default_automatic_payout_fee_ceiling")]
    pub automatic_payout_max_fee_rate_sat_vb: u64,
    /// Trusted Nitro PCR measurements from the reviewed enclave build; PCR0 or PCR8 is required when enabled.
    #[serde(default, with = "pcr_measurements")]
    pub trusted_pcrs: BTreeMap<u16, String>,
    /// URL of the Keymeld gateway server
    pub gateway_url: String,
    /// Gateway URL reachable by browsers for fresh enclave attestation.
    #[serde(default)]
    pub public_gateway_url: Option<String>,
    /// Trust simulated enclaves that produce no Nitro attestation, as in local
    /// development and Moto-backed staging. Refused on mainnet and alongside
    /// pinned measurements; forwarded to browsers in the ticket response.
    #[serde(default)]
    pub dangerous_trust_unattested_enclaves: bool,
    /// Whether Keymeld integration is enabled
    pub enabled: bool,
    /// Expiration time in seconds for keygen sessions
    pub keygen_session_expiry_secs: u64,
    /// Expiration time in seconds for signing sessions
    pub signing_session_expiry_secs: u64,
    /// Maximum polling attempts for session completion
    pub max_polling_attempts: u32,
    /// Initial polling delay in milliseconds
    pub initial_polling_delay_ms: u64,
    /// Maximum polling delay in milliseconds
    pub max_polling_delay_ms: u64,
    /// Polling backoff multiplier
    pub polling_backoff_multiplier: f64,
}

fn default_automatic_payout_fee_ceiling() -> u64 {
    100
}

impl Default for KeymeldSettings {
    fn default() -> Self {
        KeymeldSettings {
            automatic_payouts: false,
            automatic_payout_max_fee_rate_sat_vb: default_automatic_payout_fee_ceiling(),
            trusted_pcrs: BTreeMap::new(),
            gateway_url: String::from("http://localhost:8080"),
            public_gateway_url: None,
            dangerous_trust_unattested_enclaves: false,
            enabled: false,
            keygen_session_expiry_secs: 3600,
            signing_session_expiry_secs: 300,
            max_polling_attempts: 60,
            initial_polling_delay_ms: 500,
            max_polling_delay_ms: 5000,
            polling_backoff_multiplier: 1.5,
        }
    }
}

impl Settings {
    /// Reject configurations that are unsafe with real funds or that expose
    /// operator routes without authentication.
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        let network = self.bitcoin_settings.network;
        self.api_settings.validate(&self.ui_settings)?;
        self.ui_settings.validate(network)?;
        self.ln_settings.validate(network)?;
        self.coordinator_settings.validate(network)?;
        self.admin_settings.validate(network)?;
        self.ark_settings.validate(&self.keymeld_settings)?;
        self.network_fee_settings.validate()?;
        self.kickoff_check_settings.validate()?;
        self.recovery_settings
            .validate(&self.coordinator_settings)?;
        self.pow_settings.validate()?;
        self.http_context.validate()?;
        self.feedback_settings.validate()?;
        self.admin_settings.logs.validate()?;
        self.keymeld_settings.validate(network)
    }
}

impl APISettings {
    pub fn validate(&self, ui: &UISettings) -> Result<(), anyhow::Error> {
        crate::api::nip98_origins::Nip98Origins::new(
            self.origins
                .iter()
                .map(String::as_str)
                .chain([ui.remote_url.as_str(), ui.private_url.as_str()]),
        )?;
        let limits = &self.rate_limit;
        if limits.enabled
            && [
                limits.per_second,
                limits.burst,
                limits.auth_per_second,
                limits.auth_burst,
            ]
            .contains(&0)
        {
            anyhow::bail!("api_settings.rate_limit rates and bursts must be at least 1");
        }
        if self.replay_capacity == 0 {
            anyhow::bail!("api_settings.replay_capacity must be at least 1");
        }
        Ok(())
    }
}

impl LnSettings {
    pub fn validate(&self, network: Network) -> Result<(), anyhow::Error> {
        if !self.mock_enabled
            && network == Network::Bitcoin
            && reqwest::Url::parse(&self.base_url)?.scheme() != "https"
        {
            return Err(anyhow::anyhow!(
                "ln_settings.base_url must use HTTPS on mainnet"
            ));
        }
        if self.dangerous_accept_invalid_tls && network == Network::Bitcoin {
            return Err(anyhow::anyhow!(
                "ln_settings.dangerous_accept_invalid_tls is refused on mainnet"
            ));
        }
        Ok(())
    }
}

impl CoordinatorSettings {
    /// Apply the value of `COORDINATOR_SETTLE_ONLY`, when set: `true` or `1` turns settle-only
    /// mode on, `false` or `0` off, and an empty value leaves the file's setting.
    pub fn apply_settle_only_env(&mut self, value: Option<String>) -> Result<(), anyhow::Error> {
        let Some(value) = value else {
            return Ok(());
        };
        self.settle_only = match value.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(()),
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => {
                return Err(anyhow!(
                    "{SETTLE_ONLY_ENV} must be true or false, not {other:?}"
                ))
            }
        };
        Ok(())
    }

    pub fn validate(&self, network: Network) -> Result<(), anyhow::Error> {
        if !(1..=coordinator_escrow::capacity::MAX_COMPETITION_WINNING_PLACES)
            .contains(&self.max_winning_places)
        {
            return Err(anyhow::anyhow!(
                "coordinator_settings.max_winning_places must be 1 or 2"
            ));
        }
        if self.escrow_enabled && network == Network::Bitcoin {
            return Err(anyhow::anyhow!(
                "coordinator_settings.escrow_enabled is refused on mainnet until the escrow flow has been exercised end to end on a test network"
            ));
        }
        Ok(())
    }
}

/// Operator listener serving `/admin/*`, the wallet API, and competition creation.
///
/// Operator routes never share the public listener. Keep `listen_addr` on loopback or a
/// private network and reach it through a tunnel or an operator-only gateway name.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdminSettings {
    /// Optional server-side Grafana signals for the operator dashboard.
    #[serde(default)]
    pub monitoring: Option<crate::infra::admin_monitoring::MonitoringSettings>,
    /// Socket address of the operator listener, for example "127.0.0.1:9991".
    pub listen_addr: SocketAddr,
    /// File holding the operator bearer token: at least 32 characters, surrounding
    /// whitespace ignored. The coordinator refuses to start when it is missing or empty.
    pub token_file: String,
    /// Serve operator routes without authentication, for local development only.
    /// Refused on mainnet and on non-loopback addresses.
    #[serde(default)]
    pub dangerous_allow_unauthenticated: bool,
    /// Where the Visitors page reads visitor logs.
    #[serde(default)]
    pub logs: LogsSettings,
}

impl Default for AdminSettings {
    fn default() -> Self {
        AdminSettings {
            monitoring: None,
            listen_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 9991)),
            token_file: String::from("./creds/admin_token"),
            dangerous_allow_unauthenticated: false,
            logs: LogsSettings::default(),
        }
    }
}

/// Operator listener settings that must stop startup.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AdminSettingsError {
    #[error("admin_settings.dangerous_allow_unauthenticated is refused on mainnet")]
    UnauthenticatedOnMainnet,
    #[error(
        "admin_settings.dangerous_allow_unauthenticated requires a loopback listen_addr, not {0}"
    )]
    UnauthenticatedOffLoopback(SocketAddr),
}

impl AdminSettings {
    pub fn validate(&self, network: Network) -> Result<(), AdminSettingsError> {
        if !self.dangerous_allow_unauthenticated {
            return Ok(());
        }
        if network == Network::Bitcoin {
            return Err(AdminSettingsError::UnauthenticatedOnMainnet);
        }
        if !self.listen_addr.ip().is_loopback() {
            return Err(AdminSettingsError::UnauthenticatedOffLoopback(
                self.listen_addr,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod admin_settings_tests {
    use super::*;

    #[test]
    fn operator_listener_defaults_to_loopback_with_authentication() {
        let settings = AdminSettings::default();
        assert!(settings.listen_addr.ip().is_loopback());
        assert!(!settings.dangerous_allow_unauthenticated);
        assert_eq!(settings.validate(Network::Bitcoin), Ok(()));
        assert_eq!(
            UISettings::default().private_url,
            format!("http://{}", settings.listen_addr)
        );
    }

    #[test]
    fn configs_without_admin_section_load_the_authenticated_default() {
        let text = toml::to_string(&Settings::default()).unwrap();
        let without_admin: String = text.split("[admin_settings]").next().unwrap().to_string();
        let parsed: Settings = toml::from_str(&without_admin).unwrap();
        assert_eq!(
            parsed.admin_settings.listen_addr,
            AdminSettings::default().listen_addr
        );
        assert!(!parsed.admin_settings.dangerous_allow_unauthenticated);
    }

    #[test]
    fn unauthenticated_operator_routes_are_refused_on_mainnet_and_off_loopback() {
        let unauthenticated = AdminSettings {
            dangerous_allow_unauthenticated: true,
            ..AdminSettings::default()
        };
        assert_eq!(unauthenticated.validate(Network::Regtest), Ok(()));
        assert_eq!(unauthenticated.validate(Network::Signet), Ok(()));
        assert_eq!(
            unauthenticated.validate(Network::Bitcoin),
            Err(AdminSettingsError::UnauthenticatedOnMainnet)
        );

        let exposed = AdminSettings {
            listen_addr: "0.0.0.0:9991".parse().unwrap(),
            ..unauthenticated
        };
        assert_eq!(
            exposed.validate(Network::Regtest),
            Err(AdminSettingsError::UnauthenticatedOffLoopback(
                exposed.listen_addr
            ))
        );

        let settings = Settings {
            admin_settings: exposed,
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }
}

impl KeymeldSettings {
    /// The gateway browsers call for fresh enclave attestation: the public
    /// URL when one is set, otherwise the one the coordinator uses.
    pub fn browser_gateway_url(&self) -> &str {
        self.public_gateway_url
            .as_deref()
            .unwrap_or(&self.gateway_url)
    }

    pub fn validate(&self, network: Network) -> Result<(), anyhow::Error> {
        if self.automatic_payouts && !self.enabled {
            return Err(anyhow::anyhow!("Automatic payouts require Keymeld"));
        }
        if self.automatic_payout_max_fee_rate_sat_vb == 0
            || bitcoin::FeeRate::from_sat_per_vb(self.automatic_payout_max_fee_rate_sat_vb)
                .is_none()
        {
            return Err(anyhow::anyhow!("Invalid automatic payout fee ceiling"));
        }
        if !self.enabled {
            return Ok(());
        }
        if self.dangerous_trust_unattested_enclaves {
            if network == Network::Bitcoin {
                return Err(anyhow::anyhow!(
                    "keymeld_settings.dangerous_trust_unattested_enclaves is refused on mainnet"
                ));
            }
            if !self.trusted_pcrs.is_empty() {
                return Err(anyhow::anyhow!(
                    "keymeld_settings.trusted_pcrs must be empty when trusting unattested enclaves"
                ));
            }
        } else if self.trusted_pcrs.is_empty() {
            return Err(anyhow::anyhow!(
                "keymeld_settings.trusted_pcrs must pin PCR0 or PCR8; set dangerous_trust_unattested_enclaves only for simulated enclaves"
            ));
        }
        Ok(())
    }
}

mod pcr_measurements {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S>(values: &BTreeMap<u16, String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        values
            .iter()
            .map(|(index, value)| (index.to_string(), value))
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<u16, String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        BTreeMap::<String, String>::deserialize(deserializer)?
            .into_iter()
            .map(|(index, value)| {
                index
                    .parse()
                    .map(|index| (index, value))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[cfg(test)]
mod keymeld_config_tests {
    use super::*;

    #[test]
    fn trusted_measurements_round_trip_through_operator_toml() {
        let settings = KeymeldSettings {
            trusted_pcrs: BTreeMap::from([(0, "ab".repeat(48)), (8, "cd".repeat(48))]),
            public_gateway_url: Some("https://keymeld.example.com".into()),
            ..KeymeldSettings::default()
        };
        let text = toml::to_string(&settings).unwrap();
        let parsed: KeymeldSettings = toml::from_str(&text).unwrap();
        assert_eq!(parsed.trusted_pcrs, settings.trusted_pcrs);
        assert_eq!(parsed.public_gateway_url, settings.public_gateway_url);
    }

    #[test]
    fn browsers_use_the_public_gateway_when_there_is_one() {
        let mut settings = KeymeldSettings {
            gateway_url: "http://keymeld.internal:8080".into(),
            ..KeymeldSettings::default()
        };
        assert_eq!(
            settings.browser_gateway_url(),
            "http://keymeld.internal:8080"
        );
        settings.public_gateway_url = Some("https://keymeld.example.com".into());
        assert_eq!(
            settings.browser_gateway_url(),
            "https://keymeld.example.com"
        );
    }

    #[test]
    fn simulation_trust_is_explicit_and_refused_on_mainnet_or_with_pins() {
        let simulation = KeymeldSettings {
            enabled: true,
            dangerous_trust_unattested_enclaves: true,
            ..KeymeldSettings::default()
        };
        let parsed: KeymeldSettings =
            toml::from_str(&toml::to_string(&simulation).unwrap()).unwrap();
        assert!(parsed.dangerous_trust_unattested_enclaves);
        let mut without_flag = toml::to_string(&simulation).unwrap();
        without_flag = without_flag
            .lines()
            .filter(|line| !line.starts_with("dangerous_trust_unattested_enclaves"))
            .collect::<Vec<_>>()
            .join("\n");
        let legacy: KeymeldSettings = toml::from_str(&without_flag).unwrap();
        assert!(!legacy.dangerous_trust_unattested_enclaves);

        assert!(simulation.validate(Network::Regtest).is_ok());
        assert!(simulation.validate(Network::Signet).is_ok());
        assert!(simulation.validate(Network::Bitcoin).is_err());
        let pinned_and_unattested = KeymeldSettings {
            trusted_pcrs: BTreeMap::from([(0, "ab".repeat(48))]),
            ..simulation.clone()
        };
        assert!(pinned_and_unattested.validate(Network::Regtest).is_err());

        let unpinned = KeymeldSettings {
            enabled: true,
            ..KeymeldSettings::default()
        };
        assert!(unpinned.validate(Network::Regtest).is_err());
        let pinned = KeymeldSettings {
            trusted_pcrs: BTreeMap::from([(0, "ab".repeat(48))]),
            ..unpinned
        };
        assert!(pinned.validate(Network::Bitcoin).is_ok());
        assert!(KeymeldSettings::default()
            .validate(Network::Bitcoin)
            .is_ok());
        let settings = Settings {
            keymeld_settings: simulation,
            ..Settings::default()
        };
        assert!(settings.validate().is_ok());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BitcoinSettings {
    /// On-chain network to use
    pub network: Network,
    /// Electrum server (electrs) used for chain lookups LND cannot answer,
    /// such as escrow and outcome transactions: "tcp://host:50001" or "ssl://host:50002"
    pub electrum_url: String,
    /// Optional public block explorer, such as `https://mempool.space`. The admin pages link
    /// transactions to `<url>/tx/<txid>`, and so do players' entries and leaderboards for a
    /// contract's funding transaction, so it must be reachable by players.
    #[serde(default)]
    pub explorer_url: Option<String>,
    /// Path to the coordinator's private key (can be the same as the nostr private key file).
    /// It signs DLC escrow inputs and nostr events; the on-chain wallet itself lives in LND.
    pub seed_path: String,
    /// Frequency in seconds for how often to refresh block data with on-chain
    /// (usually want to set to half as often as a block on average will come in, 10min block time -> refresh every 5min)
    pub refresh_blocks_secs: u64,
    /// Frequency in seconds of the same refresh while the Electrum block header
    /// subscription is up; a new block also triggers a refresh at once
    #[serde(default = "default_refresh_blocks_secs_subscribed")]
    pub refresh_blocks_secs_subscribed: u64,
    /// Enable mock Bitcoin client for E2E testing (no real Bitcoin infrastructure required)
    #[serde(default)]
    pub mock_enabled: bool,
}

fn default_refresh_blocks_secs_subscribed() -> u64 {
    120
}

impl Default for BitcoinSettings {
    fn default() -> Self {
        BitcoinSettings {
            network: Network::Regtest,
            electrum_url: String::from("tcp://127.0.0.1:50001"),
            explorer_url: None,
            seed_path: String::from("./creds/coordinator_private_key.pem"),
            refresh_blocks_secs: 15,
            refresh_blocks_secs_subscribed: default_refresh_blocks_secs_subscribed(),
            mock_enabled: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoordinatorSettings {
    pub name: String,
    pub oracle_url: String,
    /// Key to use to sign nostr notes and auth, may also be used for the bitcoin private key
    /// The service will generate one for the bitcoin wallet and use as the signing key for nostr by default
    pub private_key_file: String,

    /// A reasonable number of blocks within which a transaction can confirm.
    /// Used for enforcing relative locktime timeout spending conditions.
    /// We keep this the same for all competitions so it is a known behavior to the users/players
    /// Default is 432 blocks or about 72 hours on mainnet
    /// Reasonable values are:
    ///
    /// - `72`:  ~12 hours
    /// - `144`: ~24 hours
    /// - `432`: ~72 hours
    /// - `1008`: ~1 week
    ///
    /// It is also the window for paying winners over Lightning: a payout HTLC must expire
    /// inside it, and each hop of the route claims about 80 blocks of that, so 144 only
    /// reaches winners one hop away while 432 reaches most of the network.
    pub relative_locktime_block_delta: u16,

    /// The number of confirmations required for a transaction to be considered confirmed
    /// by the coordinator system
    pub required_confirmations: u32,
    /// The longest a waiting competition sleeps before it is checked again. Events such as a
    /// paid ticket wake it sooner.
    pub sync_interval_secs: u64,

    /// How often the competition runners' sweep restarts missing runners and cleans up dead
    /// competitions.
    #[serde(default = "default_sweep_interval_secs")]
    pub sweep_interval_secs: u64,

    /// How long a coordinator's lease on a competition lasts without renewal. When a
    /// coordinator stops without releasing its leases, another takes over after this.
    #[serde(default = "default_lease_ttl_secs")]
    pub lease_ttl_secs: u64,

    /// How many competition steps run at once.
    #[serde(default = "default_max_concurrent_steps")]
    pub max_concurrent_steps: usize,

    /// Names this process in leases and logs, such as its blue/green slot.
    #[serde(default)]
    pub instance_name: Option<String>,

    /// Enable on-chain escrow transactions (default: false)
    /// When disabled, only HODL invoices protect against non-completion.
    /// With keymeld signing, escrow is typically not needed since signing is fast.
    /// Enable this as a safety net if HODL invoice timing becomes an issue.
    #[serde(default)]
    pub escrow_enabled: bool,

    /// Enable mock oracle for E2E testing (no real oracle server required)
    #[serde(default)]
    pub mock_oracle: bool,

    /// Number of confirmations to wait before settling hold invoices after funding broadcast.
    /// Set to 0 to settle immediately at broadcast time (riskier but faster).
    /// Set to 1+ for more safety (wait for confirmations before settling).
    /// Default is 0 (settle immediately at broadcast).
    #[serde(default)]
    pub invoice_settlement_confirmations: u32,

    /// Settle what is owed and take no new money, as after a restore from backups: no
    /// competition, ticket or entry is accepted, while kickoffs, attestations, settlement
    /// transactions, payouts, refunds and reclaims carry on. See `docs/ops/disaster-recovery.md`.
    /// `COORDINATOR_SETTLE_ONLY` overrides it.
    #[serde(default)]
    pub settle_only: bool,

    /// In settle-only mode, what happens to a competition or pool that has not kicked off:
    /// "refund" (the default) cancels it and refunds every entry; "kickoff" lets those whose
    /// entries are already paid start as usual.
    #[serde(default)]
    pub settle_only_unstarted: SettleOnlyUnstarted,

    /// The most winning places a new competition or queued competition may pay: 1 (the
    /// default) or 2. Competitions created earlier keep their places.
    #[serde(default = "default_max_winning_places")]
    pub max_winning_places: usize,
}

fn default_max_winning_places() -> usize {
    1
}

/// Environment variable that sets `coordinator_settings.settle_only`, overriding the file.
pub const SETTLE_ONLY_ENV: &str = "COORDINATOR_SETTLE_ONLY";

/// What settle-only mode does with a competition that has not kicked off yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleOnlyUnstarted {
    /// Cancel it before its contract is built, and refund its entries.
    #[default]
    Refund,
    /// Kick it off as usual once its paid entries are in.
    Kickoff,
}

fn default_sweep_interval_secs() -> u64 {
    60
}

// Each held lease is renewed every third of this, and every renewal is a write that
// replication has to ship. A clean shutdown releases leases at once, so the length
// only delays takeover after a crash.
fn default_lease_ttl_secs() -> u64 {
    120
}

fn default_max_concurrent_steps() -> usize {
    8
}

impl CoordinatorSettings {
    pub fn pacing(&self) -> crate::domain::Pacing {
        crate::domain::Pacing {
            idle: std::time::Duration::from_secs(self.sync_interval_secs.max(1)),
            sweep: std::time::Duration::from_secs(self.sweep_interval_secs.max(1)),
            lease_ttl: std::time::Duration::from_secs(self.lease_ttl_secs.max(3)),
            max_concurrent_steps: self.max_concurrent_steps.max(1),
            ..Default::default()
        }
    }

    /// This process as a lease holder: its instance name and a fresh ID, so a restarted
    /// process never mistakes a previous run's leases for its own.
    pub fn lease_holder(&self) -> String {
        format!(
            "{}-{}",
            self.instance_name.as_deref().unwrap_or(&self.name),
            uuid::Uuid::now_v7()
        )
    }
}

impl Default for CoordinatorSettings {
    fn default() -> Self {
        CoordinatorSettings {
            name: String::from("coordinator"),
            oracle_url: String::from("http://127.0.0.1:9800"),
            private_key_file: String::from("./creds/coordinator_private_key.pem"),
            relative_locktime_block_delta: 432,
            required_confirmations: 1,
            sync_interval_secs: 15,
            sweep_interval_secs: default_sweep_interval_secs(),
            lease_ttl_secs: default_lease_ttl_secs(),
            max_concurrent_steps: default_max_concurrent_steps(),
            instance_name: None,
            escrow_enabled: false,
            mock_oracle: false,
            invoice_settlement_confirmations: 0,
            settle_only: false,
            settle_only_unstarted: SettleOnlyUnstarted::Refund,
            max_winning_places: default_max_winning_places(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UISettings {
    pub private_url: String,
    pub remote_url: String,
    pub ui_dir: String,
    /// Optional Satchel wallet, a Lightning wallet for test networks, as an `https://` origin
    /// such as `https://wallet.5day4cast.com`. Pages then offer "Pay with Satchel" beside the
    /// invoice, "Open Satchel" in the account menu, and the player's Satchel Lightning Address
    /// on the Payouts page, signing the player in there with their Nostr key. Refused on
    /// mainnet.
    #[serde(default)]
    pub satchel_url: Option<String>,
}

impl Default for UISettings {
    fn default() -> Self {
        UISettings {
            private_url: String::from("http://127.0.0.1:9991"),
            remote_url: String::from("http://127.0.0.1:9990"),
            ui_dir: String::from("./crates/public_ui"),
            satchel_url: None,
        }
    }
}

impl UISettings {
    /// Refuse a `satchel_url` that is not a bare `https://` origin, and any on mainnet:
    /// Satchel pays only test-network invoices.
    pub fn validate(&self, network: Network) -> Result<(), anyhow::Error> {
        let Some(url) = &self.satchel_url else {
            return Ok(());
        };
        if network == Network::Bitcoin {
            anyhow::bail!("ui_settings.satchel_url is refused on mainnet: Satchel is a wallet for test networks");
        }
        satchel_origin(url).map(|_| ())
    }

    /// Satchel's origin as pages link to it, `https://host[:port]` with no trailing slash;
    /// `None` when none is configured, or it is not valid (see [`Self::validate`]).
    pub fn satchel_origin(&self) -> Option<String> {
        self.satchel_url
            .as_deref()
            .and_then(|url| satchel_origin(url).ok())
    }
}

/// `url` as a bare `https://` origin: no credentials, path, query or fragment.
fn satchel_origin(url: &str) -> Result<String, anyhow::Error> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| anyhow!("ui_settings.satchel_url {url:?} is not a URL: {error}"))?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        anyhow::bail!("ui_settings.satchel_url must be an https:// origin, not {url:?}");
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        anyhow::bail!(
            "ui_settings.satchel_url must be an origin alone, with no credentials, path or query: {url:?}"
        );
    }
    Ok(parsed.origin().ascii_serialization())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct APISettings {
    pub domain: String,
    pub port: String,
    pub origins: Vec<String>,
    #[serde(default)]
    pub rate_limit: RateLimitSettings,
    #[serde(default = "APISettings::default_replay_capacity")]
    pub replay_capacity: usize,
}

impl APISettings {
    fn default_replay_capacity() -> usize {
        crate::api::nip98_replay::DEFAULT_REPLAY_CAPACITY
    }
}

impl Default for APISettings {
    fn default() -> Self {
        APISettings {
            domain: String::from("127.0.0.1"),
            port: String::from("9990"),
            origins: vec![String::from("http://localhost:9990")],
            rate_limit: RateLimitSettings::default(),
            replay_capacity: Self::default_replay_capacity(),
        }
    }
}

/// Public requests are keyed by the TCP peer address. Forwarded IP headers
/// are never trusted. A reverse proxy therefore shares one limit across clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitSettings {
    pub enabled: bool,
    pub per_second: u32,
    pub burst: u32,
    pub auth_per_second: u32,
    pub auth_burst: u32,
}

impl RateLimitSettings {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

impl Default for RateLimitSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            per_second: 20,
            burst: 60,
            auth_per_second: 2,
            auth_burst: 10,
        }
    }
}

pub fn get_settings() -> Result<Settings, anyhow::Error> {
    get_server_settings_with_cli(Cli::parse().into())
}

/// Load server settings after command dispatch, including runtime environment overrides.
/// Keep this distinct from the generic loader used by the wallet CLI.
pub fn get_server_settings_with_cli(cli: CliSettings) -> Result<Settings, anyhow::Error> {
    server_settings_with_env(cli, |name| env::var(name).ok())
}

fn server_settings_with_env(
    cli: CliSettings,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Settings, anyhow::Error> {
    let mut settings: Settings = get_settings_with_cli(cli)?;
    settings
        .metrics_settings
        .apply_env_override(var(METRICS_LISTEN_ADDR_ENV))?;
    settings
        .coordinator_settings
        .apply_settle_only_env(var(SETTLE_ONLY_ENV))?;
    settings
        .telemetry
        .apply_env_override(var(TELEMETRY_ENABLED_ENV))?;
    settings
        .http_context
        .apply_env_overrides(var(TRUSTED_PROXIES_ENV), var(CLIENT_IP_HEADER_ENV));
    settings.feedback_settings.apply_env(&var)?;
    settings.admin_settings.logs.apply_env(var);
    Ok(settings)
}

pub struct CliSettings {
    pub config: Option<String>,
    pub level: Option<String>,
}

impl From<Cli> for CliSettings {
    fn from(cli: Cli) -> Self {
        Self {
            config: cli.config,
            level: cli.level,
        }
    }
}
pub trait ConfigurableSettings: Serialize + for<'de> Deserialize<'de> + Default {
    /// Apply CLI settings after loading from file
    fn apply_cli_overrides(&mut self, cli_settings: &CliSettings);

    /// Get the default config file path
    fn default_config_path() -> PathBuf {
        PathBuf::from("./config/settings.toml")
    }

    /// Get the config directory path
    fn config_directory() -> PathBuf {
        PathBuf::from("./config")
    }
}

pub fn get_settings_with_cli<T: ConfigurableSettings>(
    cli_settings: CliSettings,
) -> Result<T, anyhow::Error> {
    let mut settings = if let Some(config_path) = cli_settings.config.clone() {
        let path = PathBuf::from(config_path);

        let absolute_path = if path.is_absolute() {
            path
        } else {
            env::current_dir()?.join(path)
        };

        let file_settings = match File::open(absolute_path) {
            Ok(mut file) => {
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .map_err(|e| anyhow!("Failed to read config: {}", e))?;
                toml::from_str(&content)
                    .map_err(|e| anyhow!("Failed to map config to settings: {}", e))?
            }
            Err(err) => return Err(anyhow!("Failed to find file: {}", err)),
        };
        file_settings
    } else {
        let default_path = T::default_config_path();
        match File::open(&default_path) {
            Ok(mut file) => {
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .map_err(|e| anyhow!("Failed to read default config: {}", e))?;
                toml::from_str(&content)
                    .map_err(|e| anyhow!("Failed to parse default config: {}", e))?
            }
            Err(_) => {
                // Create default settings
                let default_settings = T::default();

                // Create config directory if it doesn't exist
                fs::create_dir_all(T::config_directory())
                    .map_err(|e| anyhow!("Failed to create config directory: {}", e))?;

                let toml_content = toml::to_string(&default_settings)
                    .map_err(|e| anyhow!("Failed to serialize default settings: {}", e))?;

                let mut file = fs::File::create(&default_path)
                    .map_err(|e| anyhow!("Failed to create config file: {}", e))?;
                file.write_all(toml_content.as_bytes())
                    .map_err(|e| anyhow!("Failed to write default config: {}", e))?;

                default_settings
            }
        }
    };

    settings.apply_cli_overrides(&cli_settings);

    Ok(settings)
}

pub fn setup_logger(
    level: Option<String>,
    filter_targets: Vec<String>,
) -> Result<(), fern::InitError> {
    let rust_log = get_log_level(level);
    let colors = ColoredLevelConfig::new()
        .trace(Color::White)
        .debug(Color::Cyan)
        .info(Color::Blue)
        .warn(Color::Yellow)
        .error(Color::Magenta);

    fern::Dispatch::new()
        .format(move |out, message, record| {
            // Lines written while handling a request end with its id, except the
            // lines that carry their own `rid=` field.
            let rid = crate::api::request_context::REQUEST_CONTEXT
                .try_with(|context| context.rid.clone())
                .ok()
                .filter(|_| {
                    !crate::api::request_context::OWN_RID_TARGETS.contains(&record.target())
                });
            match rid {
                Some(rid) => out.finish(format_args!(
                    "[{} {}] {}: {} rid={}",
                    OffsetDateTime::now_utc().format(&Iso8601::DEFAULT).unwrap(),
                    colors.color(record.level()),
                    record.target(),
                    message,
                    rid
                )),
                None => out.finish(format_args!(
                    "[{} {}] {}: {}",
                    OffsetDateTime::now_utc().format(&Iso8601::DEFAULT).unwrap(),
                    colors.color(record.level()),
                    record.target(),
                    message
                )),
            }
        })
        .level(rust_log)
        .filter(move |metadata| {
            !filter_targets
                .iter()
                .any(|filter| metadata.target().starts_with(filter))
        })
        .chain(std::io::stdout())
        .apply()?;
    Ok(())
}

pub fn get_log_level(level: Option<String>) -> LevelFilter {
    if let Some(level) = &level {
        match level.as_ref() {
            "trace" => LevelFilter::Trace,
            "debug" => LevelFilter::Debug,
            "info" => LevelFilter::Info,
            "warn" => LevelFilter::Warn,
            "error" => LevelFilter::Error,
            _ => LevelFilter::Info,
        }
    } else {
        let rust_log = env::var("RUST_LOG").unwrap_or_else(|_| String::from(""));
        match rust_log.to_lowercase().as_str() {
            "trace" => LevelFilter::Trace,
            "debug" => LevelFilter::Debug,
            "info" => LevelFilter::Info,
            "warn" => LevelFilter::Warn,
            "error" => LevelFilter::Error,
            _ => LevelFilter::Info,
        }
    }
}

#[cfg(test)]
mod mainnet_guards {
    use super::*;

    #[test]
    fn unverified_lnd_tls_is_refused_on_mainnet_only() {
        let unverified = LnSettings {
            dangerous_accept_invalid_tls: true,
            ..Default::default()
        };
        assert!(unverified.validate(Network::Bitcoin).is_err());
        assert!(unverified.validate(Network::Signet).is_ok());
        assert!(LnSettings::default().validate(Network::Bitcoin).is_ok());
    }

    #[test]
    fn plaintext_lnd_connections_are_refused_on_mainnet() {
        let plaintext = LnSettings {
            base_url: "http://localhost:9095".into(),
            ..Default::default()
        };
        assert!(plaintext.validate(Network::Bitcoin).is_err());
        assert!(plaintext.validate(Network::Signet).is_ok());
        assert!(LnSettings::default().validate(Network::Bitcoin).is_ok());
    }

    #[test]
    fn escrow_mode_is_refused_on_mainnet_only() {
        let escrow = CoordinatorSettings {
            escrow_enabled: true,
            ..Default::default()
        };
        assert!(escrow.validate(Network::Bitcoin).is_err());
        assert!(escrow.validate(Network::Signet).is_ok());
        assert!(CoordinatorSettings::default()
            .validate(Network::Bitcoin)
            .is_ok());
    }

    #[test]
    fn winning_places_default_to_one_and_allow_two() {
        // A config written before the setting existed loads with one place.
        let text = toml::to_string(&Settings::default()).unwrap();
        let without: String = text
            .lines()
            .filter(|line| !line.starts_with("max_winning_places"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Settings = toml::from_str(&without).unwrap();
        assert_eq!(parsed.coordinator_settings.max_winning_places, 1);

        for (places, valid) in [(0, false), (1, true), (2, true), (3, false)] {
            let settings = CoordinatorSettings {
                max_winning_places: places,
                ..Default::default()
            };
            assert_eq!(
                settings.validate(Network::Signet).is_ok(),
                valid,
                "{places}"
            );
        }
    }
}

#[cfg(test)]
mod settle_only_settings_tests {
    use super::*;

    #[test]
    fn settle_only_is_off_and_refunds_unstarted_pools_by_default() {
        let defaults = CoordinatorSettings::default();
        assert!(!defaults.settle_only);
        assert_eq!(defaults.settle_only_unstarted, SettleOnlyUnstarted::Refund);

        // A config written before the setting existed loads with it off.
        let text = toml::to_string(&Settings::default()).unwrap();
        let without: String = text
            .lines()
            .filter(|line| !line.starts_with("settle_only"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Settings = toml::from_str(&without).unwrap();
        assert!(!parsed.coordinator_settings.settle_only);
        assert_eq!(
            parsed.coordinator_settings.settle_only_unstarted,
            SettleOnlyUnstarted::Refund
        );

        let configured: Settings = toml::from_str(&text.replace(
            "settle_only_unstarted = \"refund\"",
            "settle_only_unstarted = \"kickoff\"",
        ))
        .unwrap();
        assert_eq!(
            configured.coordinator_settings.settle_only_unstarted,
            SettleOnlyUnstarted::Kickoff
        );
    }

    #[test]
    fn environment_overrides_settle_only() {
        let mut settings = CoordinatorSettings::default();
        settings.apply_settle_only_env(None).unwrap();
        assert!(!settings.settle_only);
        settings.apply_settle_only_env(Some("true".into())).unwrap();
        assert!(settings.settle_only);
        settings.apply_settle_only_env(Some(" ".into())).unwrap();
        assert!(settings.settle_only);
        settings.apply_settle_only_env(Some("0".into())).unwrap();
        assert!(!settings.settle_only);
        assert!(settings
            .apply_settle_only_env(Some("maybe".into()))
            .is_err());
    }
}

#[cfg(test)]
mod satchel_settings_tests {
    use super::*;

    fn with_satchel(url: &str, network: Network) -> Settings {
        let mut settings = Settings::default();
        settings.ui_settings.satchel_url = Some(url.into());
        settings.bitcoin_settings.network = network;
        settings
    }

    #[test]
    fn satchel_is_off_unless_configured() {
        assert_eq!(UISettings::default().satchel_url, None);
        assert_eq!(UISettings::default().satchel_origin(), None);
        // A config written before the setting existed loads without it.
        let text = toml::to_string(&Settings::default()).unwrap();
        assert!(!text.contains("satchel_url"));
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(parsed.ui_settings.satchel_url, None);

        let configured: Settings = toml::from_str(&text.replace(
            "[ui_settings]",
            "[ui_settings]\nsatchel_url = \"https://wallet.5day4cast.com\"",
        ))
        .unwrap();
        assert_eq!(
            configured.ui_settings.satchel_origin().as_deref(),
            Some("https://wallet.5day4cast.com")
        );
    }

    #[test]
    fn satchel_is_refused_on_mainnet() {
        let url = "https://wallet.5day4cast.com";
        let refused = with_satchel(url, Network::Bitcoin).validate().unwrap_err();
        assert!(refused.to_string().contains("satchel_url"), "{refused}");
        for network in [Network::Signet, Network::Testnet, Network::Regtest] {
            assert!(with_satchel(url, network).validate().is_ok(), "{network}");
        }
        // Without Satchel, mainnet is not refused for it.
        assert!(UISettings::default().validate(Network::Bitcoin).is_ok());
    }

    #[test]
    fn satchel_must_be_a_bare_https_origin() {
        for url in [
            "http://wallet.5day4cast.com",
            "ftp://wallet.5day4cast.com",
            "wallet.5day4cast.com",
            "https://",
            "https://wallet.5day4cast.com/wallet",
            "https://wallet.5day4cast.com/?next=/wallet",
            "https://wallet.5day4cast.com/#wallet",
            "https://alice:secret@wallet.5day4cast.com",
            "https://alice@wallet.5day4cast.com",
            "not a url",
        ] {
            assert!(
                with_satchel(url, Network::Signet).validate().is_err(),
                "{url}"
            );
            let ui = UISettings {
                satchel_url: Some(url.into()),
                ..Default::default()
            };
            assert_eq!(ui.satchel_origin(), None, "{url}");
        }
        for (url, origin) in [
            (
                "https://wallet.5day4cast.com",
                "https://wallet.5day4cast.com",
            ),
            (
                "https://wallet.5day4cast.com/",
                "https://wallet.5day4cast.com",
            ),
            (
                "https://Wallet.Example.org:8443",
                "https://wallet.example.org:8443",
            ),
        ] {
            let settings = with_satchel(url, Network::Signet);
            assert!(settings.validate().is_ok(), "{url}");
            assert_eq!(
                settings.ui_settings.satchel_origin().as_deref(),
                Some(origin),
                "{url}"
            );
        }
    }
}

/// The feedback form and its alerts. Off by default.
///
/// Each key can also be set by an environment variable, which wins: `COORDINATOR_FEEDBACK_`
/// and the key in capitals, for example `COORDINATOR_FEEDBACK_NTFY_URL`. Without an ntfy URL
/// messages are stored and shown on the operator's Feedback page, and no alert is sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackSettings {
    pub enabled: bool,
    /// The ntfy server alerts are posted to, for example `https://ntfy.example.com`.
    pub ntfy_url: Option<String>,
    pub ntfy_topic: String,
    /// File holding an ntfy access token allowed to publish to the topic.
    pub ntfy_token_file: Option<String>,
    /// Sent as ntfy's `Email` header, so the server emails a copy too.
    pub notify_email: Option<String>,
    /// The operator origin alerts link to, for example `https://admin.example.com:9443`.
    pub admin_url: Option<String>,
}

pub const FEEDBACK_ENV_PREFIX: &str = "COORDINATOR_FEEDBACK_";

impl Default for FeedbackSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            ntfy_url: None,
            ntfy_topic: String::from("feedback"),
            ntfy_token_file: None,
            notify_email: None,
            admin_url: None,
        }
    }
}

impl FeedbackSettings {
    /// Apply the `COORDINATOR_FEEDBACK_*` variables `var` finds. An empty value unsets an
    /// optional key.
    pub fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) -> Result<(), anyhow::Error> {
        let var = |key: &str| var(&format!("{FEEDBACK_ENV_PREFIX}{key}"));
        if let Some(enabled) = var("ENABLED") {
            self.enabled = match enabled.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" | "" => false,
                other => {
                    return Err(anyhow!(
                        "{FEEDBACK_ENV_PREFIX}ENABLED must be true or false, not {other:?}"
                    ))
                }
            };
        }
        let optional = |value: String| Some(value.trim().to_owned()).filter(|v| !v.is_empty());
        if let Some(value) = var("NTFY_URL") {
            self.ntfy_url = optional(value);
        }
        if let Some(value) = var("NTFY_TOPIC").and_then(optional) {
            self.ntfy_topic = value;
        }
        if let Some(value) = var("NTFY_TOKEN_FILE") {
            self.ntfy_token_file = optional(value);
        }
        if let Some(value) = var("NOTIFY_EMAIL") {
            self.notify_email = optional(value);
        }
        if let Some(value) = var("ADMIN_URL") {
            self.admin_url = optional(value);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if !self.enabled {
            return Ok(());
        }
        if self.ntfy_topic.is_empty()
            || self.ntfy_topic.len() > 64
            || !self
                .ntfy_topic
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(anyhow!(
                "feedback.ntfy_topic must be 1-64 letters, digits, - or _"
            ));
        }
        for (key, url) in [
            ("feedback.ntfy_url", &self.ntfy_url),
            ("feedback.admin_url", &self.admin_url),
        ] {
            if let Some(url) = url {
                let parsed = reqwest::Url::parse(url).map_err(|e| anyhow!("{key}: {e}"))?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                    || !parsed.username().is_empty()
                {
                    return Err(anyhow!(
                        "{key} must be an http(s) URL without credentials, query or fragment"
                    ));
                }
            }
        }
        if let Some(email) = &self.notify_email {
            if email.len() > 254 || email.contains(char::is_whitespace) || !email.contains('@') {
                return Err(anyhow!("feedback.notify_email is not an email address"));
            }
        }
        Ok(())
    }
}

/// Where the operator's Visitors page reads visitor logs: a Grafana-shaped base URL whose
/// `query_path` answers Loki's `query_range`. Unset, the page says logs are not configured.
/// Environment variables win: `COORDINATOR_LOGS_URL`, `COORDINATOR_LOGS_QUERY_PATH` and
/// `COORDINATOR_LOGS_EXPLORE_URL`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogsSettings {
    pub url: Option<String>,
    pub query_path: String,
    /// The operator's Grafana, for "Open in Explore" links.
    pub explore_url: Option<String>,
}

pub const DEFAULT_LOGS_QUERY_PATH: &str =
    "/api/datasources/proxy/uid/visitors/loki/api/v1/query_range";

impl Default for LogsSettings {
    fn default() -> Self {
        Self {
            url: None,
            query_path: String::from(DEFAULT_LOGS_QUERY_PATH),
            explore_url: None,
        }
    }
}

impl LogsSettings {
    pub fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) {
        let optional = |value: String| Some(value.trim().to_owned()).filter(|v| !v.is_empty());
        if let Some(value) = var("COORDINATOR_LOGS_URL") {
            self.url = optional(value);
        }
        if let Some(value) = var("COORDINATOR_LOGS_QUERY_PATH").and_then(optional) {
            self.query_path = value;
        }
        if let Some(value) = var("COORDINATOR_LOGS_EXPLORE_URL") {
            self.explore_url = optional(value);
        }
    }

    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if !self.query_path.starts_with('/')
            || self.query_path.contains(['?', '#'])
            || self.query_path.contains("..")
        {
            return Err(anyhow!(
                "admin_settings.logs.query_path must be an absolute path without a query"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod feedback_settings_tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn server_loader_applies_runtime_environment_after_file_and_cli() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), toml::to_string(&Settings::default()).unwrap()).unwrap();
        let settings = server_settings_with_env(
            CliSettings {
                config: Some(file.path().to_string_lossy().into_owned()),
                level: Some("debug".into()),
            },
            env(&[
                (METRICS_LISTEN_ADDR_ENV, "127.0.0.1:9989"),
                (SETTLE_ONLY_ENV, "true"),
                (TELEMETRY_ENABLED_ENV, "true"),
                (TRUSTED_PROXIES_ENV, "127.0.0.1/32"),
                (CLIENT_IP_HEADER_ENV, "X-Real-IP"),
                ("COORDINATOR_FEEDBACK_ENABLED", "true"),
                (
                    "COORDINATOR_FEEDBACK_NTFY_URL",
                    "https://notify.example.com",
                ),
                ("COORDINATOR_LOGS_URL", "https://monitoring.example.com"),
            ]),
        )
        .unwrap();
        assert_eq!(settings.level.as_deref(), Some("debug"));
        assert_eq!(
            settings.metrics_settings.listen_addr,
            Some("127.0.0.1:9989".parse().unwrap())
        );
        assert!(settings.coordinator_settings.settle_only);
        assert!(settings.telemetry.enabled);
        assert_eq!(settings.http_context.trusted_proxies, ["127.0.0.1/32"]);
        assert_eq!(settings.http_context.client_ip_header, "X-Real-IP");
        assert!(settings.feedback_settings.enabled);
        assert_eq!(
            settings.feedback_settings.ntfy_url.as_deref(),
            Some("https://notify.example.com")
        );
        assert_eq!(
            settings.admin_settings.logs.url.as_deref(),
            Some("https://monitoring.example.com")
        );
    }

    #[test]
    fn feedback_is_off_by_default_and_read_from_toml_and_the_environment() {
        let defaults = FeedbackSettings::default();
        assert!(!defaults.enabled);
        assert_eq!(defaults.ntfy_topic, "feedback");
        assert!(defaults.validate().is_ok());

        let mut settings: FeedbackSettings =
            toml::from_str("enabled = true\nntfy_url = \"https://ntfy.example.com\"\n").unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.ntfy_topic, "feedback");
        settings
            .apply_env(env(&[
                ("COORDINATOR_FEEDBACK_NTFY_TOPIC", "feedback-test"),
                ("COORDINATOR_FEEDBACK_NTFY_URL", ""),
                (
                    "COORDINATOR_FEEDBACK_ADMIN_URL",
                    "https://admin.example.com:9443",
                ),
                ("COORDINATOR_FEEDBACK_NOTIFY_EMAIL", "ops@example.com"),
            ]))
            .unwrap();
        assert_eq!(settings.ntfy_topic, "feedback-test");
        assert_eq!(settings.ntfy_url, None);
        assert_eq!(
            settings.admin_url.as_deref(),
            Some("https://admin.example.com:9443")
        );
        assert!(settings.validate().is_ok());

        settings
            .apply_env(env(&[("COORDINATOR_FEEDBACK_ENABLED", "false")]))
            .unwrap();
        assert!(!settings.enabled);
        assert!(settings
            .apply_env(env(&[("COORDINATOR_FEEDBACK_ENABLED", "maybe")]))
            .is_err());
    }

    #[test]
    fn bad_feedback_settings_are_refused_when_enabled() {
        for bad in [
            FeedbackSettings {
                ntfy_topic: "a/b".into(),
                ..FeedbackSettings::default()
            },
            FeedbackSettings {
                ntfy_url: Some("ftp://ntfy.example.com".into()),
                ..FeedbackSettings::default()
            },
            FeedbackSettings {
                admin_url: Some("https://user:pw@admin.example.com".into()),
                ..FeedbackSettings::default()
            },
            FeedbackSettings {
                notify_email: Some("not an email".into()),
                ..FeedbackSettings::default()
            },
        ] {
            assert!(bad.validate().is_ok(), "ignored while disabled");
            let enabled = FeedbackSettings {
                enabled: true,
                ..bad
            };
            assert!(enabled.validate().is_err(), "{enabled:?}");
        }
    }

    #[test]
    fn logs_settings_default_to_the_visitors_datasource() {
        let mut logs = LogsSettings::default();
        assert_eq!(logs.url, None);
        assert_eq!(logs.query_path, DEFAULT_LOGS_QUERY_PATH);
        logs.apply_env(env(&[
            ("COORDINATOR_LOGS_URL", "https://monitoring.example.com"),
            (
                "COORDINATOR_LOGS_EXPLORE_URL",
                "https://grafana.example.com",
            ),
        ]));
        assert_eq!(logs.url.as_deref(), Some("https://monitoring.example.com"));
        assert!(logs.validate().is_ok());
        logs.query_path = "/x?y=1".into();
        assert!(logs.validate().is_err());
    }
}
