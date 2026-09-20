//! LNURL-pay (LUD-06) resolution of Lightning Addresses (LUD-16).
//!
//! A payout address is chosen by the user, so every request here goes to a
//! host the coordinator does not control. The client only speaks HTTPS, never
//! follows redirects, refuses hosts that resolve to private or local
//! addresses and pins the checked addresses for the connection, and bounds
//! response size and time. Metadata is display information, not recipient proof.
//! Both ordinary BOLT11 description forms are supported. Automatic escrow
//! release requires the enclave to fetch and commit its own authenticated invoice;
//! this coordinator-side resolver cannot authenticate invoice origin to Keymeld.

use async_trait::async_trait;
use bitcoin::Network;
use lightning_invoice::{Bolt11Invoice, Currency};
use reqwest::{redirect::Policy, Client, ClientBuilder, Url};
use serde::Deserialize;
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

const MAX_USER_LEN: usize = 64;
const MAX_DOMAIN_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;
/// Includes ordinary provider metadata with an optional image thumbnail.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_INVOICE_BYTES: usize = 16 * 1024;
const MAX_URL_BYTES: usize = 8192;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!("coordinator/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LnurlError {
    #[error("Invalid Lightning Address: {0}")]
    InvalidAddress(String),
    #[error("{0} does not resolve to a public address")]
    NotPublic(String),
    #[error("LNURL request failed: {0}")]
    Request(String),
    #[error("LNURL provider responded with HTTP {0}")]
    Status(u16),
    #[error("LNURL response exceeds {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    #[error("LNURL provider returned an error: {0}")]
    Provider(String),
    #[error("LNURL response is malformed: {0}")]
    Malformed(String),
    #[error("LNURL provider requires unsupported payer data")]
    RequiredPayerData,
    #[error("LNURL redirects are not supported")]
    Redirect,
    #[error("LNURL request timed out")]
    Timeout,
    #[error(
        "Amount {amount_msat} msat is outside the provider's range {min_msat}..={max_msat} msat"
    )]
    AmountOutOfRange {
        amount_msat: u64,
        min_msat: u64,
        max_msat: u64,
    },
    #[error("LNURL provider returned an invalid invoice: {0}")]
    Invoice(String),
    #[error("LNURL provider returned an invoice that does not match the request: {0}")]
    InvoiceMismatch(&'static str),
}

/// A LUD-16 Lightning Address, normalised to lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LightningAddress {
    user: String,
    domain: String,
}

impl LightningAddress {
    /// Parse and normalise `user@domain`.
    ///
    /// Only public DNS names are accepted: IP literals, single-label hosts,
    /// `.onion`, `.local` and similar names never resolve to a payout
    /// provider and would otherwise be fetched by the coordinator.
    pub fn parse(input: &str) -> Result<Self, LnurlError> {
        let input = input.trim().to_ascii_lowercase();
        let (user, domain) = input
            .split_once('@')
            .ok_or_else(|| LnurlError::InvalidAddress("expected user@domain".into()))?;
        if user.is_empty() || user == "." || user == ".." || user.len() > MAX_USER_LEN {
            return Err(LnurlError::InvalidAddress(format!(
                "the user part must be 1 to {MAX_USER_LEN} characters"
            )));
        }
        if !user.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.' | b'+')
        }) {
            return Err(LnurlError::InvalidAddress(
                "the user part may only contain letters, digits, '-', '_', '.' and '+'".into(),
            ));
        }
        check_domain(domain)?;
        Ok(Self {
            user: user.to_string(),
            domain: domain.to_string(),
        })
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// The LUD-16 endpoint that describes how to pay this address.
    pub fn well_known_url(&self) -> String {
        format!("https://{}/.well-known/lnurlp/{}", self.domain, self.user)
    }
}

impl fmt::Display for LightningAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.user, self.domain)
    }
}

