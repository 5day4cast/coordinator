use anyhow::anyhow;
use async_trait::async_trait;
use base64::Engine;
use bitcoin::hashes::{sha256, Hash};
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

/// LND has no payment attempt for this hash. This is the only lookup failure
/// for which the payout watcher may initiate the stored invoice again.
#[derive(Debug, thiserror::Error)]
#[error("LND payment not found")]
pub struct PaymentNotFound;

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
        let tls_cert = settings
            .tls_cert_path
            .as_deref()
            .map(get_tls_cert)
            .transpose()?;
        let client = match (tls_cert, settings.dangerous_accept_invalid_tls) {
            (None, false) => {
                info!("No LND tls cert configured; verifying against the system roots");
                client
            }
            (tls_cert, dangerous) => build_reqwest_tls_client(tls_cert, dangerous)?,
        };
        Ok(Self {
            base_url: Url::parse(&settings.base_url)?,
            client,
            macaroon,
        })
    }
}

/// Every LND request carries the admin macaroon, so the connection must be
/// verified: against the pinned `tls.cert` when one is configured, otherwise
/// the system roots. `dangerous_accept_invalid_certs` exists for local
/// development against a self-signed cert without matching SANs; config
/// refuses it on mainnet.
pub fn build_reqwest_tls_client(
    tls_cert: Option<Certificate>,
    dangerous_accept_invalid_certs: bool,
) -> Result<ClientWithMiddleware, anyhow::Error> {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    let mut builder = Client::builder().tls_built_in_root_certs(true);
    if let Some(tls_cert) = tls_cert {
        builder = builder.add_root_certificate(tls_cert);
    }
    if dangerous_accept_invalid_certs {
        warn!("ln_settings.dangerous_accept_invalid_tls is set: LND certificate verification is disabled");
        builder = builder.danger_accept_invalid_certs(true);
    }
    let client = ClientBuilder::new(builder.build()?)
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
            .get(self.base_url.join(&format!(
                "v2/router/track/{}?no_inflight_updates=false",
                hash_base64
            ))?)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(PaymentNotFound.into());
        }
        if !response.status().is_success() {
            return Err(anyhow!("Failed to lookup payment: {}", response.status()));
        }

        // TrackPaymentV2 is a stream, even for an already settled payment.
        // Read the current state without waiting for an in-flight payment to end.
        let mut stream = response.bytes_stream();
        let mut lines = JsonLines::default();
        while let Some(chunk) = stream.next().await {
            lines.push(&chunk?)?;
            if let Some(line) = lines.next_line() {
                return parse_payment_lookup(&line);
            }
        }
        parse_payment_lookup(&lines.buffer)
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

        // `/v2/router/send` streams updates until the payment resolves. A slow
        // payment hits the request timeout while still in flight; the payout
        // stays pending and the payment subscriber records how it ends.
        match response {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    return Err(anyhow!("Payment request failed with {}", status));
                }
                // Discard completed stream messages as they arrive; neither a
                // long payment history nor an unterminated message may grow
                // memory without a bound. Reconciliation owns terminal state.
                let mut stream = response.bytes_stream();
                let mut lines = JsonLines::default();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(chunk) => lines.push(&chunk)?,
                        Err(error) if error.is_timeout() => return Ok(()),
                        Err(error) => return Err(error.into()),
                    }
                    while let Some(line) = lines.next_line() {
                        let message: serde_json::Value = serde_json::from_slice(&line)?;
                        if message.get("error").is_some() {
                            return Err(anyhow!("LND payment stream reported an error"));
                        }
                    }
                }
                Ok(())
            }
            Err(e) if e.is_timeout() => {
                debug!("Payment request timed out (expected): {}", e);
                Ok(())
            }
            Err(e) => Err(anyhow!("Failed to send payment: {}", e)),
        }
    }

    async fn subscribe_invoices(&self) -> Result<mpsc::Receiver<InvoiceUpdate>, anyhow::Error> {
        let (tx, rx) = mpsc::channel(100);
        let base_url = self.base_url.clone();
        let macaroon = self.macaroon.clone();
        // The same verified client as every other LND call.
        let client = self.client.clone();

        tokio::spawn(async move {
            let url = format!("{}v1/invoices/subscribe", base_url);
            info!("Starting invoice subscription at {}", url);

            // Ends when the subscriber drops its receiver, which shutdown does;
            // until then, reconnect after a short pause.
            while !tx.is_closed() {
                tokio::select! {
                    result = process_invoice_stream(&client, &url, &macaroon, &tx) => {
                        if let Err(e) = result { warn!("Invoice subscription error: {}", e); }
                    }
                    _ = tx.closed() => break,
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        info!("Invoice subscription reconnecting...");
                    }
                    _ = tx.closed() => {}
                }
            }
            info!("Invoice subscription stopped");
        });

        Ok(rx)
    }

    async fn subscribe_payments(&self) -> Result<mpsc::Receiver<PaymentUpdate>, anyhow::Error> {
        let (tx, rx) = mpsc::channel(100);
        let base_url = self.base_url.clone();
        let macaroon = self.macaroon.clone();
        // The same verified client as every other LND call.
        let client = self.client.clone();

        tokio::spawn(async move {
            let url = format!("{}v2/router/payments", base_url);
            info!("Starting payment subscription at {}", url);

            // Ends when the subscriber drops its receiver, which shutdown does;
            // until then, reconnect after a short pause.
            while !tx.is_closed() {
                tokio::select! {
                    result = process_payment_stream(&client, &url, &macaroon, &tx) => {
                        if let Err(e) = result { warn!("Payment subscription error: {}", e); }
                    }
                    _ = tx.closed() => break,
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        info!("Payment subscription reconnecting...");
                    }
                    _ = tx.closed() => {}
                }
            }
            info!("Payment subscription stopped");
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
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let resp: SubscribeInvoiceResponse =
        serde_json::from_value(value.get("result").cloned().unwrap_or(value)).ok()?;
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
    // Payment.payment_hash and payment_preimage are hex strings in LND's
    // schema; Invoice.r_hash is a bytes field and uses base64 instead.
    let payment_hash = hex::decode(hash).ok().filter(|hash| hash.len() == 32)?;
    let preimage = result.payment_preimage.as_ref().and_then(|value| {
        hex::decode(value)
            .ok()
            .filter(|bytes| bytes.len() == 32)
            .map(hex::encode)
    });

    Some(PaymentUpdate {
        payment_hash: hex::encode(payment_hash),
        status: status.clone(),
        failure_reason: result.failure_reason.clone(),
        preimage,
    })
}

async fn process_invoice_stream(
    client: &ClientWithMiddleware,
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
    let mut lines = JsonLines::default();
    while let Some(chunk) = stream.next().await {
        lines.push(&chunk?)?;
        while let Some(line) = lines.next_line() {
            let line = std::str::from_utf8(&line)?;
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
    client: &ClientWithMiddleware,
    url: &str,
    macaroon: &SecretString,
    tx: &mpsc::Sender<PaymentUpdate>,
) -> Result<(), anyhow::Error> {
    let response = client
        .get(format!("{}?no_inflight_updates=false", url))
        .header(MACAROON_HEADER, macaroon.expose_secret())
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow!("Subscription failed: {}", response.status()));
    }

    let mut stream = response.bytes_stream();
    let mut lines = JsonLines::default();
    while let Some(chunk) = stream.next().await {
        lines.push(&chunk?)?;
        while let Some(line) = lines.next_line() {
            let line = std::str::from_utf8(&line)?;
            let Some(update) = parse_payment_update(line) else {
                continue;
            };
            debug!(
                "Payment {} status: {:?}",
                update.payment_hash, update.status
            );
            if tx.send(update).await.is_err() {
                return Err(anyhow!("Channel closed"));
            }
        }
    }

    Ok(())
}

// HTTP chunks have no relation to gRPC Gateway's newline-delimited messages.
// Keep bytes until a full line is available, including split UTF-8 characters.
const MAX_STREAM_BUFFER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct JsonLines {
    buffer: Vec<u8>,
}

impl JsonLines {
    fn push(&mut self, bytes: &[u8]) -> Result<(), anyhow::Error> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_STREAM_BUFFER_BYTES {
            return Err(anyhow!("LND stream message exceeds buffer limit"));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn next_line(&mut self) -> Option<Vec<u8>> {
        loop {
            let end = self.buffer.iter().position(|byte| *byte == b'\n')?;
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if !line.iter().all(u8::is_ascii_whitespace) {
                return Some(line);
            }
        }
    }
}

fn parse_payment_lookup(line: &[u8]) -> Result<PaymentLookupResponse, anyhow::Error> {
    let value: serde_json::Value = serde_json::from_slice(line)?;
    if let Some(error) = value.get("error") {
        if error.get("code").and_then(serde_json::Value::as_i64) == Some(5) {
            return Err(PaymentNotFound.into());
        }
        return Err(anyhow!("LND payment lookup failed"));
    }
    let payment: PaymentLookupResponse =
        serde_json::from_value(value.get("result").cloned().unwrap_or(value))?;
    Ok(payment)
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

    if invoice.is_expired() {
        return Err(anyhow!("Lightning invoice has expired"));
    }

    match invoice.amount_milli_satoshis() {
        Some(amount) if amount % 1000 != 0 => Err(anyhow!("Invoice amount must be whole satoshis")),
        amount => Ok(amount.map(|amount| amount / 1000)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payment_json() -> serde_json::Value {
        json!({
            "payment_hash": hex::encode([7; 32]), "status": "IN_FLIGHT",
            "value": "10", "creation_date": "0", "fee": "0",
            "value_sat": "10", "value_msat": "10000", "payment_request": "invoice",
            "fee_sat": "0", "fee_msat": "0", "creation_time_ns": "0",
            "failure_reason": "FAILURE_REASON_NONE"
        })
    }

    #[test]
    fn subscription_messages_survive_every_possible_chunk_boundary() {
        let invoice = json!({"result": {
            "r_hash": base64::engine::general_purpose::STANDARD.encode([7; 32]),
            "state": "ACCEPTED", "amt_paid_sat": "10", "memo": "météo"
        }});
        let payment = json!({"result": {
            "payment_hash": hex::encode([7; 32]), "status": "SUCCEEDED",
            "payment_preimage": hex::encode([8; 32])
        }});
        let bytes = format!("{invoice}\n{payment}\n").into_bytes();
        for split in 0..=bytes.len() {
            let mut decoder = JsonLines::default();
            let mut lines = Vec::new();
            for chunk in [&bytes[..split], &bytes[split..]] {
                decoder.push(chunk).unwrap();
                while let Some(line) = decoder.next_line() {
                    lines.push(String::from_utf8(line).unwrap());
                }
            }
            assert_eq!(lines.len(), 2);
            let invoice = parse_invoice_update(&lines[0]).unwrap();
            assert_eq!(invoice.payment_hash, hex::encode([7; 32]));
            assert_eq!(invoice.state, InvoiceState::Accepted);
            let payment = parse_payment_update(&lines[1]).unwrap();
            assert_eq!(payment.payment_hash, hex::encode([7; 32]));
            assert_eq!(payment.status, PaymentStatus::Succeeded);
            assert_eq!(payment.preimage, Some(hex::encode([8; 32])));
        }
    }

    #[test]
    fn unterminated_stream_messages_have_a_bounded_buffer() {
        let mut lines = JsonLines::default();
        lines.push(&vec![b'x'; MAX_STREAM_BUFFER_BYTES]).unwrap();
        assert!(lines.push(b"x").is_err());
        assert_eq!(lines.buffer.len(), MAX_STREAM_BUFFER_BYTES);
    }

    #[test]
    fn payment_not_found_is_distinct_from_transient_or_malformed_responses() {
        assert!(
            parse_payment_lookup(br#"{"error":{"code":5,"message":"payment not found"}}"#)
                .unwrap_err()
                .is::<PaymentNotFound>()
        );
        assert!(
            !parse_payment_lookup(br#"{"error":{"code":14,"message":"unavailable"}}"#)
                .unwrap_err()
                .is::<PaymentNotFound>()
        );
        assert!(!parse_payment_lookup(b"invalid")
            .unwrap_err()
            .is::<PaymentNotFound>());
        let wrapped = json!({"result": payment_json()});
        let response = parse_payment_lookup(&serde_json::to_vec(&wrapped).unwrap()).unwrap();
        assert_eq!(response.status, PaymentStatus::InFlight);
    }

    #[tokio::test]
    async fn payment_lookup_returns_the_current_state_without_waiting_for_stream_end() {
        use axum::{body::Body, response::Response, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/v2/router/track/{hash}",
            get(|| async {
                let line = format!("{}\n", json!({"result": payment_json()}));
                let body = futures::stream::once(async { Ok::<_, std::io::Error>(line) })
                    .chain(futures::stream::pending());
                Response::new(Body::from_stream(body))
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = LnClient {
            base_url: Url::parse(&format!("http://{address}/")).unwrap(),
            client: ClientBuilder::new(Client::new()).build(),
            macaroon: SecretString::from("test-macaroon"),
        };
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            client.lookup_payment(&hex::encode([7; 32])),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status, PaymentStatus::InFlight);
        assert_eq!(response.payment_hash, hex::encode([7; 32]));
        server.abort();
    }

    #[test]
    fn payout_amount_validation_rejects_fractional_satoshis() {
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
        let invoice = |amount, timestamp| {
            InvoiceBuilder::new(Currency::Regtest)
                .description("payout".into())
                .payment_hash(sha256::Hash::from_byte_array([7; 32]))
                .payment_secret(PaymentSecret([8; 32]))
                .amount_milli_satoshis(amount)
                .duration_since_epoch(timestamp)
                .min_final_cltv_expiry_delta(18)
                .build_signed(|hash| {
                    Secp256k1::new()
                        .sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[9; 32]).unwrap())
                })
                .unwrap()
                .to_string()
        };
        assert_eq!(
            extract_amount_from_invoice(&invoice(
                10_000,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
            ))
            .unwrap(),
            Some(10)
        );
        assert!(extract_amount_from_invoice(&invoice(
            10_999,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
        ))
        .is_err());
        assert!(extract_amount_from_invoice(&invoice(10_000, Duration::from_secs(1))).is_err());
    }
}
