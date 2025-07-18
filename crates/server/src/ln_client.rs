use anyhow::anyhow;
use async_trait::async_trait;
use base64::Engine;
use bdk_wallet::bitcoin::hashes::{sha256, Hash};
use log::{debug, info};
use reqwest_middleware::{
    reqwest::{Certificate, Client, Url},
    ClientBuilder, ClientWithMiddleware,
};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{self, metadata},
    path::Path,
    time::Duration,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::LnSettings;

#[async_trait]
pub trait Ln: Send + Sync {
    async fn ping(&self) -> Result<(), anyhow::Error>;
    async fn add_hold_invoice(
        &self,
        value: u64,
        expiry_time_secs: u64,
        ticket_hash: String,
        competition_id: Uuid,
        hex_refund_tx: String,
    ) -> Result<InvoiceAddResponse, anyhow::Error>;
    async fn add_invoice(
        &self,
        value: u64,
        expiry_time_secs: u64,
        memo: String,
        competition_id: Uuid,
    ) -> Result<InvoiceAddResponse, anyhow::Error>;
    async fn create_invoice(
        &self,
        value: u64,
        expiry_time_secs: u64,
    ) -> Result<String, anyhow::Error>;
    async fn cancel_hold_invoice(&self, ticket_hash: String) -> Result<(), anyhow::Error>;
    async fn settle_hold_invoice(&self, ticket_preimage: String) -> Result<(), anyhow::Error>;
    async fn lookup_invoice(&self, r_hash: &str) -> Result<InvoiceLookupResponse, anyhow::Error>;
    async fn send_payment(
        &self,
        payout_payment_request: String,
        timeout_seconds: u64,
        fee_limit_sat: u64,
    ) -> Result<(), anyhow::Error>;
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum InvoiceState {
    Open,
    Settled,
    Canceled,
    Accepted,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InvoiceLookupResponse {
    pub state: InvoiceState,
    pub memo: Option<String>,
    pub r_hash: String,
    pub value: String,
    pub settled: bool,
    pub creation_date: String,
    pub settle_date: String,
    pub payment_request: String,
    pub expiry: String,
}

//TODO: we might need to add tls cert as an option, skipping for now
#[derive(Clone)]
pub struct LnClient {
    pub base_url: Url,
    pub client: ClientWithMiddleware,
    pub macaroon: SecretString,
}

impl LnClient {
    pub async fn new(
        client: ClientWithMiddleware,
        settings: LnSettings,
    ) -> Result<Self, anyhow::Error> {
        let macaroon = get_macaroon(&settings.macaroon_file_path)?;
        let client = if let Some(tls_cert_path) = settings.tls_cert_path {
            info!("Found tls.crt file, using for lnd client");
            let cert = get_tls_cert(&tls_cert_path)?;
            let client = build_reqwest_tls_client(cert)?;
            client
        } else {
            info!("No tls.crt file found, skipping for lnd client");
            client
        };
        Ok(Self {
            base_url: Url::parse(&settings.base_url)?,
            client,
            macaroon,
        })
    }
}

pub fn build_reqwest_tls_client(
    tls_cert: Certificate,
) -> Result<ClientWithMiddleware, anyhow::Error> {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    let client = Client::builder()
        .add_root_certificate(tls_cert)
        .danger_accept_invalid_certs(true) // only use this for development locally or for self signed certs
        .tls_built_in_root_certs(true)
        .build()?;
    let client = ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
    Ok(client)
}

pub fn get_tls_cert(file_path: &str) -> Result<Certificate, anyhow::Error> {
    if !is_tls_cert_file(file_path) {
        return Err(anyhow!("Not a '.cert' file extension"));
    }

    metadata(file_path)?;
    read_tls_cert(file_path.to_owned())
}

fn is_tls_cert_file(file_path: &str) -> bool {
    Path::new(file_path)
        .extension()
        .and_then(|s| s.to_str())
        .map_or(false, |ext| ext == "cert")
}

fn read_tls_cert(macaroon_path: String) -> Result<Certificate, anyhow::Error> {
    let cert_bytes =
        fs::read(macaroon_path).map_err(|e| anyhow!("Failed to read tls cert file: {}", e))?;
    Certificate::from_pem(&cert_bytes).map_err(|e| anyhow!("Failed to build cert from file: {}", e))
}

pub fn get_macaroon(file_path: &str) -> Result<SecretString, anyhow::Error> {
    if !is_macaroon_file(file_path) {
        return Err(anyhow!("Not a '.macaroon' file extension"));
    }

    metadata(file_path)?;
    read_macaroon(file_path.to_owned())
}

fn is_macaroon_file(file_path: &str) -> bool {
    Path::new(file_path)
        .extension()
        .and_then(|s| s.to_str())
        .map_or(false, |ext| ext == "macaroon")
}

fn read_macaroon(macaroon_path: String) -> Result<SecretString, anyhow::Error> {
    let contents =
        fs::read(macaroon_path).map_err(|e| anyhow!("Failed to read macaroon file: {}", e))?;
    let hex_string = buffer_as_hex(contents);
    Ok(SecretString::from(hex_string))
}

fn buffer_as_hex(bytes: Vec<u8>) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

const MACAROON_HEADER: &str = "Grpc-Metadata-macaroon";
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct InvoiceAddResponse {
    pub payment_request: String,
    pub add_index: String,
    pub payment_addr: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentResponse {
    pub payment_error: String,
    pub payment_preimage: String,
    pub payment_route: Route,
    pub payment_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Route {
    pub total_time_lock: u32,
    pub total_fees: String,
    pub total_amt: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HoldInvoiceRequest {
    pub hash: String,         // Base64 encoded payment hash
    pub value: String,        // Amount in satoshis
    pub expiry: String,       // Expiry time in seconds
    pub memo: Option<String>, // Holds refund transaction and competition id, encrypted to the invoice preimage
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InvoiceRequest {
    pub value: String,  // Amount in satoshis
    pub expiry: String, // Expiry time in seconds
    pub memo: String,   // Memo field (description)
}

#[async_trait]
impl Ln for LnClient {
    async fn ping(&self) -> Result<(), anyhow::Error> {
        let response = self
            .client
            .get(format!("{}v1/getinfo", self.base_url))
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to ping lnd node: {}",
                response.text().await?
            ));
        }

        let now = OffsetDateTime::now_utc();
        info!(
            "Ping was successful at: {}",
            OffsetDateTime::format(now, &Rfc3339)?
        );
        let data = response.text().await?;
        let json_data: serde_json::Value = serde_json::from_str(&data)?;
        info!(
            "Current LND state: {}",
            serde_json::to_string_pretty(&json_data)?
        );

        Ok(())
    }

    async fn add_hold_invoice(
        &self,
        value: u64,
        expiry_time_secs: u64,
        ticket_hash_hex: String,
        competition_id: Uuid,
        hex_refund_tx: String,
    ) -> Result<InvoiceAddResponse, anyhow::Error> {
        info!("ticket_hash_hex: {:?}", ticket_hash_hex);

        let hash_bytes = hex::decode(&ticket_hash_hex)
            .map_err(|e| anyhow!("Failed to decode hex hash: {}", e))?;

        if hash_bytes.len() != 32 {
            return Err(anyhow!(
                "Hash must be 32 bytes, got {} bytes",
                hash_bytes.len()
            ));
        }

        let refund_tx_hash = sha256::Hash::hash(hex_refund_tx.as_bytes()).to_byte_array();

        let memo = format!("c:{};r:{:?}", competition_id, refund_tx_hash);

        let hash_base64 = base64::engine::general_purpose::STANDARD.encode(&hash_bytes);

        let body = HoldInvoiceRequest {
            hash: hash_base64.clone(),
            value: value.to_string(),
            expiry: expiry_time_secs.to_string(),
            memo: Some(memo),
        };

        info!("hold invoice: {:?}", body);
        info!("hash_base64: {:?}", hash_base64);
        let response = self
            .client
            .post(format!("{}v2/invoices/hodl", self.base_url))
            .json(&body)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to create hold invoice: {}",
                response.status()
            ));
        }

        let invoice_response = response.json::<InvoiceAddResponse>().await?;
        Ok(invoice_response)
    }