impl FromStr for LightningAddress {
    type Err = LnurlError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn check_domain(domain: &str) -> Result<(), LnurlError> {
    let invalid = |reason: &str| LnurlError::InvalidAddress(format!("domain {domain:?} {reason}"));
    if domain.is_empty() || domain.len() > MAX_DOMAIN_LEN {
        return Err(invalid("must be 1 to 253 characters"));
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return Err(invalid("must be a public DNS name"));
    }
    for label in &labels {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(invalid("has an empty or overlong label"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(invalid("may only contain letters, digits, '-' and '.'"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid("has a label starting or ending with '-'"));
        }
    }
    let tld = labels[labels.len() - 1];
    if !tld.bytes().all(|b| b.is_ascii_lowercase()) {
        // Rejects IPv4 literals along with every other non-DNS name.
        return Err(invalid("must end in a letters-only top-level domain"));
    }
    if matches!(
        tld,
        "onion"
            | "local"
            | "localhost"
            | "internal"
            | "lan"
            | "home"
            | "corp"
            | "arpa"
            | "test"
            | "example"
            | "invalid"
    ) {
        return Err(invalid("is not a public DNS name"));
    }
    Ok(())
}

/// A provider's LUD-06 pay request for one Lightning Address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayRequest {
    address: LightningAddress,
    callback: Url,
    min_sendable_msat: u64,
    max_sendable_msat: u64,
    metadata: String,
}

impl PayRequest {
    fn from_response(address: LightningAddress, body: &[u8]) -> Result<Self, LnurlError> {
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(LnurlError::ResponseTooLarge);
        }
        let response: PayRequestResponse = serde_json::from_slice(body)
            .map_err(|e| LnurlError::Malformed(format!("pay request: {e}")))?;
        if let Some(reason) = response.error_reason() {
            return Err(LnurlError::Provider(reason));
        }
        if response.tag.as_deref() != Some("payRequest") {
            return Err(LnurlError::Malformed("tag is not payRequest".into()));
        }
        let callback = response
            .callback
            .as_deref()
            .ok_or_else(|| LnurlError::Malformed("missing callback".into()))?;
        let (min_sendable_msat, max_sendable_msat) =
            match (response.min_sendable, response.max_sendable) {
                (Some(min), Some(max)) => (min, max),
                _ => return Err(LnurlError::Malformed("missing sendable range".into())),
            };
        validate_payer_data(response.payer_data.as_ref())?;
        let metadata = response
            .metadata
            .ok_or_else(|| LnurlError::Malformed("missing metadata".into()))?;
        Self::new(
            address,
            callback,
            min_sendable_msat,
            max_sendable_msat,
            metadata,
        )
    }

    /// A validated pay request: `callback` must be an https URL to a public
    /// destination, the range must be sane, and metadata must be well formed.
    /// Metadata does not authenticate this address or its invoice recipient.
    pub fn new(
        address: LightningAddress,
        callback: &str,
        min_sendable_msat: u64,
        max_sendable_msat: u64,
        metadata: String,
    ) -> Result<Self, LnurlError> {
        let callback =
            Url::parse(callback).map_err(|e| LnurlError::Malformed(format!("callback: {e}")))?;
        validate_url(&callback)?;
        if min_sendable_msat < 1 || min_sendable_msat > max_sendable_msat {
            return Err(LnurlError::Malformed("sendable range is invalid".into()));
        }
        validate_metadata(&metadata)?;
        Ok(Self {
            address,
            callback,
            min_sendable_msat,
            max_sendable_msat,
            metadata,
        })
    }

    pub fn address(&self) -> &LightningAddress {
        &self.address
    }

    pub fn min_sendable_msat(&self) -> u64 {
        self.min_sendable_msat
    }

    pub fn max_sendable_msat(&self) -> u64 {
        self.max_sendable_msat
    }

    /// The provider's metadata exactly as served, for display only.
    pub fn metadata(&self) -> &str {
        &self.metadata
    }

    pub fn check_amount(&self, amount_msat: u64) -> Result<(), LnurlError> {
        if (self.min_sendable_msat..=self.max_sendable_msat).contains(&amount_msat) {
            Ok(())
        } else {
            Err(LnurlError::AmountOutOfRange {
                amount_msat,
                min_msat: self.min_sendable_msat,
                max_msat: self.max_sendable_msat,
            })
        }
    }

    fn invoice_url(&self, amount_msat: u64) -> Url {
        let mut url = self.callback.clone();
        // Preserve opaque provider parameters byte-for-byte, replacing decoded
        // amount keys so providers cannot interpret ambiguous duplicate values.
        let query = {
            let mut decoded = url.query_pairs();
            url.query()
                .unwrap_or_default()
                .split('&')
                .filter(|raw| {
                    raw.is_empty() || decoded.next().is_some_and(|(key, _)| key != "amount")
                })
                .collect::<Vec<_>>()
                .join("&")
        };
        url.set_query(if query.is_empty() { None } else { Some(&query) });
        url.query_pairs_mut()
            .append_pair("amount", &amount_msat.to_string());
        url
    }

    /// Validate a provider invoice's signature, amount, network and expiry.
    /// The description is not recipient evidence, regardless of its encoding.
    pub fn verify_invoice(
        &self,
        invoice: &str,
        amount_msat: u64,
        network: Network,
    ) -> Result<Bolt11Invoice, LnurlError> {
        self.check_amount(amount_msat)?;
        if invoice.len() > MAX_INVOICE_BYTES {
            return Err(LnurlError::Invoice("invoice is too large".into()));
        }
        let invoice = Bolt11Invoice::from_str(invoice.trim())
            .map_err(|e| LnurlError::Invoice(e.to_string()))?;
        if !matches!(
            (network, invoice.currency()),
            (Network::Bitcoin, Currency::Bitcoin)
                | (Network::Testnet, Currency::BitcoinTestnet)
                | (Network::Signet, Currency::Signet)
                | (Network::Regtest, Currency::Regtest)
        ) {
            return Err(LnurlError::InvoiceMismatch("wrong network"));
        }
        if invoice.amount_milli_satoshis() != Some(amount_msat) {
            return Err(LnurlError::InvoiceMismatch("wrong amount"));
        }
        if invoice.is_expired() {
            return Err(LnurlError::InvoiceMismatch("already expired"));
        }
        Ok(invoice)
    }
}

#[derive(Deserialize)]
struct PayRequestResponse {
    status: Option<String>,
    reason: Option<String>,
    tag: Option<String>,
    callback: Option<String>,
    #[serde(rename = "minSendable")]
    min_sendable: Option<u64>,
    #[serde(rename = "maxSendable")]
    max_sendable: Option<u64>,
    metadata: Option<String>,
    #[serde(rename = "payerData")]
    payer_data: Option<serde_json::Value>,
}

impl PayRequestResponse {
    fn error_reason(&self) -> Option<String> {
        (self.status.as_deref() == Some("ERROR")).then(|| {
            self.reason
                .clone()
                .unwrap_or_else(|| "no reason given".into())
        })
    }
}

#[derive(Deserialize)]
struct InvoiceResponse {
    status: Option<String>,
    reason: Option<String>,
    pr: Option<String>,
}

fn validate_payer_data(data: Option<&serde_json::Value>) -> Result<(), LnurlError> {
    if let Some(data) = data {
        let fields = data.as_object().ok_or(LnurlError::RequiredPayerData)?;
        for field in fields.values() {
            let field = field.as_object().ok_or(LnurlError::RequiredPayerData)?;
            if field
                .get("mandatory")
                .is_some_and(|v| v.as_bool() != Some(false))
            {
                return Err(LnurlError::RequiredPayerData);
            }
        }
    }
    Ok(())
}

fn validate_metadata(metadata: &str) -> Result<(), LnurlError> {
    if metadata.len() > MAX_RESPONSE_BYTES {
        return Err(LnurlError::ResponseTooLarge);
    }
    let entries: Vec<Vec<serde_json::Value>> = serde_json::from_str(metadata)
        .map_err(|e| LnurlError::Malformed(format!("metadata: {e}")))?;
    let mut descriptions = 0;
    for entry in &entries {
        let kind = entry
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| LnurlError::Malformed("metadata type is not a string".into()))?;
        if entry.len() < 2 {
            return Err(LnurlError::Malformed("metadata value is missing".into()));
        }
        if kind == "text/plain" {
            if !entry[1].is_string() {
                return Err(LnurlError::Malformed(
                    "metadata description is not a string".into(),
                ));
            }
            descriptions += 1;
        }
        // Unknown metadata types may have non-string values. An identifier,
        // including an untagged alias, is optional display data, never authority.
    }
    if descriptions != 1 {
        return Err(LnurlError::Malformed(
            "metadata must have one text/plain entry".into(),
        ));
    }
    Ok(())
}

