//! The test network's Lightning nodes: paying entry invoices for scenarios that need real
//! payments, and moving the balance back when they drain it.
//!
//! An Arkade entry is funded by `ark-swapd` swapping the player's Lightning payment into their
//! escrow VTXO, so a scenario that exercises escrows has to pay for real: the coordinator's test
//! settle endpoint holds no invoice of ark-swapd's to settle. Scenarios that do not need an
//! escrow keep using that endpoint, and leave this unconfigured.

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE;
use base64::Engine;
use serde::{Deserialize, Serialize};
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
    pub async fn pay(&self, invoice: &str) -> Result<Paid> {
        self.send(json!({
            "payment_request": invoice,
            "timeout_seconds": self.payment_timeout_secs,
            "fee_limit_sat": self.fee_limit_sats.to_string(),
        }))
        .await
    }

    /// Pay `invoice` over `channel` only, so the payment moves that channel's balance and no
    /// other's.
    pub async fn pay_through(&self, invoice: &str, channel: &str) -> Result<Paid> {
        self.send(json!({
            "payment_request": invoice,
            "timeout_seconds": self.payment_timeout_secs,
            "fee_limit_sat": self.fee_limit_sats.to_string(),
            "outgoing_chan_ids": [channel],
        }))
        .await
    }

    /// The router streams a payment's progress, so this reads until the last status it reports.
    async fn send(&self, request: serde_json::Value) -> Result<Paid> {
        let response = self
            .client
            .post(format!("{}v2/router/send", self.base_url))
            .header(MACAROON_HEADER, &self.macaroon)
            .json(&request)
            .send()
            .await
            .context("send the payment")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("LND refused the payment ({status}): {body}");
        }
        let body = response.text().await.context("read the payment stream")?;
        let payment = last_payment(&body)?;
        match payment.status {
            PaymentStatus::Succeeded => Ok(payment.paid()),
            other => Err(anyhow!(
                "the payment ended {other:?}: {}",
                payment.failure_reason.unwrap_or_default()
            )),
        }
    }

    /// A payment this node made, by its hash: what it paid, in fees, and the preimage it got.
    /// None if the node never made it, or has not finished it. Needs a macaroon that may read
    /// payments.
    pub async fn payment(&self, payment_hash: &str) -> Result<Option<Paid>> {
        Ok(match self.track(payment_hash).await? {
            Tracked::Succeeded(paid) => Some(paid),
            Tracked::Failed | Tracked::InFlight | Tracked::NeverMade => None,
        })
    }

    /// Where a payment this node may have made stands, by its hash.
    pub async fn track(&self, payment_hash: &str) -> Result<Tracked> {
        let hash = hex::decode(payment_hash).context("the payment hash is not hex")?;
        let response = self
            .client
            .get(format!(
                "{}v2/router/track/{}",
                self.base_url,
                URL_SAFE.encode(hash)
            ))
            .query(&[("no_inflight_updates", "true")])
            .header(MACAROON_HEADER, &self.macaroon)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .context("look the payment up")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Tracked::NeverMade);
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("LND refused ({status}): {body}");
        }
        // A payment still in flight keeps the stream open; the timeout ends it.
        let body = match response.text().await {
            Ok(body) => body,
            Err(e) if e.is_timeout() => return Ok(Tracked::InFlight),
            Err(e) => return Err(e).context("read the payment"),
        };
        match last_payment(&body) {
            Ok(payment) => Ok(match payment.status {
                PaymentStatus::Succeeded => Tracked::Succeeded(payment.paid()),
                PaymentStatus::Failed => Tracked::Failed,
                PaymentStatus::InFlight | PaymentStatus::Initiated | PaymentStatus::Unknown => {
                    Tracked::InFlight
                }
            }),
            // LND reports a payment it never made as an error in the stream.
            Err(e) if never_made(&format!("{e:#}")) => Ok(Tracked::NeverMade),
            Err(e) => Err(e),
        }
    }

    /// An invoice this node issued, by its hash: whether it settled, and for how much.
    pub async fn lookup_invoice(&self, payment_hash: &str) -> Result<Option<Invoice>> {
        let response = self
            .client
            .get(format!("{}v1/invoice/{payment_hash}", self.base_url))
            .header(MACAROON_HEADER, &self.macaroon)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .context("look the invoice up")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::read(response).await?))
    }

    /// This node's public key and alias.
    pub async fn identity(&self) -> Result<NodeIdentity> {
        #[derive(Deserialize)]
        struct Info {
            identity_pubkey: String,
            #[serde(default)]
            alias: String,
        }
        let info: Info = self.get("v1/getinfo").await?;
        Ok(NodeIdentity {
            pubkey: info.identity_pubkey,
            alias: info.alias,
        })
    }

    /// What another node calls itself, from this node's view of the network graph. None for a
    /// node the graph does not know, such as a private one.
    pub async fn alias_of(&self, pubkey: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Info {
            node: Option<Node>,
        }
        #[derive(Deserialize)]
        struct Node {
            #[serde(default)]
            alias: String,
        }
        let response = self
            .client
            .get(format!("{}v1/graph/node/{pubkey}", self.base_url))
            .query(&[("include_channels", "false")])
            .header(MACAROON_HEADER, &self.macaroon)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .context("look the node up")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let info: Info = Self::read(response).await?;
        Ok(info
            .node
            .map(|node| node.alias)
            .filter(|alias| !alias.is_empty()))
    }

    /// The largest active channel this node has with `peer`, if any.
    pub async fn channel_with(&self, peer: &str) -> Result<Option<Channel>> {
        #[derive(Deserialize)]
        struct Channels {
            #[serde(default)]
            channels: Vec<ListedChannel>,
        }
        #[derive(Deserialize)]
        struct ListedChannel {
            remote_pubkey: String,
            chan_id: String,
            #[serde(default)]
            active: bool,
            #[serde(default, with = "sats")]
            local_balance: u64,
            #[serde(default, with = "sats")]
            remote_balance: u64,
        }
        let listed: Channels = self.get("v1/channels").await?;
        Ok(listed
            .channels
            .into_iter()
            .filter(|channel| channel.active && channel.remote_pubkey == peer)
            .map(|channel| Channel {
                id: channel.chan_id,
                local_sats: channel.local_balance,
                remote_sats: channel.remote_balance,
            })
            .max_by_key(|channel| channel.local_sats + channel.remote_sats))
    }

    /// An invoice for `sats`, which this node is paid.
    pub async fn invoice(&self, sats: u64, memo: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Added {
            payment_request: String,
        }
        let response = self
            .client
            .post(format!("{}v1/invoices", self.base_url))
            .header(MACAROON_HEADER, &self.macaroon)
            .json(&json!({ "value": sats.to_string(), "memo": memo, "expiry": "600" }))
            .send()
            .await
            .context("create an invoice")?;
        let added: Added = Self::read(response).await?;
        Ok(added.payment_request)
    }

    /// Send `sats` on-chain to `address`, returning the transaction's id. Needs a macaroon that
    /// may send on-chain.
    pub async fn send_on_chain(&self, address: &str, sats: u64, label: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Sent {
            txid: String,
        }
        let response = self
            .client
            .post(format!("{}v1/transactions", self.base_url))
            .header(MACAROON_HEADER, &self.macaroon)
            .json(&json!({
                "addr": address,
                "amount": sats.to_string(),
                "target_conf": 3,
                "label": label,
            }))
            .send()
            .await
            .context("send coins on-chain")?;
        let sent: Sent = Self::read(response).await?;
        Ok(sent.txid)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(format!("{}{path}", self.base_url))
            .header(MACAROON_HEADER, &self.macaroon)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        Self::read(response).await
    }

    async fn read<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("LND refused ({status}): {body}");
        }
        Ok(response.json().await?)
    }
}

