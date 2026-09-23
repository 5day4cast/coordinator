//! Paying a competition's entry invoices, for scenarios that need real payments.
//!
//! An Arkade entry is funded by `ark-swapd` swapping the player's Lightning payment into their
//! escrow VTXO, so a scenario that exercises escrows has to pay for real: the coordinator's test
//! settle endpoint holds no invoice of ark-swapd's to settle. Scenarios that do not need an
//! escrow keep using that endpoint, and leave this unconfigured.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::json;

const MACAROON_HEADER: &str = "Grpc-Metadata-macaroon";

/// Where a payment got to, as LND reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentStatus {
    Unknown,
    InFlight,
    Succeeded,
    Failed,
    Initiated,
}

/// The node a scenario's players pay from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LndConfig {
    /// For example `https://192.168.1.16:8080`.
    pub rest_url: String,
    /// A macaroon that may send payments.
    pub macaroon_file: std::path::PathBuf,
    /// The node's TLS certificate, when it is self-signed.
    #[serde(default)]
    pub tls_cert_file: Option<std::path::PathBuf>,
    /// The most a payment may pay in fees.
    #[serde(default = "default_fee_limit_sats")]
    pub fee_limit_sats: u64,
    /// How long a payment may take.
    #[serde(default = "default_payment_timeout_secs")]
    pub payment_timeout_secs: u64,
}

fn default_fee_limit_sats() -> u64 {
    100
}

fn default_payment_timeout_secs() -> u64 {
    60
}

pub struct Lnd {
    client: reqwest::Client,
    base_url: String,
    macaroon: String,
    fee_limit_sats: u64,
    payment_timeout_secs: u64,
}

impl Lnd {
    pub fn new(config: &LndConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(
                config.payment_timeout_secs + 30,
            ));
        if let Some(path) = &config.tls_cert_file {
            let pem = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        let macaroon = std::fs::read(&config.macaroon_file)
            .with_context(|| format!("read {}", config.macaroon_file.display()))?;
        let mut base_url = config.rest_url.clone();
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        Ok(Self {
            client: builder.build()?,
            base_url,
            macaroon: hex::encode(macaroon),
            fee_limit_sats: config.fee_limit_sats,
            payment_timeout_secs: config.payment_timeout_secs,
        })
    }

    /// Pay `invoice`, waiting for it to settle or fail.
    ///
    /// The router streams a payment's progress, so this reads until the last status it reports.
    pub async fn pay(&self, invoice: &str) -> Result<()> {
        let response = self
            .client
            .post(format!("{}v2/router/send", self.base_url))
            .header(MACAROON_HEADER, &self.macaroon)
            .json(&json!({
                "payment_request": invoice,
                "timeout_seconds": self.payment_timeout_secs,
                "fee_limit_sat": self.fee_limit_sats.to_string(),
            }))
            .send()
            .await
            .context("send the payment")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("LND refused the payment ({status}): {body}");
        }
        let body = response.text().await.context("read the payment stream")?;
        let last = body
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .ok_or_else(|| anyhow!("LND said nothing about the payment"))?;
        #[derive(Deserialize)]
        struct Update {
            result: Option<Payment>,
            error: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct Payment {
            status: PaymentStatus,
            #[serde(default)]
            failure_reason: Option<String>,
        }
        let update: Update =
            serde_json::from_str(last).with_context(|| format!("parse {last}"))?;
        if let Some(error) = update.error {
            anyhow::bail!("LND could not pay: {error}");
        }
        let payment = update
            .result
            .ok_or_else(|| anyhow!("LND reported no payment"))?;
        match payment.status {
            PaymentStatus::Succeeded => Ok(()),
            other => Err(anyhow!(
                "the payment ended {other:?}: {}",
                payment.failure_reason.unwrap_or_default()
            )),
        }
    }
}