/// Resolves Lightning Addresses and fetches invoices for them.
#[async_trait]
pub trait LnurlPay: Send + Sync {
    async fn resolve(&self, address: &LightningAddress) -> Result<PayRequest, LnurlError>;

    /// An invoice for `amount_msat`, verified against the pay request.
    async fn request_invoice(
        &self,
        request: &PayRequest,
        amount_msat: u64,
    ) -> Result<Bolt11Invoice, LnurlError>;
}

/// The production resolver: HTTPS to public hosts only.
pub struct HttpsLnurlPay {
    network: Network,
}

impl HttpsLnurlPay {
    pub fn new(network: Network) -> Self {
        Self { network }
    }
}

#[async_trait]
impl LnurlPay for HttpsLnurlPay {
    async fn resolve(&self, address: &LightningAddress) -> Result<PayRequest, LnurlError> {
        let url = Url::parse(&address.well_known_url())
            .map_err(|e| LnurlError::Malformed(format!("well-known url: {e}")))?;
        let body = fetch_public(&url).await?;
        PayRequest::from_response(address.clone(), &body)
    }

    async fn request_invoice(
        &self,
        request: &PayRequest,
        amount_msat: u64,
    ) -> Result<Bolt11Invoice, LnurlError> {
        request.check_amount(amount_msat)?;
        let body = fetch_public(&request.invoice_url(amount_msat)).await?;
        let response: InvoiceResponse = serde_json::from_slice(&body)
            .map_err(|e| LnurlError::Malformed(format!("invoice response: {e}")))?;
        if response.status.as_deref() == Some("ERROR") {
            return Err(LnurlError::Provider(
                response.reason.unwrap_or_else(|| "no reason given".into()),
            ));
        }
        let invoice = response
            .pr
            .ok_or_else(|| LnurlError::Malformed("missing pr".into()))?;
        request.verify_invoice(&invoice, amount_msat, self.network)
    }
}