/// Where a payment a node may have made stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    Succeeded(Paid),
    /// It failed: nothing was paid.
    Failed,
    /// It may still settle.
    InFlight,
    /// The node never made it.
    NeverMade,
}

/// A node, as it names itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub pubkey: String,
    pub alias: String,
}

/// A payment that went through, as the paying node saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paid {
    pub payment_hash: String,
    /// What the payee revealed, proving it was paid.
    pub preimage: String,
    /// What the payee got.
    pub value_sat: u64,
    /// What the routing nodes took on top.
    pub fee_msat: u64,
    /// The channels it went through, the payer's first, and the node each led to.
    #[serde(default)]
    pub route: Vec<Hop>,
}

impl Paid {
    /// The fee in whole sats, rounded up: what the payer's balance lost beyond the amount.
    pub fn fee_sat(&self) -> u64 {
        self.fee_msat.div_ceil(1000)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hop {
    pub chan_id: String,
    pub pub_key: String,
}

/// An invoice a node issued, as LND's REST gateway reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Invoice {
    #[serde(default)]
    pub state: String,
    /// What the payer got for paying, hex; LND's REST gateway sends it as base64.
    #[serde(default, rename = "r_preimage", with = "base64_hex")]
    pub preimage: String,
    #[serde(default, with = "number")]
    pub amt_paid_sat: u64,
    #[serde(default, with = "number")]
    pub settle_date: u64,
}

/// One status update from LND's router, as its REST gateway streams it.
#[derive(Debug, Deserialize)]
struct Update {
    result: Option<Payment>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Payment {
    status: PaymentStatus,
    #[serde(default)]
    failure_reason: Option<String>,
    #[serde(default)]
    payment_hash: String,
    #[serde(default)]
    payment_preimage: String,
    #[serde(default, with = "number")]
    value_sat: u64,
    #[serde(default, with = "number")]
    fee_msat: u64,
    #[serde(default)]
    htlcs: Vec<Htlc>,
}

#[derive(Debug, Deserialize)]
struct Htlc {
    #[serde(default)]
    status: String,
    #[serde(default)]
    route: Option<Route>,
}

#[derive(Debug, Deserialize)]
struct Route {
    #[serde(default)]
    hops: Vec<Hop>,
}

impl Payment {
    fn paid(self) -> Paid {
        let route = self
            .htlcs
            .into_iter()
            .filter(|htlc| htlc.status == "SUCCEEDED")
            .find_map(|htlc| htlc.route)
            .map(|route| route.hops)
            .unwrap_or_default();
        Paid {
            payment_hash: self.payment_hash,
            preimage: self.payment_preimage,
            value_sat: self.value_sat,
            fee_msat: self.fee_msat,
            route,
        }
    }
}

/// Whether LND's error says it never made the payment, rather than that the lookup failed.
fn never_made(error: &str) -> bool {
    error.contains("isn't initiated") || error.contains("not found")
}

/// The last payment update in a router stream.
fn last_payment(body: &str) -> Result<Payment> {
    let last = body
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow!("LND said nothing about the payment"))?;
    let update: Update = serde_json::from_str(last).with_context(|| format!("parse {last}"))?;
    if let Some(error) = update.error {
        anyhow::bail!("LND could not pay: {error}");
    }
    update
        .result
        .ok_or_else(|| anyhow!("LND reported no payment"))
}

/// A channel's balance, from this node's side.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Channel {
    pub id: String,
    pub local_sats: u64,
    pub remote_sats: u64,
}

