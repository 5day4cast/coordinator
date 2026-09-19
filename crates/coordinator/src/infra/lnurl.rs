//! LNURL-pay (LUD-06) resolution of Lightning Addresses (LUD-16).
//!
//! A payout address is chosen by the user, so every request here goes to a
//! host the coordinator does not control. The client only speaks HTTPS, never
//! follows redirects, refuses hosts that resolve to private or local
//! addresses and pins the checked addresses for the connection, and bounds
//! response size and time. The provider's metadata is kept verbatim: an
//! invoice only counts as payment to the address if its description hash is
//! the hash of that metadata, which is also what the Keymeld enclave checks
//! before releasing a payout preimage.

use async_trait::async_trait;
use bitcoin::{
    hashes::{sha256, Hash as _},
    Network,
};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};
use reqwest::{redirect::Policy, Client, Url};
use serde::Deserialize;
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
    time::Duration,
};

const MAX_USER_LEN: usize = 64;
const MAX_DOMAIN_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;
/// LNURL responses are small JSON documents; anything larger is not one.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
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
    #[error("LNURL metadata does not name this Lightning Address")]
    IdentifierMismatch,
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
        if user.is_empty() || user.len() > MAX_USER_LEN {
            return Err(LnurlError::InvalidAddress(format!(
                "the user part must be 1 to {MAX_USER_LEN} characters"
            )));
        }
        if !user.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        }) {
            return Err(LnurlError::InvalidAddress(
                "the user part may only contain letters, digits, '-', '_' and '.'".into(),
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
    metadata_hash: [u8; 32],
}

impl PayRequest {
    fn from_response(address: LightningAddress, body: &[u8]) -> Result<Self, LnurlError> {
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
    /// DNS name, the range must be sane, and `metadata` must name `address`.
    pub fn new(
        address: LightningAddress,
        callback: &str,
        min_sendable_msat: u64,
        max_sendable_msat: u64,
        metadata: String,
    ) -> Result<Self, LnurlError> {
        let callback =
            Url::parse(callback).map_err(|e| LnurlError::Malformed(format!("callback: {e}")))?;
        if callback.scheme() != "https" {
            return Err(LnurlError::Malformed("callback is not https".into()));
        }
        let callback_host = callback
            .host_str()
            .ok_or_else(|| LnurlError::Malformed("callback has no host".into()))?;
        check_domain(callback_host)
            .map_err(|_| LnurlError::Malformed("callback host is not a public DNS name".into()))?;
        if min_sendable_msat < 1 || min_sendable_msat > max_sendable_msat {
            return Err(LnurlError::Malformed("sendable range is invalid".into()));
        }
        check_metadata_identifier(&metadata, &address)?;
        let metadata_hash = sha256::Hash::hash(metadata.as_bytes()).to_byte_array();
        Ok(Self {
            address,
            callback,
            min_sendable_msat,
            max_sendable_msat,
            metadata,
            metadata_hash,
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

    /// The provider's metadata exactly as served; its SHA-256 is the
    /// description hash every invoice for this address must carry.
    pub fn metadata(&self) -> &str {
        &self.metadata
    }

    pub fn metadata_hash(&self) -> [u8; 32] {
        self.metadata_hash
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
        url.query_pairs_mut()
            .append_pair("amount", &amount_msat.to_string());
        url
    }

    /// Check that `invoice` pays `amount_msat` to this address on `network`.
    pub fn verify_invoice(
        &self,
        invoice: &str,
        amount_msat: u64,
        network: Network,
    ) -> Result<Bolt11Invoice, LnurlError> {
        let invoice = Bolt11Invoice::from_str(invoice.trim())
            .map_err(|e| LnurlError::Invoice(e.to_string()))?;
        if invoice.network() != network {
            return Err(LnurlError::InvoiceMismatch("wrong network"));
        }
        if invoice.amount_milli_satoshis() != Some(amount_msat) {
            return Err(LnurlError::InvoiceMismatch("wrong amount"));
        }
        match invoice.description() {
            Bolt11InvoiceDescriptionRef::Hash(hash)
                if hash.0.to_byte_array() == self.metadata_hash => {}
            _ => {
                return Err(LnurlError::InvoiceMismatch(
                    "description hash is not the metadata hash",
                ))
            }
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

/// LUD-16 requires the metadata to name the address it was fetched for, so a
/// provider cannot serve one user's metadata for another user's address.
fn check_metadata_identifier(metadata: &str, address: &LightningAddress) -> Result<(), LnurlError> {
    let entries: Vec<Vec<serde_json::Value>> = serde_json::from_str(metadata)
        .map_err(|e| LnurlError::Malformed(format!("metadata: {e}")))?;
    let expected = address.to_string();
    let mut has_plain = false;
    let mut names_address = false;
    for entry in &entries {
        let (Some(kind), Some(value)) = (
            entry.first().and_then(|v| v.as_str()),
            entry.get(1).and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        match kind {
            "text/plain" => has_plain = true,
            "text/identifier" | "text/email" => {
                names_address |= value.trim().eq_ignore_ascii_case(&expected);
            }
            _ => {}
        }
    }
    if !has_plain {
        return Err(LnurlError::Malformed(
            "metadata has no text/plain entry".into(),
        ));
    }
    if !names_address {
        return Err(LnurlError::IdentifierMismatch);
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
    if url.scheme() != "https" {
        return Err(LnurlError::Malformed("url is not https".into()));
    }
    let host = url
        .host_str()
        .ok_or_else(|| LnurlError::Malformed("url has no host".into()))?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = public_addrs(&host, port).await?;
    let client = Client::builder()
        .https_only(true)
        .redirect(Policy::none())
        .resolve_to_addrs(&host, &addrs)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| LnurlError::Request(e.to_string()))?;
    let response = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| LnurlError::Request(e.to_string()))?;
    let status = response.status();
    let body = read_bounded(response).await?;
    if !status.is_success() && serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return Err(LnurlError::Status(status.as_u16()));
    }
    Ok(body)
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, LnurlError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(LnurlError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| LnurlError::Request(e.to_string()))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(LnurlError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Resolve `host` and require every address to be public, so a name that
/// mixes public and internal records is refused rather than half-trusted.
async fn public_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, LnurlError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| LnurlError::NotPublic(format!("{host}: {e}")))?
        .collect();
    if addrs.is_empty() || addrs.iter().any(|addr| !is_public(addr.ip())) {
        return Err(LnurlError::NotPublic(host.to_string()));
    }
    Ok(addrs)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || a == 0
        || (a == 100 && (64..=127).contains(&b)) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00 // unique local
        || (segments[0] & 0xffc0) == 0xfe80 // link local
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || (segments[0] == 0x2001 && segments[1] == 0) // Teredo
        || segments[0] == 0x2002 // 6to4
        || ip.to_ipv4().is_some_and(|v4| !is_public_v4(v4)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};

    fn address() -> LightningAddress {
        LightningAddress::parse("Alice@Cash.App").unwrap()
    }

    fn metadata() -> String {
        r#"[["text/plain","Pay alice"],["text/identifier","alice@cash.app"]]"#.to_string()
    }

    fn pay_request_json(callback: &str, metadata: &str) -> String {
        serde_json::json!({
            "tag": "payRequest",
            "callback": callback,
            "minSendable": 1000,
            "maxSendable": 100_000_000,
            "metadata": metadata,
        })
        .to_string()
    }

    fn pay_request() -> PayRequest {
        PayRequest::from_response(
            address(),
            pay_request_json("https://cash.app/api/lnurl/pay/alice", &metadata()).as_bytes(),
        )
        .unwrap()
    }

    fn invoice(amount_msat: u64, description_hash: sha256::Hash, currency: Currency) -> String {
        let secp = Secp256k1::new();
        let node_key = SecretKey::from_slice(&[0x11; 32]).unwrap();
        InvoiceBuilder::new(currency)
            .description_hash(description_hash)
            .payment_hash(sha256::Hash::hash(&[0x22; 32]))
            .payment_secret(PaymentSecret([0x33; 32]))
            .amount_milli_satoshis(amount_msat)
            .current_timestamp()
            .min_final_cltv_expiry_delta(18)
            .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &node_key))
            .unwrap()
            .to_string()
    }

    #[test]
    fn addresses_are_normalised_and_validated() {
        let address = address();
        assert_eq!(address.to_string(), "alice@cash.app");
        assert_eq!(
            address.well_known_url(),
            "https://cash.app/.well-known/lnurlp/alice"
        );
        assert_eq!(
            LightningAddress::parse(" bob.smith_1@ln.example-wallet.com ")
                .unwrap()
                .to_string(),
            "bob.smith_1@ln.example-wallet.com"
        );
        for bad in [
            "alice",
            "@cash.app",
            "alice@",
            "al ice@cash.app",
            "alice@localhost",
            "alice@127.0.0.1",
            "alice@[::1]",
            "alice@wallet.onion",
            "alice@printer.local",
            "alice@-bad.com",
            "alice@cash.app:8443",
            "alice@cash.app/x",
        ] {
            assert!(
                matches!(
                    LightningAddress::parse(bad),
                    Err(LnurlError::InvalidAddress(_))
                ),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn only_public_addresses_are_fetched() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.5.5",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "192.0.0.8",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "2002:0a00:0001::1",
        ] {
            assert!(
                !is_public(private.parse().unwrap()),
                "{private} is not public"
            );
        }
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "104.16.0.1",
            "2606:4700::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(public.parse().unwrap()), "{public} is public");
        }
    }

    #[test]
    fn pay_request_requires_https_callback_and_matching_identifier() {
        let request = pay_request();
        assert_eq!(request.min_sendable_msat(), 1000);
        assert_eq!(request.max_sendable_msat(), 100_000_000);
        assert_eq!(
            request.metadata_hash(),
            sha256::Hash::hash(metadata().as_bytes()).to_byte_array()
        );
        assert_eq!(
            request.invoice_url(21_000).as_str(),
            "https://cash.app/api/lnurl/pay/alice?amount=21000"
        );
        assert_eq!(
            request.check_amount(999),
            Err(LnurlError::AmountOutOfRange {
                amount_msat: 999,
                min_msat: 1000,
                max_msat: 100_000_000
            })
        );

        let http = pay_request_json("http://cash.app/api/lnurl/pay/alice", &metadata());
        assert!(matches!(
            PayRequest::from_response(address(), http.as_bytes()),
            Err(LnurlError::Malformed(_))
        ));
        let internal = pay_request_json("https://10.0.0.5/pay", &metadata());
        assert!(matches!(
            PayRequest::from_response(address(), internal.as_bytes()),
            Err(LnurlError::Malformed(_))
        ));
        let someone_else = pay_request_json(
            "https://cash.app/api/lnurl/pay/alice",
            r#"[["text/plain","Pay mallory"],["text/identifier","mallory@cash.app"]]"#,
        );
        assert_eq!(
            PayRequest::from_response(address(), someone_else.as_bytes()),
            Err(LnurlError::IdentifierMismatch)
        );
        let no_identifier = pay_request_json(
            "https://cash.app/api/lnurl/pay/alice",
            r#"[["text/plain","Pay alice"]]"#,
        );
        assert_eq!(
            PayRequest::from_response(address(), no_identifier.as_bytes()),
            Err(LnurlError::IdentifierMismatch)
        );
        let error = br#"{"status":"ERROR","reason":"Error generating LUD06"}"#;
        assert_eq!(
            PayRequest::from_response(address(), error),
            Err(LnurlError::Provider("Error generating LUD06".into()))
        );
    }

    #[test]
    fn invoices_must_match_amount_metadata_and_network() {
        let request = pay_request();
        let hash = sha256::Hash::hash(metadata().as_bytes());
        let good = invoice(21_000, hash, Currency::Regtest);
        let parsed = request
            .verify_invoice(&good, 21_000, Network::Regtest)
            .unwrap();
        assert_eq!(parsed.amount_milli_satoshis(), Some(21_000));

        assert_eq!(
            request.verify_invoice(&good, 22_000, Network::Regtest),
            Err(LnurlError::InvoiceMismatch("wrong amount"))
        );
        assert_eq!(
            request.verify_invoice(&good, 21_000, Network::Bitcoin),
            Err(LnurlError::InvoiceMismatch("wrong network"))
        );
        let other_hash = sha256::Hash::hash(b"other metadata");
        assert_eq!(
            request.verify_invoice(
                &invoice(21_000, other_hash, Currency::Regtest),
                21_000,
                Network::Regtest
            ),
            Err(LnurlError::InvoiceMismatch(
                "description hash is not the metadata hash"
            ))
        );
        assert!(matches!(
            request.verify_invoice("lnbc1notaninvoice", 21_000, Network::Regtest),
            Err(LnurlError::Invoice(_))
        ));
    }
}