/// GET `url` from a host that resolves only to public addresses, without
/// following redirects, returning at most [`MAX_RESPONSE_BYTES`].
///
/// LNURL errors are JSON documents that may come with any status code, so
/// the body is returned for every status and the caller interprets it; a
/// non-JSON body on a failure status surfaces as [`LnurlError::Status`].
async fn fetch_public(url: &Url) -> Result<Vec<u8>, LnurlError> {
    validate_url(url)?;
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        let host = url
            .host_str()
            .ok_or_else(|| LnurlError::Malformed("url has no host".into()))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| LnurlError::Malformed("url has no port".into()))?;
        let addrs = if let Some(ip) = literal_ip(url) {
            vec![SocketAddr::new(ip, port)]
        } else {
            public_addrs(host, port).await?
        };
        validate_addresses(&addrs)?;
        let client = pinned_client_builder(host, &addrs)
            .build()
            .map_err(request_error)?;
        let response = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(request_error)?;
        let status = response.status();
        let body = read_bounded(response).await?;
        if !status.is_success() && serde_json::from_slice::<serde_json::Value>(&body).is_err() {
            return Err(LnurlError::Status(status.as_u16()));
        }
        Ok(body)
    })
    .await
    .map_err(|_| LnurlError::Timeout)?
}

fn validate_url(url: &Url) -> Result<(), LnurlError> {
    if url.as_str().len() > MAX_URL_BYTES
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err(LnurlError::Malformed(
            "URL must be public HTTPS without credentials or fragments".into(),
        ));
    }
    if let Some(ip) = literal_ip(url) {
        if !is_public(ip) {
            return Err(LnurlError::Malformed("URL IP address is not public".into()));
        }
    } else {
        check_domain(url.host_str().unwrap())?;
    }
    Ok(())
}

