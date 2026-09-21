//! The LND REST calls for hold invoices.

use anyhow::{anyhow, Context};
use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::config::LndConfig;

const MACAROON_HEADER: &str = "Grpc-Metadata-macaroon";

/// A hold invoice's state in LND.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InvoiceState {
    Open,
    /// The payer's HTLC has arrived and is held until we settle or cancel.
    Accepted,
    Settled,
    Canceled,
}

pub struct Lnd {
    client: reqwest::Client,
    base_url: String,
    macaroon: String,
}

impl Lnd {
    pub fn new(config: &LndConfig) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(20));
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
        })
    }

    /// Add a hold invoice for `payment_hash`, returning the BOLT11 payment request.
    pub async fn add_hold_invoice(
        &self,
        payment_hash: &[u8; 32],
        amount_sat: u64,
        expiry_secs: u64,
        cltv_expiry: u32,
        memo: &str,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Added {
            payment_request: String,
        }
        let body = json!({
            "hash": STANDARD.encode(payment_hash),
            "value": amount_sat.to_string(),
            "expiry": expiry_secs.to_string(),
            "cltv_expiry": cltv_expiry.to_string(),
            "memo": memo,
        });
        let added: Added = self.post("v2/invoices/hodl", body).await?.json().await?;
        Ok(added.payment_request)
    }

    pub async fn invoice_state(&self, payment_hash: &[u8; 32]) -> anyhow::Result<InvoiceState> {
        #[derive(Deserialize)]
        struct Lookup {
            state: InvoiceState,
        }
        let response = self
            .client
            .get(format!(
                "{}v2/invoices/lookup?payment_hash={}",
                self.base_url,
                URL_SAFE.encode(payment_hash)
            ))
            .header(MACAROON_HEADER, &self.macaroon)
            .send()
            .await?;
        let response = checked(response, "look up the invoice").await?;
        Ok(response.json::<Lookup>().await?.state)
    }

    pub async fn settle(&self, preimage: &[u8; 32]) -> anyhow::Result<()> {
        self.post(
            "v2/invoices/settle",
            json!({ "preimage": STANDARD.encode(preimage) }),
        )
        .await?;
        Ok(())
    }

    pub async fn cancel(&self, payment_hash: &[u8; 32]) -> anyhow::Result<()> {
        self.post(
            "v2/invoices/cancel",
            json!({ "payment_hash": STANDARD.encode(payment_hash) }),
        )
        .await?;
        Ok(())
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> anyhow::Result<reqwest::Response> {
        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header(MACAROON_HEADER, &self.macaroon)
            .json(&body)
            .send()
            .await?;
        checked(response, path).await
    }
}

async fn checked(response: reqwest::Response, what: &str) -> anyhow::Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(anyhow!("LND {what} failed: {status} {body}"))
}
