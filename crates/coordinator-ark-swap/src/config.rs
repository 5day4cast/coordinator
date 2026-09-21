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

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
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