fn literal_ip(url: &Url) -> Option<IpAddr> {
    url.host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

fn pinned_client_builder(host: &str, addresses: &[SocketAddr]) -> ClientBuilder {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .dns_resolver(Arc::new(NoFallbackDns))
        .resolve_to_addrs(host, addresses)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
}

struct NoFallbackDns;
impl reqwest::dns::Resolve for NoFallbackDns {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "LNURL destination was not pinned",
            )
            .into())
        })
    }
}

fn request_error(error: reqwest::Error) -> LnurlError {
    if error.is_timeout() {
        LnurlError::Timeout
    } else {
        LnurlError::Request(error.without_url().to_string())
    }
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, LnurlError> {
    if response.status().is_redirection() {
        return Err(LnurlError::Redirect);
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(LnurlError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
            return Err(LnurlError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_addresses(addresses: &[SocketAddr]) -> Result<(), LnurlError> {
    if addresses.is_empty() || addresses.iter().any(|a| !is_public(a.ip())) {
        return Err(LnurlError::NotPublic("LNURL host".into()));
    }
    Ok(())
}

async fn public_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, LnurlError> {
    let addresses = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| LnurlError::Timeout)?
        .map_err(|_| LnurlError::NotPublic(host.into()))?
        .collect::<Vec<_>>();
    validate_addresses(&addresses)?;
    Ok(addresses)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            // IANA special-purpose ranges, including shared space and relays.
            // Conservatively reject the complete 192.0.0.0/24 protocol block.
            !(a == 0
                || a == 10
                || (a == 100 && (64..=127).contains(&b))
                || a == 127
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            // Only ordinary global unicast. This excludes local, mapped,
            // compatible, NAT64, multicast, and reserved address spaces.
            (a & 0xe000) == 0x2000
                // Special-use block includes Teredo, benchmarking, and ORCHID.
                && !(a == 0x2001 && b < 0x0200)
                && !(a == 0x2001 && b == 0x0db8)
                && a != 0x2002 // 6to4 can embed a private IPv4 destination.
                && a != 0x3ffe // Retired 6bone, including old Teredo stacks.
                && !(a == 0x3fff && b < 0x1000) // Documentation (RFC 9637).
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        hashes::{sha256, Hash},
        secp256k1::{Secp256k1, SecretKey},
    };
    use lightning_invoice::{InvoiceBuilder, PaymentSecret};
    use reqwest::Response;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn address() -> LightningAddress {
        LightningAddress::parse("Alice+Prize@Cash.App").unwrap()
    }

    fn request_json() -> serde_json::Value {
        json!({"tag":"payRequest", "callback":"https://invoices.provider.com/pay?token=secret&amount=1",
            "minSendable":1000, "maxSendable":100_000_000,
            "metadata":"[[\"text/plain\",\"payout\"]]"})
    }

    fn request() -> PayRequest {
        PayRequest::from_response(address(), request_json().to_string().as_bytes()).unwrap()
    }

    fn invoice(
        amount_msat: Option<u64>,
        hashed: bool,
        currency: Currency,
        expired: bool,
    ) -> String {
        let builder = InvoiceBuilder::new(currency)
            .payment_hash(sha256::Hash::hash(&[0x22; 32]))
            .payment_secret(PaymentSecret([0x33; 32]))
            .min_final_cltv_expiry_delta(18);
        let builder = if hashed {
            // Deliberately unrelated to the LNURL metadata.
            builder.description_hash(sha256::Hash::hash(b"provider invoice description"))
        } else {
            builder.description("ordinary payout".into())
        };
        let builder = if expired {
            builder.duration_since_epoch(Duration::from_secs(1))
        } else {
            builder.current_timestamp()
        };
        let builder = match amount_msat {
            Some(amount) => builder.amount_milli_satoshis(amount),
            None => builder,
        };
        builder
            .build_signed(|hash| {
                Secp256k1::new()
                    .sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[0x11; 32]).unwrap())
            })
            .unwrap()
            .to_string()
    }

    #[test]
    fn supports_tagged_addresses_and_refuses_path_or_authority_injection() {
        assert_eq!(address().to_string(), "alice+prize@cash.app");
        assert_eq!(
            address().well_known_url(),
            "https://cash.app/.well-known/lnurlp/alice+prize"
        );
        for bad in [
            "alice",
            "@cash.app",
            "alice@",
            ".@cash.app",
            "..@cash.app",
            "alice/../bob@cash.app",
            "alice%2fbob@cash.app",
            "alice@@cash.app",
            "alice@localhost",
            "alice@127.0.0.1",
            "alice@2130706433",
            "alice@0x7f000001",
            "alice@[::1]",
            "alice@wallet.onion",
            "alice@printer.local",
            "alice@-bad.com",
            "alice@cash.app:8443",
            "alice@cash.app/x",
            "alice@cash.app?x=y",
        ] {
            assert!(LightningAddress::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ordinary_invoices_require_signature_exact_amount_network_and_expiry() {
        let request = request();
        for hashed in [false, true] {
            let good = invoice(Some(21_000), hashed, Currency::Regtest, false);
            assert_eq!(
                request
                    .verify_invoice(&good, 21_000, Network::Regtest)
                    .unwrap()
                    .amount_milli_satoshis(),
                Some(21_000)
            );
            assert_eq!(
                request.verify_invoice(&good, 22_000, Network::Regtest),
                Err(LnurlError::InvoiceMismatch("wrong amount"))
            );
            for network in [Network::Bitcoin, Network::Testnet4] {
                assert_eq!(
                    request.verify_invoice(&good, 21_000, network),
                    Err(LnurlError::InvoiceMismatch("wrong network"))
                );
            }
        }
        for amount in [None, Some(20_000), Some(21_001), Some(22_000)] {
            assert_eq!(
                request.verify_invoice(
                    &invoice(amount, false, Currency::Regtest, false),
                    21_000,
                    Network::Regtest
                ),
                Err(LnurlError::InvoiceMismatch("wrong amount"))
            );
        }
        assert_eq!(
            request.verify_invoice(
                &invoice(Some(21_000), false, Currency::Regtest, true),
                21_000,
                Network::Regtest
            ),
            Err(LnurlError::InvoiceMismatch("already expired"))
        );
        assert!(matches!(
            request.verify_invoice("lnbcrt1garbage", 21_000, Network::Regtest),
            Err(LnurlError::Invoice(_))
        ));
        assert!(matches!(
            request.verify_invoice(&"x".repeat(MAX_INVOICE_BYTES + 1), 21_000, Network::Regtest),
            Err(LnurlError::Invoice(_))
        ));
    }

    #[test]
    fn metadata_is_display_data_and_accepts_thumbnails_and_untagged_identifiers() {
        for metadata in [
            json!([["text/plain", "payout"]]),
            json!([
                ["text/plain", "payout"],
                ["text/identifier", "alice@cash.app"]
            ]),
            json!([
                ["text/plain", "payout"],
                ["text/identifier", "different@cash.app"]
            ]),
            json!([["text/plain","payout"],["image/png;base64","a".repeat(136_536)],
                ["future/object",{"version":1}]]),
        ] {
            let mut response = request_json();
            response["metadata"] = json!(metadata.to_string());
            PayRequest::from_response(address(), response.to_string().as_bytes()).unwrap();
        }
        for metadata in [
            "{}",
            "[]",
            "[1]",
            "[[\"text/plain\",7]]",
            "[[\"text/plain\",\"a\"],[\"text/plain\",\"b\"]]",
        ] {
            assert!(validate_metadata(metadata).is_err(), "{metadata}");
        }
    }

    #[test]
    fn cross_host_callbacks_preserve_opaque_parameters_and_replace_amount() {
        let mut response = request_json();
        response["callback"] =
            json!("https://callback.other.com/pay?token=a%2fb%20c&&am%6funt=1&bare&amount=2");
        let request =
            PayRequest::from_response(address(), response.to_string().as_bytes()).unwrap();
        assert_eq!(
            request.invoice_url(42_000).as_str(),
            "https://callback.other.com/pay?token=a%2fb%20c&&bare&amount=42000"
        );
        for url in [
            "http://wallet.com/pay",
            "https://user@wallet.com/pay",
            "https://:secret@wallet.com/pay",
            "https://wallet.com/pay#fragment",
            "https://wallet.com:0/pay",
            "https://127.0.0.1/pay",
            "https://2130706433/pay",
            "https://[::1]/pay",
            "https://[::ffff:8.8.8.8]/pay",
            "https://provider.onion/pay",
        ] {
            response["callback"] = json!(url);
            assert!(
                PayRequest::from_response(address(), response.to_string().as_bytes()).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn mandatory_payer_data_stops_flow_but_optional_extensions_are_ignored() {
        for payer_data in [
            json!({"email":{"mandatory":true}}),
            json!({"unknown":{"mandatory":true}}),
            json!({"email":{"mandatory":"false"}}),
            json!({"email":{"mandatory":null}}),
            json!({"email":false}),
            json!([]),
        ] {
            let mut response = request_json();
            response["payerData"] = payer_data;
            assert_eq!(
                PayRequest::from_response(address(), response.to_string().as_bytes()),
                Err(LnurlError::RequiredPayerData)
            );
        }
        for payer_data in [json!({"email":{"mandatory":false}}), json!({"email":{}})] {
            let mut response = request_json();
            response["payerData"] = payer_data;
            response["allowsNostr"] = json!(true);
            response["futureField"] = json!({"version":7});
            let request =
                PayRequest::from_response(address(), response.to_string().as_bytes()).unwrap();
            assert!(!request.invoice_url(42_000).as_str().contains("payerdata"));
        }
    }

    #[test]
    fn provider_ranges_and_error_responses_are_checked() {
        let request = request();
        for amount in [0, 999, 100_000_001] {
            assert!(matches!(
                request.check_amount(amount),
                Err(LnurlError::AmountOutOfRange { .. })
            ));
        }
        for (key, value) in [
            ("minSendable", json!(0)),
            ("maxSendable", json!(999)),
            ("tag", json!("withdrawRequest")),
        ] {
            let mut response = request_json();
            response[key] = value;
            assert!(PayRequest::from_response(address(), response.to_string().as_bytes()).is_err());
        }
        assert_eq!(
            PayRequest::from_response(address(), br#"{"status":"ERROR","reason":"unavailable"}"#),
            Err(LnurlError::Provider("unavailable".into()))
        );
    }
    #[test]
    fn refuses_nonpublic_ipv4_and_ipv6() {
        for address in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.9",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:8.8.8.8",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::1",
            "100::1",
            "2001::7f00:1",
            "2001:2::1",
            "2001:20::1",
            "2001:db8::1",
            "2002:7f00:1::1",
            "3ffe:831f::1",
            "3fff::1",
            "3fff:fff::1",
            "5f00::1",
            "fc00::1",
            "fd00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn accepts_public_destinations_and_special_range_boundaries() {
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "2001:200::1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "2a00:1450::1",
        ] {
            assert!(is_public(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn rejects_empty_or_mixed_dns_answers() {
        let public = "8.8.8.8:443".parse().unwrap();
        let private = "169.254.169.254:443".parse().unwrap();
        assert!(validate_addresses(&[]).is_err());
        assert!(validate_addresses(&[public, private]).is_err());
        assert!(validate_addresses(&[private, public]).is_err());
        assert!(validate_addresses(&[public]).is_ok());
    }

    // HTTP is enabled only for these loopback fixtures. Production always uses
    // HTTPS and validates every address before constructing its pinned client.
    async fn fixture(response: Vec<u8>) -> Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            // Oversized responses may cause the client to close before all data
            // is sent, which is the expected safety behavior under test.
            let _ = stream.write_all(&response).await;
        });
        pinned_client_builder("lnurl-fixture.invalid", &[address])
            .https_only(false)
            .build()
            .unwrap()
            .get(format!("http://lnurl-fixture.invalid:{}/", address.port()))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn pinned_client_uses_supplied_address_and_accepts_lnurl_error_body() {
        // .invalid cannot resolve via DNS. The fixture succeeds only if the
        // production client builder uses the pin. LUD-01 permits this HTTP 400.
        let body = br#"{"status":"ERROR","reason":"unavailable"}"#;
        let mut response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let bytes = read_bounded(fixture(response).await).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ERROR");
    }

    #[tokio::test]
    async fn pinned_client_refuses_dns_fallback_and_redacts_request_urls() {
        let client = pinned_client_builder("pinned.invalid", &["8.8.8.8:443".parse().unwrap()])
            .https_only(false)
            .build()
            .unwrap();
        let error = client
            .get("http://localhost/?token=secret")
            .send()
            .await
            .unwrap_err();
        assert!(error.is_connect());
        let mut cause: &dyn std::error::Error = &error;
        while let Some(source) = cause.source() {
            cause = source;
        }
        assert_eq!(cause.to_string(), "LNURL destination was not pinned");
        let LnurlError::Request(error) = request_error(error) else {
            panic!("expected a transport error");
        };
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn refuses_redirect_without_following_it() {
        let response = fixture(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/secret\r\nContent-Length: 2\r\n\r\n{}"
                .to_vec(),
        )
        .await;
        assert!(matches!(
            read_bounded(response).await,
            Err(LnurlError::Redirect)
        ));
    }

    #[tokio::test]
    async fn times_out_while_reading_stalled_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let response = pinned_client_builder("lnurl-fixture.invalid", &[address])
            .https_only(false)
            .read_timeout(Duration::from_millis(100))
            .build()
            .unwrap()
            .get(format!("http://lnurl-fixture.invalid:{}/", address.port()))
            .send()
            .await
            .unwrap();
        let result = read_bounded(response).await;
        server.abort();
        assert!(matches!(result, Err(LnurlError::Timeout)));
    }

    #[tokio::test]
    async fn refuses_declared_oversized_body() {
        let response = fixture(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_RESPONSE_BYTES + 1
            )
            .into_bytes(),
        )
        .await;
        assert!(matches!(
            read_bounded(response).await,
            Err(LnurlError::ResponseTooLarge)
        ));
    }

    #[tokio::test]
    async fn refuses_streamed_oversized_body_without_content_length() {
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..=MAX_RESPONSE_BYTES / 1024 {
            response.extend_from_slice(b"400\r\n");
            response.extend_from_slice(&[b' '; 1024]);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        assert!(matches!(
            read_bounded(fixture(response).await).await,
            Err(LnurlError::ResponseTooLarge)
        ));
    }

    #[tokio::test]
    async fn accepts_body_exactly_at_size_limit() {
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {MAX_RESPONSE_BYTES}\r\n\r\n{{}}")
                .into_bytes();
        response.extend(std::iter::repeat_n(b' ', MAX_RESPONSE_BYTES - 2));
        let bytes = read_bounded(fixture(response).await).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }
}
