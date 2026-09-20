use anyhow::anyhow;
use async_trait::async_trait;
use base64::Engine;
use bitcoin::hashes::Hash;
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

/// Reserve blocks for payment resolution and publishing the DLC close before
/// the participant's earliest relative-locktime spend. This is a safety margin,
/// not an atomic guarantee across chain reads and LND accepting the request.
pub const PAYOUT_SETTLEMENT_MARGIN_BLOCKS: u32 = 12;
pub const MAX_PAYOUT_CLTV_DELTA: u32 = 144;
const LND_FINAL_CLTV_PADDING: u32 = 3;

#[derive(Debug, Clone, Copy)]
pub struct PaymentDeadline {
    pub max_htlc_expiry_height: u32,
    /// LND must be at least as current as the confirmed-output check.
    pub minimum_chain_height: u32,
}

pub fn payout_htlc_expiry_height(
    outcome_height: u32,
    relative_delta: u16,
) -> Result<u32, anyhow::Error> {
    outcome_height
        .checked_add(u32::from(relative_delta))
        .and_then(|height| height.checked_sub(PAYOUT_SETTLEMENT_MARGIN_BLOCKS + 1))
        .ok_or_else(|| anyhow!("DLC maturity cannot support a safe Lightning payout window"))
}

pub fn payout_cltv_limit(
    deadline: PaymentDeadline,
    current_height: u32,
    final_cltv: u64,
) -> Result<u32, anyhow::Error> {
    if current_height < deadline.minimum_chain_height {
        return Err(anyhow!("LND is behind the checked outcome chain tip"));
    }
    let limit = deadline
        .max_htlc_expiry_height
        .checked_sub(current_height)
        .filter(|limit| *limit > 0)
        .ok_or_else(|| anyhow!("Lightning payout deadline has passed"))?
        .min(MAX_PAYOUT_CLTV_DELTA);
    let minimum = u32::try_from(final_cltv)
        .ok()
        .and_then(|value| value.checked_add(LND_FINAL_CLTV_PADDING))
        .ok_or_else(|| anyhow!("Invoice final CLTV exceeds the payout window"))?;
    if limit < minimum {
        return Err(anyhow!(
            "Insufficient blocks remain for the invoice's final CLTV"
        ));
    }
    Ok(limit)
}

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

    async fn send_payment_before_height(
        &self,
        payout_payment_request: String,
        amount_sats: u64,
        timeout_seconds: u64,
        fee_limit_sat: u64,
        deadline: PaymentDeadline,
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

#[derive(Clone)]
pub struct LnClient {
    pub base_url: Url,
    pub client: ClientWithMiddleware,
    /// Payments must not be transparently retried after their chain check.
    pub(crate) payment_client: Client,
    pub macaroon: SecretString,
}

impl LnClient {
    async fn send_payment_inner(
        &self,
        payout_payment_request: String,
        amount_sats: u64,
        timeout_seconds: u64,
        fee_limit_sat: u64,
        deadline: Option<PaymentDeadline>,
    ) -> Result<(), anyhow::Error> {
        let invoice = Bolt11Invoice::from_str(&payout_payment_request)
            .map_err(|e| anyhow!("invalid invoice: {}", e))?;
        if let Some(val) = invoice.amount_milli_satoshis() {
            if val
                != amount_sats
                    .checked_mul(1000)
                    .ok_or_else(|| anyhow!("Payment amount overflow"))?
            {
                return Err(anyhow!(
                    "Invoice amount {} does not equal the requested amount {}",
                    val,
                    amount_sats
                ));
            }
        }

        let cltv_limit = if let Some(deadline) = deadline {
            // timeout_seconds limits routing attempts, not accepted HTLC lifetime.
            // Verify a current synced tip immediately before asking LND to route.
            let response = self
                .payment_client
                .get(format!("{}v1/getinfo", self.base_url))
                .header(MACAROON_HEADER, self.macaroon.expose_secret())
                .timeout(Duration::from_secs(5))
                .send()
                .await?;
            if !response.status().is_success() {
                return Err(anyhow!("LND chain status unavailable"));
            }
            let info: serde_json::Value = response.json().await?;
            if info["synced_to_chain"].as_bool() != Some(true) {
                return Err(anyhow!("LND is not synchronized to the chain"));
            }
            let height = info["block_height"]
                .as_u64()
                .or_else(|| {
                    info["block_height"]
                        .as_str()
                        .and_then(|value| value.parse().ok())
                })
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| anyhow!("LND returned an invalid chain height"))?;
            Some(payout_cltv_limit(
                deadline,
                height,
                invoice.min_final_cltv_expiry_delta(),
            )?)
        } else {
            None
        };
        let mut body = if amount_sats > 0 && invoice.amount_milli_satoshis().is_none() {
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

        if let Some(limit) = cltv_limit {
            body["cltv_limit"] = json!(limit);
            body["cancelable"] = json!(true);
        }
        debug!("sending payment: {:?}", body);
        let url = format!("{}v2/router/send", self.base_url);
        debug!("Making payment request to: {}", url);

        let response = self
            .payment_client
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
        let payment_client =
            build_ln_http_client(tls_cert.clone(), settings.dangerous_accept_invalid_tls)?;
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
            payment_client,
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
    Ok(ClientBuilder::new(build_ln_http_client(
        tls_cert,
        dangerous_accept_invalid_certs,
    )?)
    .with(RetryTransientMiddleware::new_with_policy(retry_policy))
    .build())
}

fn build_ln_http_client(
    tls_cert: Option<Certificate>,
    dangerous_accept_invalid_certs: bool,
) -> Result<Client, anyhow::Error> {
    let mut builder = Client::builder()
        .tls_built_in_root_certs(true)
        .connect_timeout(Duration::from_secs(5));
    if let Some(tls_cert) = tls_cert {
        builder = builder.add_root_certificate(tls_cert);
    }
    if dangerous_accept_invalid_certs {
        warn!("ln_settings.dangerous_accept_invalid_tls is set: LND certificate verification is disabled");
        builder = builder.danger_accept_invalid_certs(true);
    }
    Ok(builder.build()?)
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

        let memo = format!("c:{}", competition_id);

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
        invoice: String,
        amount_sats: u64,
        timeout_seconds: u64,
        fee_limit_sat: u64,
    ) -> Result<(), anyhow::Error> {
        self.send_payment_inner(invoice, amount_sats, timeout_seconds, fee_limit_sat, None)
            .await
    }

    async fn send_payment_before_height(
        &self,
        invoice: String,
        amount_sats: u64,
        timeout_seconds: u64,
        fee_limit_sat: u64,
        deadline: PaymentDeadline,
    ) -> Result<(), anyhow::Error> {
        self.send_payment_inner(
            invoice,
            amount_sats,
            timeout_seconds,
            fee_limit_sat,
            Some(deadline),
        )
        .await
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

pub(crate) fn invoice_is_expired(invoice: &Bolt11Invoice) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    invoice.would_expire(now)
}

pub fn extract_amount_from_invoice(payment_request: &str) -> Result<Option<u64>, anyhow::Error> {
    let invoice = Bolt11Invoice::from_str(payment_request)
        .map_err(|e| anyhow::anyhow!("Failed to parse BOLT11 invoice: {}", e))?;

    if invoice_is_expired(&invoice) {
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
    use bitcoin::hashes::sha256;

    #[test]
    fn payout_deadline_reserves_resolution_blocks_and_bounds_total_route_cltv() {
        let expiry = payout_htlc_expiry_height(100, 72).unwrap();
        assert_eq!(expiry, 159);
        let deadline = PaymentDeadline {
            max_htlc_expiry_height: expiry,
            minimum_chain_height: 103,
        };
        assert_eq!(payout_cltv_limit(deadline, 103, 40).unwrap(), 56);
        assert_eq!(payout_cltv_limit(deadline, 116, 40).unwrap(), 43);
        assert!(payout_cltv_limit(deadline, 117, 40).is_err());
        assert!(payout_cltv_limit(deadline, 102, 18).is_err());
        assert!(payout_cltv_limit(deadline, 159, 0).is_err());
        assert!(payout_cltv_limit(deadline, 160, 0).is_err());
        assert!(payout_cltv_limit(deadline, 103, u64::MAX).is_err());
        assert!(payout_htlc_expiry_height(u32::MAX, 72).is_err());
        assert!(payout_htlc_expiry_height(1, 1).is_err());
        assert_eq!(
            payout_cltv_limit(
                PaymentDeadline {
                    max_htlc_expiry_height: 10_000,
                    minimum_chain_height: 100,
                },
                100,
                18
            )
            .unwrap(),
            MAX_PAYOUT_CLTV_DELTA
        );
    }

    fn fresh_test_invoice() -> String {
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
        InvoiceBuilder::new(Currency::Regtest)
            .description("bounded payout".into())
            .payment_hash(sha256::Hash::from_byte_array([7; 32]))
            .payment_secret(PaymentSecret([8; 32]))
            .amount_milli_satoshis(10_000)
            .duration_since_epoch(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap(),
            )
            .min_final_cltv_expiry_delta(18)
            .build_signed(|hash| {
                Secp256k1::new()
                    .sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[9; 32]).unwrap())
            })
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn bounded_send_checks_fresh_synced_height_and_does_not_retry_http_failures() {
        use axum::{
            extract::State,
            routing::{get, post},
            Json, Router,
        };
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct RpcState {
            height: Arc<AtomicU32>,
            synced: Arc<AtomicBool>,
            sends: Arc<AtomicUsize>,
            body: Arc<Mutex<serde_json::Value>>,
        }
        let state = RpcState::default();
        state.height.store(100, Ordering::SeqCst);
        let router = Router::new()
            .route(
                "/v1/getinfo",
                get(|State(s): State<RpcState>| async move {
                    Json(json!({"block_height": s.height.load(Ordering::SeqCst),
                    "synced_to_chain": s.synced.load(Ordering::SeqCst)}))
                }),
            )
            .route(
                "/v2/router/send",
                post(
                    |State(s): State<RpcState>, Json(body): Json<serde_json::Value>| async move {
                        s.sends.fetch_add(1, Ordering::SeqCst);
                        *s.body.lock().unwrap() = body;
                        reqwest::StatusCode::BAD_GATEWAY
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = LnClient {
            base_url: Url::parse(&format!("http://{address}/")).unwrap(),
            // General RPC retries must never be used for payment initiation.
            client: build_reqwest_tls_client(None, false).unwrap(),
            payment_client: Client::new(),
            macaroon: SecretString::from("test-macaroon"),
        };
        let deadline = PaymentDeadline {
            max_htlc_expiry_height: 159,
            minimum_chain_height: 100,
        };
        assert!(client
            .send_payment_before_height(fresh_test_invoice(), 10, 2, 1000, deadline)
            .await
            .is_err());
        state.synced.store(true, Ordering::SeqCst);
        for height in [99, 140, 159] {
            state.height.store(height, Ordering::SeqCst);
            assert!(client
                .send_payment_before_height(fresh_test_invoice(), 10, 2, 1000, deadline)
                .await
                .is_err());
        }
        assert_eq!(state.sends.load(Ordering::SeqCst), 0);
        state.height.store(105, Ordering::SeqCst);
        assert!(client
            .send_payment_before_height(fresh_test_invoice(), 10, 2, 1000, deadline)
            .await
            .is_err());
        assert_eq!(state.sends.load(Ordering::SeqCst), 1);
        assert_eq!(state.body.lock().unwrap()["cltv_limit"], 54);
        assert_eq!(state.body.lock().unwrap()["cancelable"], true);
        server.abort();
    }

    /// Runs only against an explicitly supplied disposable regtest LND node.
    /// Environment: LND_PAYOUT_TEST_{URL,TLS_CERT,MACAROON,INVOICE_FILE,
    /// AMOUNT_SATS,DEADLINE_HEIGHT}. The invoice must be fresh and unpaid.
    #[tokio::test]
    #[ignore = "requires funded disposable regtest LND channel and fresh invoice"]
    async fn live_regtest_lnd_bounded_payment_and_preimage() {
        let env = |name: &str| {
            std::env::var(format!("LND_PAYOUT_TEST_{name}"))
                .expect("missing explicit regtest test environment")
        };
        let invoice = fs::read_to_string(env("INVOICE_FILE"))
            .unwrap()
            .trim()
            .to_owned();
        let parsed: Bolt11Invoice = invoice.parse().unwrap();
        assert_eq!(parsed.currency(), lightning_invoice::Currency::Regtest);
        assert!(
            !invoice_is_expired(&parsed),
            "test requires a fresh invoice"
        );
        let amount: u64 = env("AMOUNT_SATS").parse().unwrap();
        assert_eq!(parsed.amount_milli_satoshis(), amount.checked_mul(1000));
        let client = LnClient::new(
            ClientBuilder::new(Client::new()).build(),
            LnSettings {
                base_url: env("URL"),
                tls_cert_path: Some(env("TLS_CERT")),
                macaroon_file_path: env("MACAROON"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let info: serde_json::Value = client
            .payment_client
            .get(format!("{}v1/getinfo", client.base_url))
            .header(MACAROON_HEADER, client.macaroon.expose_secret())
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(info["synced_to_chain"], true);
        let height = u32::try_from(info["block_height"].as_u64().unwrap()).unwrap();
        let hash = extract_payment_hash_from_invoice(&invoice).unwrap();
        assert!(client
            .lookup_payment(&hash)
            .await
            .unwrap_err()
            .is::<PaymentNotFound>());
        for expiry in [
            height,
            height + u32::try_from(parsed.min_final_cltv_expiry_delta()).unwrap() - 1,
        ] {
            assert!(client
                .send_payment_before_height(
                    invoice.clone(),
                    amount,
                    10,
                    1000,
                    PaymentDeadline {
                        max_htlc_expiry_height: expiry,
                        minimum_chain_height: height
                    }
                )
                .await
                .is_err());
            assert!(
                client
                    .lookup_payment(&hash)
                    .await
                    .unwrap_err()
                    .is::<PaymentNotFound>(),
                "rejected deadline must not create an LND payment"
            );
        }
        client
            .send_payment_before_height(
                invoice,
                amount,
                15,
                1000,
                PaymentDeadline {
                    max_htlc_expiry_height: env("DEADLINE_HEIGHT").parse().unwrap(),
                    minimum_chain_height: height,
                },
            )
            .await
            .unwrap();
        let payment = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let payment = client.lookup_payment(&hash).await.unwrap();
                match payment.status {
                    PaymentStatus::Succeeded => break payment,
                    PaymentStatus::Failed => panic!("regtest LND payment failed"),
                    _ => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            }
        })
        .await
        .expect("regtest LND payment did not settle within test deadline");
        assert_eq!(payment.value_sat.parse::<u64>().unwrap(), amount);
        let preimage = hex::decode(payment.payment_preimage.unwrap()).unwrap();
        assert!(
            sha256::Hash::hash(&preimage) == *parsed.payment_hash(),
            "settled LND proof must match invoice payment hash"
        );
    }

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
            payment_client: Client::new(),
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