    async fn add_invoice(
        &self,
        value: u64,
        expiry_time_secs: u64,
        memo: String,
        competition_id: Uuid,
    ) -> Result<InvoiceAddResponse, anyhow::Error> {
        let body = json!({
            "value": value.to_string(),
            "expiry": expiry_time_secs.to_string(),
            "memo": format!("{} - competition_id:{}", memo, competition_id.to_string()),
        });

        info!("Creating regular invoice: {:?}", body);

        let response = self
            .client
            .post(format!("{}v1/invoices", self.base_url))
            .json(&body)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to create invoice: {}",
                response.status()
            ));
        }

        let invoice_response = response.json::<InvoiceAddResponse>().await?;
        Ok(invoice_response)
    }

    async fn cancel_hold_invoice(&self, ticket_hash_hex: String) -> Result<(), anyhow::Error> {
        let hash_bytes = hex::decode(&ticket_hash_hex)
            .map_err(|e| anyhow!("Failed to decode hex hash: {}", e))?;

        if hash_bytes.len() != 32 {
            return Err(anyhow!(
                "Hash must be 32 bytes, got {} bytes",
                hash_bytes.len()
            ));
        }

        let hash_base64 = base64::engine::general_purpose::STANDARD.encode(&hash_bytes);
        let body = json!({
            "payment_hash":hash_base64
        });

        let response = self
            .client
            .post(format!("{}v2/invoices/hodl/cancel", self.base_url))
            .json(&body)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to cancel hold invoice: {}",
                response.status()
            ));
        }

        Ok(())
    }

    async fn settle_hold_invoice(&self, ticket_preimage: String) -> Result<(), anyhow::Error> {
        let preimage_bytes = hex::decode(&ticket_preimage)
            .map_err(|e| anyhow!("Failed to decode hex preimage: {}", e))?;

        let preimage_base64 = base64::engine::general_purpose::STANDARD.encode(&preimage_bytes);

        let body = json!({
            "preimage": preimage_base64
        });

        let response = self
            .client
            .post(format!("{}v2/invoices/settle", self.base_url))
            .json(&body)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to settle hold invoice: {}",
                response.status()
            ));
        }

        Ok(())
    }

    async fn create_invoice(
        &self,
        value: u64,
        expiry_time_secs: u64,
    ) -> Result<String, anyhow::Error> {
        let body = json!({
            "value": value,
            "expiry": expiry_time_secs
        });

        let response = self
            .client
            .post(format!("{}v1/invoices", self.base_url))
            .json(&body)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to create invoice: {}",
                response.text().await?
            ));
        }

        let invoice_response = response.json::<InvoiceAddResponse>().await?;
        Ok(invoice_response.payment_request)
    }

    async fn lookup_invoice(
        &self,
        ticket_hash_hex: &str,
    ) -> Result<InvoiceLookupResponse, anyhow::Error> {
        let hash_bytes = hex::decode(&ticket_hash_hex)
            .map_err(|e| anyhow!("Failed to decode hex hash: {}", e))?;

        if hash_bytes.len() != 32 {
            return Err(anyhow!(
                "Hash must be 32 bytes, got {} bytes",
                hash_bytes.len()
            ));
        }

        let hash_base64 = base64::engine::general_purpose::URL_SAFE.encode(&hash_bytes);

        let response = self
            .client
            .get(format!(
                "{}v2/invoices/lookup?payment_hash={}",
                self.base_url, hash_base64
            ))
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow!("Failed to lookup invoice: {}", response.status()));
        }

        let invoice = response.json::<InvoiceLookupResponse>().await?;
        Ok(invoice)
    }

    async fn send_payment(
        &self,
        payout_payment_request: String,
        timeout_seconds: u64,
        fee_limit_sat: u64,
    ) -> Result<(), anyhow::Error> {
        let body = json!({
            "payment_request": payout_payment_request,
            "timeout_seconds": timeout_seconds,
            "fee_limit_sat": fee_limit_sat.to_string(),
            "allow_self_payment": true,
        });
        debug!("sending payment: {:?}", body);
        let response = self
            .client
            .post(format!("{}v2/router/send", self.base_url))
            .json(&body)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .timeout(Duration::from_secs(2))
            .send()
            .await;

        match response {
            Ok(_) => Ok(()),
            Err(e) if e.is_timeout() => {
                debug!("Payment request timed out (expected): {}", e);
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("Failed to send payment: {}", e)),
        }
    }
}
