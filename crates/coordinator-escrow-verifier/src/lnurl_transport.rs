//! Enclave-terminated HTTPS for optional automatic Lightning Address payouts.
//!
//! The relay supplies untrusted DNS hints and carries opaque TLS records. The
//! enclave owns the trust roots, verifies the original hostname, pins the public
//! IP before connecting, and parses the provider's response itself.

use anyhow::{anyhow, bail, ensure, Context, Result};
use bytes::Bytes;
use coordinator_escrow::lnurl_relay::{
    read_control, validate_addresses, validate_dns_host, write_control, RelayRequest, RelayResponse,
};
use http_body_util::{BodyExt, Empty};
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::TokioIo;
use keymeld_core::managed_socket::{SocketConnector, SocketStream};
use rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio_rustls::TlsConnector;
use tracing::instrument::WithSubscriber;
use url::Url;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_URL_BYTES: usize = 8192;
const MAX_INVOICE_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LnurlError {
    #[error("Invalid Lightning Address")]
    InvalidAddress,
    #[error("LNURL requires a public HTTPS URL without credentials or fragments")]
    UnsafeUrl,
    #[error("Invalid LNURL response: {0}")]
    InvalidResponse(#[source] serde_json::Error),
    #[error("LNURL provider refused the request: {0}")]
    Provider(String),
    #[error("LNURL endpoint does not offer payRequest")]
    WrongTag,
    #[error("LNURL provider returned an invalid amount range")]
    InvalidRange,
    #[error("Payout is outside provider range {min_msat}–{max_msat} millisatoshis")]
    AmountOutOfRange { min_msat: u64, max_msat: u64 },
    #[error("LNURL provider requires unsupported payer data")]
    RequiredPayerData,
    #[error("Invalid LNURL metadata")]
    InvalidMetadata,
}

/// Validate and canonicalize an address before accepting automatic authorization.
pub fn validate_address(value: &str) -> Result<String> {
    let value = value.trim();
    ensure!(value.len() <= 320, LnurlError::InvalidAddress);
    let (user, domain) = value.split_once('@').ok_or(LnurlError::InvalidAddress)?;
    ensure!(
        !user.is_empty()
            && user != "."
            && user != ".."
            && user.len() <= 64
            && user
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_.+".contains(&c)),
        LnurlError::InvalidAddress
    );
    validate_dns_host(domain).map_err(|_| LnurlError::InvalidAddress)?;
    let address = format!("{user}@{}", domain.to_ascii_lowercase());
    validate_url(&discovery_url(&address)?)?;
    Ok(address)
}

fn discovery_url(address: &str) -> Result<Url> {
    let (user, domain) = address.split_once('@').ok_or(LnurlError::InvalidAddress)?;
    Url::parse(&format!("https://{domain}/.well-known/lnurlp/{user}"))
        .map_err(|_| LnurlError::InvalidAddress.into())
}

fn validate_url(url: &Url) -> Result<()> {
    ensure!(
        url.as_str().len() <= MAX_URL_BYTES
            && url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.port() != Some(0),
        LnurlError::UnsafeUrl
    );
    if let Some(ip) = literal_ip(url) {
        validate_addresses(
            &[SocketAddr::new(ip, url.port_or_known_default().unwrap())],
            url.port_or_known_default().unwrap(),
        )?;
    } else {
        validate_dns_host(url.host_str().unwrap())?;
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

#[derive(Clone)]
pub struct LnurlPayClient {
    relay: SocketConnector,
    tls: Arc<ClientConfig>,
}

impl LnurlPayClient {
    pub fn new(relay: SocketConnector) -> Self {
        // Compiled into the measured enclave. No environment, gateway command,
        // operating-system certificate directory or proxy can add a trust root.
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self {
            relay,
            tls: Arc::new(tls_config(roots)),
        }
    }

    /// Returns a provider-authenticated invoice, never release authorization.
    /// The caller must verify signature, network, exact amount and expiry with
    /// the payout verifier and persist its commitment before returning it to pay.
    pub async fn request_invoice(&self, address: &str, amount_msat: u64) -> Result<String> {
        ensure!(amount_msat > 0, "LNURL payout amount must be positive");
        let address = validate_address(address)?;
        let params: PayRequest = parse_response(self.get_json(discovery_url(&address)?).await?)?;
        let callback = params.callback_url(amount_msat)?;
        let response: InvoiceResponse = parse_response(self.get_json(callback).await?)?;
        ensure!(
            !response.pr.is_empty() && response.pr.len() <= MAX_INVOICE_BYTES,
            "Invalid LNURL invoice size"
        );
        Ok(response.pr)
    }

    /// The metadata commitment an invoice from `address` must carry, for `amount_msat`.
    ///
    /// Only the provider's discovery step runs: no invoice is requested. A caller supplies the
    /// invoice instead, which is what a refund needs, because its payment hash must be known
    /// before the transaction paying it exists. See `payout::validate_address_invoice`.
    pub async fn invoice_binding(&self, address: &str, amount_msat: u64) -> Result<[u8; 32]> {
        ensure!(amount_msat > 0, "LNURL payout amount must be positive");
        let address = validate_address(address)?;
        let params: PayRequest = parse_response(self.get_json(discovery_url(&address)?).await?)?;
        // Rejects a wrong tag, an unpayable amount and required payer data, as paying would.
        params.callback_url(amount_msat)?;
        Ok(keymeld_core::escrow::sha256(params.metadata.as_bytes()))
    }

    async fn get_json(&self, url: Url) -> Result<Value> {
        validate_url(&url)?;
        // Includes relay DNS, connect, TLS, HTTP headers and complete response.
        request_with_timeout(REQUEST_TIMEOUT, self.get_json_inner(url)).await
    }

    async fn get_json_inner(&self, url: Url) -> Result<Value> {
        let host = url.host_str().ok_or(LnurlError::UnsafeUrl)?;
        let port = url.port_or_known_default().ok_or(LnurlError::UnsafeUrl)?;
        let addresses = if let Some(ip) = literal_ip(&url) {
            vec![SocketAddr::new(ip, port)]
        } else {
            let mut socket = self.relay.connect(CONNECT_TIMEOUT).await?;
            write_control(
                &mut socket,
                &RelayRequest::Resolve {
                    host: host.into(),
                    port,
                },
            )
            .await?;
            match read_control(&mut socket).await? {
                RelayResponse::Addresses(addresses) => addresses,
                _ => bail!("LNURL relay could not resolve destination"),
            }
        };
        validate_addresses(&addresses, port)?;
        // Every candidate is already checked. No hostname reaches Connect and
        // no implicit DNS resolver or proxy is used by the TLS/HTTP stack.
        let mut connected = None;
        for address in addresses {
            if let Ok(socket) = self.connect_pinned(address).await {
                connected = Some(socket);
                break;
            }
        }
        let socket = connected.ok_or_else(|| anyhow!("LNURL relay connection failed"))?;
        let hostname = host.trim_start_matches('[').trim_end_matches(']');
        let server_name =
            ServerName::try_from(hostname.to_owned()).map_err(|_| LnurlError::UnsafeUrl)?;
        let tls = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TlsConnector::from(self.tls.clone()).connect(server_name, socket),
        )
        .await
        .map_err(|_| anyhow!("LNURL TLS handshake timed out"))?
        .map_err(|_| anyhow!("LNURL TLS authentication failed"))?;
        fetch_json(TokioIo::new(tls), &url).await
    }

    async fn connect_pinned(&self, address: SocketAddr) -> Result<SocketStream> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            let mut socket = self.relay.connect(CONNECT_TIMEOUT).await?;
            write_control(&mut socket, &RelayRequest::Connect { address }).await?;
            ensure!(
                matches!(read_control(&mut socket).await?, RelayResponse::Connected),
                "LNURL relay rejected connection"
            );
            Ok(socket)
        })
        .await
        .map_err(|_| anyhow!("LNURL relay connection timed out"))?
    }
}