/// LND's REST gateway writes bytes as base64; everything else names them in hex.
mod base64_hex {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let text = Option::<String>::deserialize(deserializer)?.unwrap_or_default();
        let bytes = STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)?;
        Ok(hex::encode(bytes))
    }
}

/// LND's REST gateway writes 64-bit numbers as strings, and leaves out zeroes.
mod number {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Number {
            Text(String),
            Plain(u64),
        }
        match Number::deserialize(deserializer)? {
            Number::Text(text) if text.is_empty() => Ok(0),
            Number::Text(text) => text.parse().map_err(serde::de::Error::custom),
            Number::Plain(number) => Ok(number),
        }
    }
}

/// LND's REST gateway writes 64-bit amounts as strings.
mod sats {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The router streams each attempt; the last line says how the payment ended, with the
    /// preimage, the fee, and the route of the part that went through.
    #[test]
    fn a_settled_payment_keeps_its_preimage_fee_and_route() {
        let stream = r#"{"result":{"payment_hash":"aa","status":"IN_FLIGHT","value_sat":"1100"}}
{"result":{"payment_hash":"aa","payment_preimage":"bb","status":"SUCCEEDED","value_sat":"1100","fee_msat":"1001","htlcs":[{"status":"FAILED","route":{"hops":[{"chan_id":"1","pub_key":"x"}]}},{"status":"SUCCEEDED","route":{"hops":[{"chan_id":"3771505203178766336","pub_key":"odin"},{"chan_id":"9","pub_key":"swapd"}]}}]}}
"#;
        let paid = last_payment(stream).unwrap().paid();
        assert_eq!(paid.preimage, "bb");
        assert_eq!(paid.value_sat, 1100);
        assert_eq!(paid.fee_msat, 1001);
        assert_eq!(
            paid.fee_sat(),
            2,
            "a part of a sat still costs the payer one"
        );
        assert_eq!(
            paid.route
                .iter()
                .map(|hop| hop.chan_id.as_str())
                .collect::<Vec<_>>(),
            ["3771505203178766336", "9"],
            "the route of the attempt that went through"
        );
    }

    #[test]
    fn a_payment_the_node_never_made_is_an_error_in_the_stream() {
        let error = last_payment(r#"{"error":{"code":5,"message":"payment isn't initiated"}}"#)
            .unwrap_err();
        assert!(format!("{error:#}").contains("isn't initiated"));
    }

    #[test]
    fn an_invoice_names_its_preimage_in_hex() {
        let invoice: Invoice = serde_json::from_str(
            r#"{"state":"SETTLED","r_preimage":"q80=","amt_paid_sat":"990","settle_date":"1790213831"}"#,
        )
        .unwrap();
        assert_eq!(invoice.preimage, "abcd");
        assert_eq!(invoice.amt_paid_sat, 990);
        assert_eq!(invoice.state, "SETTLED");
    }
}
