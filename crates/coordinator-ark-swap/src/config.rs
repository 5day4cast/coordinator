use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use bitcoin::Network;
use serde::Deserialize;

/// `ark-swapd`'s settings, read from a TOML file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the HTTP API listens.
    pub listen: SocketAddr,
    /// A file holding the bearer token callers must present.
    pub api_token_file: PathBuf,
    /// Holds `swaps.sqlite` and the wallet key, `wallet.key`.
    pub data_dir: PathBuf,
    pub network: Network,
    pub ark_server_url: String,
    pub esplora_url: String,
    /// How long a swap's hold invoice stays payable.
    #[serde(default = "default_invoice_expiry_secs")]
    pub invoice_expiry_secs: u64,
    /// The hold invoice's final CLTV delta. The HTLC is held for seconds, so this can be short.
    #[serde(default = "default_invoice_cltv_expiry")]
    pub invoice_cltv_expiry: u32,
    /// Days. A VTXO with less life left than this is settled into a fresh one in the next batch.
    /// A VTXO lives for a week, and one that expires is swept and cannot be spent.
    #[serde(default = "default_renew_margin_days")]
    pub renew_margin_days: u32,
    /// Days. An escrow is never paid from a VTXO with less life left than this, since the escrow
    /// expires when the VTXO that paid it would have. No more than `renew_margin_days`.
    #[serde(default = "default_pay_margin_days")]
    pub pay_margin_days: u32,
    pub lnd: LndConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LndConfig {
    /// LND's REST base URL, for example `https://127.0.0.1:8080/`.
    pub rest_url: String,
    /// The macaroon file, binary as LND writes it. It needs invoice read and write.
    pub macaroon_file: PathBuf,
    /// LND's TLS certificate. Without it, only publicly trusted certificates are accepted.
    pub tls_cert_file: Option<PathBuf>,
}

fn default_invoice_expiry_secs() -> u64 {
    600
}

fn default_invoice_cltv_expiry() -> u32 {
    40
}

fn default_renew_margin_days() -> u32 {
    3
}

fn default_pay_margin_days() -> u32 {
    2
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parse {}", path.display()))
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text)?;
        // A VTXO between the two margins would neither pay an escrow nor be renewed.
        anyhow::ensure!(
            config.pay_margin_days <= config.renew_margin_days,
            "pay_margin_days ({}) must be no more than renew_margin_days ({})",
            config.pay_margin_days,
            config.renew_margin_days
        );
        Ok(config)
    }

    pub fn api_token(&self) -> anyhow::Result<String> {
        let token = std::fs::read_to_string(&self.api_token_file)
            .with_context(|| format!("read {}", self.api_token_file.display()))?;
        let token = token.trim().to_owned();
        anyhow::ensure!(
            token.len() >= 32,
            "the API token must be at least 32 characters"
        );
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUIRED: &str = r#"
        listen = "127.0.0.1:9100"
        api_token_file = "/run/secrets/ark-swapd-token"
        data_dir = "/var/lib/ark-swapd"
        network = "signet"
        ark_server_url = "https://arkd.example"
        esplora_url = "https://esplora.example"

        [lnd]
        rest_url = "https://127.0.0.1:8080/"
        macaroon_file = "/run/secrets/invoice.macaroon"
    "#;

    #[test]
    fn vtxos_are_renewed_three_days_and_escrows_paid_two_days_before_expiry_by_default() {
        let config = Config::parse(REQUIRED).unwrap();
        assert_eq!((config.renew_margin_days, config.pay_margin_days), (3, 2));

        let set = format!("renew_margin_days = 5\npay_margin_days = 4\n{REQUIRED}");
        let config = Config::parse(&set).unwrap();
        assert_eq!((config.renew_margin_days, config.pay_margin_days), (5, 4));

        // Coins between the margins would be stuck: too short-lived to pay, never renewed.
        let stuck = format!("renew_margin_days = 1\n{REQUIRED}");
        assert!(Config::parse(&stuck).is_err());
    }
}
