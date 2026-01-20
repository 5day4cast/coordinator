use anyhow::anyhow;
use async_trait::async_trait;
use base64::Engine;
use bdk_wallet::bitcoin::hashes::{sha256, Hash};
use futures::StreamExt;
use lightning_invoice::Bolt11Invoice;
use log::{debug, info, warn};
use reqwest::{Certificate, Client, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{self, metadata},
    path::Path,
    str::FromStr,
    time::Duration,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{domain::PaymentStatus, LnSettings};

#[derive(Debug, Clone)]
pub struct InvoiceUpdate {
    pub payment_hash: String,
    pub state: InvoiceState,
    pub amt_paid_sat: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PaymentUpdate {
    pub payment_hash: String,
    pub status: PaymentStatus,
    pub failure_reason: Option<String>,
    pub preimage: Option<String>,
}

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
    async fn lookup_payment(&self, r_hash: &str) -> Result<PaymentLookupResponse, anyhow::Error>;
    async fn send_payment(
        &self,
        payout_payment_request: String,
        amount_sats: u64,
        timeout_seconds: u64,
        fee_limit_sat: u64,
    ) -> Result<(), anyhow::Error>;

    async fn subscribe_invoices(&self) -> Result<mpsc::Receiver<InvoiceUpdate>, anyhow::Error>;
    async fn subscribe_payments(&self) -> Result<mpsc::Receiver<PaymentUpdate>, anyhow::Error>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentLookupResponse {
    pub payment_hash: String,
    pub value: String,
    pub creation_date: String,
    pub fee: String,
    pub payment_preimage: Option<String>,
    pub value_sat: String,
    pub value_msat: String,
    pub payment_request: String,
    pub status: PaymentStatus,
    pub fee_sat: String,
    pub fee_msat: String,
    pub creation_time_ns: String,
    pub failure_reason: String,
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

            build_reqwest_tls_client(cert)?
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
    Path::new(file_path).extension().and_then(|s| s.to_str()) == Some("cert")
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
    Path::new(file_path).extension().and_then(|s| s.to_str()) == Some("macaroon")
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

#[derive(Debug, Serialize, Deserialize)]
pub struct SubscribeInvoiceResponse {
    pub r_hash: Option<String>,
    pub state: Option<InvoiceState>,
    pub amt_paid_sat: Option<String>,
    #[serde(default)]
    pub settled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TrackPaymentResponse {
    pub result: Option<PaymentTrackResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentTrackResult {
    pub payment_hash: Option<String>,
    pub status: Option<PaymentStatus>,
    pub failure_reason: Option<String>,
    pub payment_preimage: Option<String>,
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
            .post(format!("{}v2/invoices/cancel", self.base_url))
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
        let hash_bytes = hex::decode(ticket_hash_hex)
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

    async fn lookup_payment(&self, r_hash: &str) -> Result<PaymentLookupResponse, anyhow::Error> {
        let hash_bytes =
            hex::decode(r_hash).map_err(|e| anyhow!("Failed to decode hex hash: {}", e))?;

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
                "{}/v2/router/track/{}?no_inflight_updates=true",
                self.base_url, hash_base64
            ))
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow!("Failed to lookup invoice: {}", response.status()));
        }

        let payment = response.json::<PaymentLookupResponse>().await?;
        Ok(payment)
    }

    async fn send_payment(
        &self,
        payout_payment_request: String,
        amount_sats: u64,
        timeout_seconds: u64,
        fee_limit_sat: u64,
    ) -> Result<(), anyhow::Error> {
        let invoice = Bolt11Invoice::from_str(&payout_payment_request)
            .map_err(|e| anyhow!("invalid invoice: {}", e))?;
        if let Some(val) = invoice.amount_milli_satoshis() {
            if val != (amount_sats * 1000) {
                return Err(anyhow!(
                    "Invoice amount {} does not equal the requested amount {}",
                    val,
                    amount_sats
                ));
            }
        }

        let body = if amount_sats > 0 && invoice.amount_milli_satoshis().is_none() {
            json!({
                "payment_request": payout_payment_request,
                "timeout_seconds": timeout_seconds,
                "fee_limit_sat": fee_limit_sat.to_string(),
                "amt": amount_sats,
                "allow_self_payment": true,
            })
        } else {
            json!({
                "payment_request": payout_payment_request,
                "timeout_seconds": timeout_seconds,
                "fee_limit_sat": fee_limit_sat.to_string(),
                "allow_self_payment": true,
            })
        };

        debug!("sending payment: {:?}", body);
        let url = format!("{}v2/router/send", self.base_url);
        debug!("Making payment request to: {}", url);

        let response = self
            .client
            .post(url)
            .json(&body)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .timeout(Duration::from_secs(timeout_seconds))
            .send()
            .await;

        match response {
            Ok(response) => {
                info!("Payment: {}", response.text().await.unwrap());
                Ok(())
            }
            Err(e) if e.is_timeout() => {
                debug!("Payment request timed out (expected): {}", e);
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("Failed to send payment: {}", e)),
        }
    }

    async fn subscribe_invoices(&self) -> Result<mpsc::Receiver<InvoiceUpdate>, anyhow::Error> {
        let (tx, rx) = mpsc::channel(100);
        let base_url = self.base_url.clone();
        let macaroon = self.macaroon.clone();

        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;

        tokio::spawn(async move {
            let url = format!("{}v1/invoices/subscribe", base_url);
            info!("Starting invoice subscription at {}", url);

            loop {
                if let Err(e) = process_invoice_stream(&client, &url, &macaroon, &tx).await {
                    warn!("Invoice subscription error: {}", e);
                }
                info!("Invoice subscription reconnecting...");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        Ok(rx)
    }

    async fn subscribe_payments(&self) -> Result<mpsc::Receiver<PaymentUpdate>, anyhow::Error> {
        let (tx, rx) = mpsc::channel(100);
        let base_url = self.base_url.clone();
        let macaroon = self.macaroon.clone();

        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;

        tokio::spawn(async move {
            let url = format!("{}v2/router/payments", base_url);
            info!("Starting payment subscription at {}", url);

            loop {
                if let Err(e) = process_payment_stream(&client, &url, &macaroon, &tx).await {
                    warn!("Payment subscription error: {}", e);
                }
                info!("Payment subscription reconnecting...");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        Ok(rx)
    }
}

fn decode_base64_to_hex(encoded: &str) -> Option<String> {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(encoded))
        .ok()
        .map(hex::encode)
}

fn parse_invoice_update(line: &str) -> Option<InvoiceUpdate> {
    let resp: SubscribeInvoiceResponse = serde_json::from_str(line).ok()?;
    let r_hash = resp.r_hash.as_ref()?;
    let state = resp.state.as_ref()?;
    let payment_hash = decode_base64_to_hex(r_hash)?;
    let amt_paid_sat = resp.amt_paid_sat.as_ref().and_then(|s| s.parse().ok());

    Some(InvoiceUpdate {
        payment_hash,
        state: state.clone(),
        amt_paid_sat,
    })
}

fn parse_payment_update(line: &str) -> Option<PaymentUpdate> {
    let resp: TrackPaymentResponse = serde_json::from_str(line).ok()?;
    let result = resp.result?;
    let hash = result.payment_hash.as_ref()?;
    let status = result.status.as_ref()?;
    let payment_hash = decode_base64_to_hex(hash)?;
    let preimage = result
        .payment_preimage
        .as_ref()
        .and_then(|p| decode_base64_to_hex(p));

    Some(PaymentUpdate {
        payment_hash,
        status: status.clone(),
        failure_reason: result.failure_reason.clone(),
        preimage,
    })
}

async fn process_invoice_stream(
    client: &Client,
    url: &str,
    macaroon: &SecretString,
    tx: &mpsc::Sender<InvoiceUpdate>,
) -> Result<(), anyhow::Error> {
    let response = client
        .get(url)
        .header(MACAROON_HEADER, macaroon.expose_secret())
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow!("Subscription failed: {}", response.status()));
    }

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let Ok(text) = String::from_utf8(chunk.to_vec()) else {
            continue;
        };

        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Some(update) = parse_invoice_update(line) else {
                continue;
            };
            debug!("Invoice update: {:?}", update);
            if tx.send(update).await.is_err() {
                return Err(anyhow!("Channel closed"));
            }
        }
    }

    Ok(())
}

async fn process_payment_stream(
    client: &Client,
    url: &str,
    macaroon: &SecretString,
    tx: &mpsc::Sender<PaymentUpdate>,
) -> Result<(), anyhow::Error> {
    let body = json!({ "no_inflight_updates": false });
    let response = client
        .post(url)
        .json(&body)
        .header(MACAROON_HEADER, macaroon.expose_secret())
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow!("Subscription failed: {}", response.status()));
    }

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let Ok(text) = String::from_utf8(chunk.to_vec()) else {
            continue;
        };

        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Some(update) = parse_payment_update(line) else {
                continue;
            };
            debug!("Payment update: {:?}", update);
            if tx.send(update).await.is_err() {
                return Err(anyhow!("Channel closed"));
            }
        }
    }

    Ok(())
}

pub fn extract_payment_hash_from_invoice(payment_request: &str) -> Result<String, anyhow::Error> {
    let invoice = Bolt11Invoice::from_str(payment_request)
        .map_err(|e| anyhow::anyhow!("Failed to parse BOLT11 invoice: {}", e))?;

    let payment_hash = invoice.payment_hash();
    Ok(hex::encode(payment_hash.as_byte_array()))
}

pub fn extract_amount_from_invoice(payment_request: &str) -> Result<Option<u64>, anyhow::Error> {
    let invoice = Bolt11Invoice::from_str(payment_request)
        .map_err(|e| anyhow::anyhow!("Failed to parse BOLT11 invoice: {}", e))?;

    let amt = invoice.amount_milli_satoshis().map(|amount| amount / 1000);
    Ok(amt)
}