async fn request_with_timeout<T>(
    duration: Duration,
    request: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(duration, request)
        .await
        .map_err(|_| anyhow!("LNURL request timed out"))?
}

fn tls_config(roots: RootCertStore) -> ClientConfig {
    let mut config = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("supported TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

// Aborting the HTTP driver on timeout prevents detached socket tasks surviving
// their request. Dropping the response and sender alone is not our lifetime bound.
struct ConnectionTask(tokio::task::JoinHandle<()>);
impl Drop for ConnectionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn fetch_json<S>(socket: S, url: &Url) -> Result<Value>
where
    S: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_buf_size(32 * 1024)
        .max_headers(64)
        .handshake(socket)
        .await
        .context("LNURL HTTP handshake failed")?;
    // Tokio tasks do not inherit the confidential command's tracing dispatcher.
    // Keep HTTP headers, callback tokens and provider response diagnostics off
    // the host-visible enclave console even when dependency tracing is enabled.
    let _connection = ConnectionTask(tokio::spawn(
        async move {
            let _ = connection.await;
        }
        .with_subscriber(tracing::subscriber::NoSubscriber::default()),
    ));
    let target = &url[url::Position::BeforePath..url::Position::AfterQuery];
    let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
    let request = Request::get(target)
        .header("Host", authority)
        .header("Accept", "application/json")
        .header("Accept-Encoding", "identity")
        .header("Connection", "close")
        .body(Empty::<Bytes>::new())?;
    let response = sender
        .send_request(request)
        .await
        .context("LNURL HTTP request failed")?;
    read_json(response).await
}

async fn read_json(response: Response<Incoming>) -> Result<Value> {
    ensure!(
        !response.status().is_redirection(),
        "LNURL redirects are not supported"
    );
    if let Some(length) = response.headers().get(hyper::header::CONTENT_LENGTH) {
        let length: usize = length.to_str()?.parse()?;
        ensure!(length <= MAX_RESPONSE_BYTES, "LNURL response is too large");
    }
    let mut response = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = response.frame().await {
        let frame = frame.context("LNURL response body failed")?;
        if let Ok(data) = frame.into_data() {
            ensure!(
                data.len() <= MAX_RESPONSE_BYTES - bytes.len(),
                "LNURL response is too large"
            );
            bytes.extend_from_slice(&data);
        }
    }
    // LUD-01 permits JSON errors with a non-success HTTP status or MIME type.
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PayRequest {
    callback: String,
    min_sendable: u64,
    max_sendable: u64,
    metadata: String,
    tag: String,
    #[serde(default)]
    payer_data: Option<Value>,
}

impl PayRequest {
    fn callback_url(&self, amount_msat: u64) -> Result<Url, LnurlError> {
        if self.tag != "payRequest" {
            return Err(LnurlError::WrongTag);
        }
        if self.min_sendable == 0 || self.max_sendable < self.min_sendable {
            return Err(LnurlError::InvalidRange);
        }
        if !(self.min_sendable..=self.max_sendable).contains(&amount_msat) {
            return Err(LnurlError::AmountOutOfRange {
                min_msat: self.min_sendable,
                max_msat: self.max_sendable,
            });
        }
        if let Some(data) = &self.payer_data {
            let fields = data.as_object().ok_or(LnurlError::RequiredPayerData)?;
            for field in fields.values() {
                let field = field.as_object().ok_or(LnurlError::RequiredPayerData)?;
                if field
                    .get("mandatory")
                    .is_some_and(|value| value.as_bool() != Some(false))
                {
                    return Err(LnurlError::RequiredPayerData);
                }
            }
        }
        validate_metadata(&self.metadata)?;
        let mut callback = Url::parse(&self.callback).map_err(|_| LnurlError::UnsafeUrl)?;
        validate_url(&callback).map_err(|_| LnurlError::UnsafeUrl)?;
        // Keep provider query parameters, replacing any existing amount rather
        // than sending ambiguous duplicate values. Cross-host callbacks work.
        // Match decoded keys but retain the original bytes of opaque provider
        // parameters. Reencoding spaces or percent escapes can break signed URLs.
        let query = {
            let mut decoded = callback.query_pairs();
            callback
                .query()
                .unwrap_or_default()
                .split('&')
                .filter(|raw| {
                    raw.is_empty() || decoded.next().is_some_and(|(key, _)| key != "amount")
                })
                .collect::<Vec<_>>()
                .join("&")
        };
        callback.set_query(if query.is_empty() { None } else { Some(&query) });
        callback
            .query_pairs_mut()
            .append_pair("amount", &amount_msat.to_string());
        Ok(callback)
    }
}

#[derive(Debug, Deserialize)]
struct InvoiceResponse {
    pr: String,
}

fn parse_response<T: DeserializeOwned>(value: Value) -> Result<T, LnurlError> {
    if value.get("status").and_then(Value::as_str) == Some("ERROR") {
        // Reasons are untrusted text. Never render them as HTML.
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("unspecified error");
        return Err(LnurlError::Provider(
            reason
                .chars()
                .filter(|c| !c.is_control())
                .take(256)
                .collect(),
        ));
    }
    serde_json::from_value(value).map_err(LnurlError::InvalidResponse)
}

fn validate_metadata(metadata: &str) -> Result<(), LnurlError> {
    let rows: Vec<Vec<Value>> =
        serde_json::from_str(metadata).map_err(|_| LnurlError::InvalidMetadata)?;
    let mut descriptions = 0;
    for row in rows {
        let kind = row
            .first()
            .and_then(Value::as_str)
            .ok_or(LnurlError::InvalidMetadata)?;
        if row.len() < 2 {
            return Err(LnurlError::InvalidMetadata);
        }
        if kind == "text/plain" {
            if !row[1].is_string() {
                return Err(LnurlError::InvalidMetadata);
            }
            descriptions += 1;
        }
        // Future metadata types may contain non-string JSON values.
    }
    if descriptions != 1 {
        return Err(LnurlError::InvalidMetadata);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn request() -> Value {
        json!({"tag":"payRequest", "callback":"https://invoices.provider.example/pay?token=secret&amount=1",
            "minSendable":1000, "maxSendable":1_000_000, "metadata":"[[\"text/plain\",\"payout\"]]"})
    }

    #[test]
    fn validates_discovery_and_callback_urls_with_the_same_public_https_policy() {
        for input in [
            "http://wallet.example/lnurl",
            "https://user@wallet.example/lnurl",
            "https://:secret@wallet.example/lnurl",
            "https://wallet.example/#fragment",
            "https://wallet.example:0/",
            "https://127.0.0.1/",
            "https://2130706433/",
            "https://0x7f000001/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://provider.onion/",
            "https://localhost/",
        ] {
            assert!(
                validate_url(&Url::parse(input).unwrap()).is_err(),
                "{input}"
            );
        }
        for address in [
            "alice@127.0.0.1",
            "alice@0x7f000001",
            "alice@127.1",
            "alice@wallet.onion",
            "alice@wallet.example/path",
            "alice@wallet.example:443",
            "alice/../bob@wallet.example",
            "alice%2fbob@wallet.example",
            ".@wallet.example",
            "..@wallet.example",
            "alice@localhost",
        ] {
            assert!(validate_address(address).is_err(), "{address}");
        }
        assert_eq!(
            validate_address(" alice+prize@WALLET.Example ").unwrap(),
            "alice+prize@wallet.example"
        );
    }

    #[test]
    fn callback_supports_other_hosts_and_preserves_provider_parameters() {
        let params: PayRequest = parse_response(request()).unwrap();
        let callback = params.callback_url(42_000).unwrap();
        assert_eq!(callback.host_str(), Some("invoices.provider.example"));
        assert_eq!(callback.query(), Some("token=secret&amount=42000"));
        assert_eq!(
            callback
                .query_pairs()
                .filter(|(key, _)| key == "amount")
                .count(),
            1
        );
    }

    #[test]
    fn callback_preserves_opaque_query_bytes_and_replaces_encoded_amounts() {
        let mut value = request();
        value["callback"] =
            json!("https://provider.example/pay?token=a%2fb%20c&&am%6funt=1&bare&amount=2");
        let params: PayRequest = parse_response(value).unwrap();
        let callback = params.callback_url(42_000).unwrap();
        assert_eq!(callback.query(), Some("token=a%2fb%20c&&bare&amount=42000"));
    }

    #[test]
    fn optional_extensions_are_not_required_or_executed() {
        let mut value = request();
        value["allowsNostr"] = json!(true);
        value["commentAllowed"] = json!(100);
        value["payerData"] = json!({"email": {"mandatory": false}});
        let params: PayRequest = parse_response(value).unwrap();
        assert!(!params
            .callback_url(42_000)
            .unwrap()
            .as_str()
            .contains("payerdata"));

        let mut value = request();
        value["payerData"] = json!({"email": {}});
        let params: PayRequest = parse_response(value).unwrap();
        params.callback_url(42_000).unwrap();

        // A successAction or verify URL is not a payment proof or a URL to fetch.
        let response: InvoiceResponse = parse_response(json!({
            "pr": "invoice", "routes": [],
            "successAction": {"tag": "url", "url": "http://127.0.0.1/"},
            "verify": "http://169.254.169.254/"
        }))
        .unwrap();
        assert_eq!(response.pr, "invoice");
    }

    #[test]
    fn required_payer_data_stops_before_payment() {
        for payer_data in [
            json!({"email": {"mandatory": true}}),
            json!({"unknownFutureField": {"mandatory": true}}),
            json!({"email": {"mandatory": "false"}}),
            json!({"email": {"mandatory": null}}),
            json!({"email": false}),
            json!([]),
        ] {
            let mut value = request();
            value["payerData"] = payer_data;
            let params: PayRequest = parse_response(value).unwrap();
            assert!(matches!(
                params.callback_url(42_000),
                Err(LnurlError::RequiredPayerData)
            ));
        }
    }

    #[test]
    fn refuses_invalid_tag_ranges_and_unpayable_amounts() {
        let mut params: PayRequest = parse_response(request()).unwrap();
        for amount in [999, 1_000_001] {
            assert!(matches!(
                params.callback_url(amount),
                Err(LnurlError::AmountOutOfRange { .. })
            ));
        }
        params.min_sendable = 0;
        assert!(matches!(
            params.callback_url(42_000),
            Err(LnurlError::InvalidRange)
        ));
        params.min_sendable = params.max_sendable + 1;
        assert!(matches!(
            params.callback_url(42_000),
            Err(LnurlError::InvalidRange)
        ));
        params.tag = "withdrawRequest".into();
        assert!(matches!(
            params.callback_url(42_000),
            Err(LnurlError::WrongTag)
        ));
    }

    #[test]
    fn rejects_unsafe_callbacks_before_fetching_them() {
        for callback in [
            "http://provider.example/pay",
            "https://127.0.0.1/pay",
            "https://user:pass@provider.example/pay",
        ] {
            let mut value = request();
            value["callback"] = json!(callback);
            let params: PayRequest = parse_response(value).unwrap();
            assert!(params.callback_url(42_000).is_err());
        }
    }

    #[test]
    fn handles_provider_errors_at_both_protocol_steps() {
        let value = json!({"status": "ERROR", "reason": "recipient unavailable"});
        assert!(matches!(
            parse_response::<PayRequest>(value.clone()),
            Err(LnurlError::Provider(_))
        ));
        assert!(matches!(
            parse_response::<InvoiceResponse>(value),
            Err(LnurlError::Provider(_))
        ));
        assert!(matches!(
            parse_response::<InvoiceResponse>(json!({"pr": 42})),
            Err(LnurlError::InvalidResponse(_))
        ));
    }

    #[test]
    fn metadata_allows_thumbnails_and_unknown_non_string_values() {
        let metadata = json!([
            ["text/plain", "payout"],
            ["image/png;base64", "a".repeat(136_536)],
            ["future/object", {"version": 1}],
            ["future/number", 7, true],
            // Providers may strip a LUD-16 tag in their identifier.
            ["text/identifier", "alice@provider.example"]
        ])
        .to_string();
        validate_metadata(&metadata).unwrap();
        for invalid in [
            "{}",
            "[]",
            "[1]",
            "[[\"text/plain\",7]]",
            "[[\"text/plain\",\"a\"],[\"text/plain\",\"b\"]]",
        ] {
            assert!(
                matches!(validate_metadata(invalid), Err(LnurlError::InvalidMetadata)),
                "{invalid}"
            );
        }
    }

    async fn http_fixture(response: Vec<u8>) -> Result<Value> {
        let (client, mut server) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let mut request = [0; 4096];
            let _ = server.read(&mut request).await;
            let _ = server.write_all(&response).await;
        });
        let result = fetch_json(
            TokioIo::new(client),
            &Url::parse("https://fixture.invalid/?token=secret").unwrap(),
        )
        .await;
        task.abort();
        result
    }

    #[tokio::test]
    async fn rejects_redirects_and_declared_or_streamed_oversized_responses() {
        let result = http_fixture(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/\r\nContent-Length: 2\r\n\r\n{}"
                .to_vec(),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("redirect"));
        let result = http_fixture(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_RESPONSE_BYTES + 1
            )
            .into_bytes(),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("too large"));
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..=MAX_RESPONSE_BYTES / 1024 {
            response.extend_from_slice(b"400\r\n");
            response.extend_from_slice(&[b' '; 1024]);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        assert!(http_fixture(response)
            .await
            .unwrap_err()
            .to_string()
            .contains("too large"));
    }

    #[tokio::test]
    async fn accepts_exact_response_limit_and_lnurl_http_error_json() {
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {MAX_RESPONSE_BYTES}\r\n\r\n{{}}")
                .into_bytes();
        response.extend(std::iter::repeat_n(b' ', MAX_RESPONSE_BYTES - 2));
        assert_eq!(http_fixture(response).await.unwrap(), json!({}));
        let body = br#"{"status":"ERROR","reason":"offline"}"#;
        let mut response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        assert_eq!(http_fixture(response).await.unwrap()["status"], "ERROR");
    }

    /// The fake relay claims a public pin, then routes ciphertext to the local
    /// TLS fixture. Production relay never bypasses its public-IP checks.
    async fn tls_fixture(
        trust_fixture: bool,
        certificate_host: &str,
    ) -> (LnurlPayClient, tokio::task::JoinHandle<()>) {
        let cert = rcgen::generate_simple_self_signed(vec![certificate_host.into()]).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut dns, _) = listener.accept().await.unwrap();
            match read_control::<_, RelayRequest>(&mut dns).await.unwrap() {
                RelayRequest::Resolve { host, port } => {
                    assert_eq!(host, "fixture.invalid");
                    assert_eq!(port, 443);
                }
                _ => panic!("Expected DNS request"),
            }
            write_control(
                &mut dns,
                &RelayResponse::Addresses(vec!["8.8.8.8:443".parse().unwrap()]),
            )
            .await
            .unwrap();
            drop(dns);
            let (mut stream, _) = listener.accept().await.unwrap();
            match read_control::<_, RelayRequest>(&mut stream).await.unwrap() {
                RelayRequest::Connect { address } => {
                    assert_eq!(address, "8.8.8.8:443".parse().unwrap())
                }
                _ => panic!("Second DNS lookup instead of pinned connection"),
            }
            write_control(&mut stream, &RelayResponse::Connected)
                .await
                .unwrap();
            if let Ok(mut stream) = acceptor.accept(stream).await {
                let mut request = [0; 4096];
                let n = stream.read(&mut request).await.unwrap();
                let request = std::str::from_utf8(&request[..n])
                    .unwrap()
                    .to_ascii_lowercase();
                assert!(request.contains("host: fixture.invalid"));
                assert!(request.starts_with("get /?token=secret http/1.1"));
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .await
                    .unwrap();
            }
        });
        let mut client = LnurlPayClient::new(SocketConnector::tcp("127.0.0.1", addr.port()));
        if trust_fixture {
            client.tls = Arc::new(tls_config(roots));
        }
        (client, server)
    }

    /// Connected payout-operation fixture. Only tests may trust this generated
    /// CA or route the asserted public pins to this local TLS origin.
    pub(crate) async fn automatic_payout_tls_fixture(
        invoice: String,
    ) -> (LnurlPayClient, tokio::task::JoinHandle<()>) {
        payout_fixture(Some(invoice)).await
    }

    /// The metadata this fixture's provider serves, which its invoices commit to.
    pub(crate) const FIXTURE_METADATA: &str = "[[\"text/plain\",\"Provider display metadata\"]]";

    /// The same provider, serving discovery alone: what verifying a supplied invoice needs.
    pub(crate) async fn discovery_tls_fixture() -> (LnurlPayClient, tokio::task::JoinHandle<()>) {
        payout_fixture(None).await
    }

    /// Serves LNURL discovery, and the callback too when an `invoice` is given.
    async fn payout_fixture(invoice: Option<String>) -> (LnurlPayClient, tokio::task::JoinHandle<()>) {
        let certificate = rcgen::generate_simple_self_signed(vec![
            "wallet.example".into(),
            "invoices.example".into(),
        ])
        .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(certificate.cert.der().clone()).unwrap();
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.key_pair.serialize_der())
                .into(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let discovery = json!({
                "tag": "payRequest",
                "callback": "https://invoices.example/pay?token=opaque%2Btoken&amount=1",
                "minSendable": 1000, "maxSendable": 100_000_000,
                "metadata": FIXTURE_METADATA,
            });
            let mut exchanges = vec![(
                "wallet.example",
                "8.8.8.8:443",
                "/.well-known/lnurlp/alice+prize".to_string(),
                discovery,
            )];
            if let Some(invoice) = invoice {
                exchanges.push((
                    "invoices.example",
                    "9.9.9.9:443",
                    "/pay?token=opaque%2Btoken&amount=100000000".to_string(),
                    json!({"pr": invoice}),
                ));
            }
            for (host, pin, path, body) in exchanges {
                let (mut dns, _) = listener.accept().await.unwrap();
                match read_control::<_, RelayRequest>(&mut dns).await.unwrap() {
                    RelayRequest::Resolve {
                        host: requested,
                        port,
                    } => {
                        assert_eq!(requested, host);
                        assert_eq!(port, 443);
                    }
                    _ => panic!("Expected hostname resolution"),
                }
                let pin: SocketAddr = pin.parse().unwrap();
                write_control(&mut dns, &RelayResponse::Addresses(vec![pin]))
                    .await
                    .unwrap();
                drop(dns);
                let (mut socket, _) = listener.accept().await.unwrap();
                match read_control::<_, RelayRequest>(&mut socket).await.unwrap() {
                    RelayRequest::Connect { address } => assert_eq!(address, pin),
                    _ => panic!("Expected exact public DNS pin"),
                }
                write_control(&mut socket, &RelayResponse::Connected)
                    .await
                    .unwrap();
                let mut tls = acceptor.accept(socket).await.unwrap();
                assert_eq!(tls.get_ref().1.server_name(), Some(host));
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let read = tls.read(&mut buffer).await.unwrap();
                    assert!(read > 0 && request.len() + read <= 8192);
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = std::str::from_utf8(&request).unwrap();
                assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
                assert!(request
                    .to_ascii_lowercase()
                    .contains(&format!("host: {host}\r\n")));
                let body = body.to_string();
                tls.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            }
        });
        let mut client = LnurlPayClient::new(SocketConnector::tcp("127.0.0.1", address.port()));
        client.tls = Arc::new(tls_config(roots));
        (client, task)
    }

    #[tokio::test]
    async fn tls_terminates_inside_enclave_with_original_hostname_and_dns_pin() {
        let (client, server) = tls_fixture(true, "fixture.invalid").await;
        assert_eq!(
            client
                .get_json(Url::parse("https://fixture.invalid/?token=secret").unwrap())
                .await
                .unwrap(),
            json!({})
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn relay_cannot_substitute_certificate_or_hostname() {
        for (trust_fixture, name) in [(false, "fixture.invalid"), (true, "attacker.invalid")] {
            let (client, server) = tls_fixture(trust_fixture, name).await;
            let error = client
                .get_json(Url::parse("https://fixture.invalid/?token=secret").unwrap())
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "LNURL TLS authentication failed");
            assert!(!error.to_string().contains("secret"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn private_dns_answers_are_rejected_before_connect_command() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _: RelayRequest = read_control(&mut stream).await.unwrap();
            write_control(
                &mut stream,
                &RelayResponse::Addresses(vec![
                    "8.8.8.8:443".parse().unwrap(),
                    "169.254.169.254:443".parse().unwrap(),
                ]),
            )
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let client = LnurlPayClient::new(SocketConnector::tcp("127.0.0.1", addr.port()));
        assert!(client
            .get_json(Url::parse("https://fixture.invalid/").unwrap())
            .await
            .unwrap_err()
            .to_string()
            .contains("not a public address"));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn deadline_covers_stalled_response_body_and_drops_connection_driver() {
        let (client, mut server) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let mut request = [0; 4096];
            let _ = server.read(&mut request).await;
            server
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{")
                .await
                .unwrap();
            // Cancellation must close the HTTP driver and the underlying tunnel.
            assert_eq!(server.read(&mut request).await.unwrap(), 0);
        });
        let result = request_with_timeout(
            Duration::from_millis(100),
            fetch_json(
                TokioIo::new(client),
                &Url::parse("https://fixture.invalid/").unwrap(),
            ),
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), "LNURL request timed out");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn callback_dns_is_validated_again_after_authenticated_discovery() {
        let cert = rcgen::generate_simple_self_signed(vec!["fixture.invalid".into()]).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut dns, _) = listener.accept().await.unwrap();
            match read_control::<_, RelayRequest>(&mut dns).await.unwrap() {
                RelayRequest::Resolve { host, .. } => assert_eq!(host, "fixture.invalid"),
                _ => panic!("Expected discovery DNS"),
            }
            write_control(
                &mut dns,
                &RelayResponse::Addresses(vec!["8.8.8.8:443".parse().unwrap()]),
            )
            .await
            .unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(matches!(
                read_control::<_, RelayRequest>(&mut stream).await.unwrap(),
                RelayRequest::Connect { .. }
            ));
            write_control(&mut stream, &RelayResponse::Connected)
                .await
                .unwrap();
            let mut tls = acceptor.accept(stream).await.unwrap();
            let mut request_bytes = [0; 4096];
            let _ = tls.read(&mut request_bytes).await.unwrap();
            let mut discovery = request();
            discovery["callback"] = json!("https://callback.other.invalid/pay");
            let body = discovery.to_string();
            tls.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            let (mut dns, _) = listener.accept().await.unwrap();
            match read_control::<_, RelayRequest>(&mut dns).await.unwrap() {
                RelayRequest::Resolve { host, .. } => assert_eq!(host, "callback.other.invalid"),
                _ => panic!("Expected callback DNS"),
            }
            write_control(
                &mut dns,
                &RelayResponse::Addresses(vec!["127.0.0.1:443".parse().unwrap()]),
            )
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let mut client = LnurlPayClient::new(SocketConnector::tcp("127.0.0.1", addr.port()));
        client.tls = Arc::new(tls_config(roots));
        let error = client
            .request_invoice("alice@fixture.invalid", 42_000)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not a public address"));
        server.await.unwrap();
    }
}
