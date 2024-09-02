use async_trait::async_trait;
use reqwest_middleware::{reqwest::Url, ClientWithMiddleware};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct LnClient {
    pub base_url: Url,
    pub client: ClientWithMiddleware,
    pub macaroon: SecretString,
}

#[async_trait]
pub trait Ln {
    async fn add_hold_invoice(
        &self,
        ticket_hash: String,
    ) -> Result<InvoiceAddResponse, anyhow::Error>;
    async fn cancel_hold_invoice(&self, payment_hash: String) -> Result<(), anyhow::Error>;
    async fn settle_hold_invoice(&self, ticker_preimage: String) -> Result<(), anyhow::Error>;
}

impl LnClient {
    pub fn new(base_url: Url, client: ClientWithMiddleware, macaroon: SecretString) -> Self {
        Self {
            base_url,
            client,
            macaroon,
        }
    }
}

const MACAROON_HEADER: &str = "Grpc-Metadata-macaroon";

#[derive(Debug, Serialize, Deserialize)]
pub struct InvoiceAddResponse {
    pub payment_request: String,
    pub add_index: String,
    pub payment_addr: String,
}

#[async_trait]
impl Ln for LnClient {
    async fn add_hold_invoice(
        &self,
        ticket_hash: String,
    ) -> Result<InvoiceAddResponse, anyhow::Error> {
        /*let body = json! {
          memo: <string>, // <string>
          hash: <string>, // <bytes> (base64 encoded)
          value: <string>, // <int64>
          value_msat: <string>, // <int64>
          description_hash: <string>, // <bytes> (base64 encoded)
          expiry: <string>, // <int64>
          fallback_addr: <string>, // <string>
          cltv_expiry: <string>, // <uint64>
          route_hints: <array>, // <RouteHint>
          private: <boolean>, // <bool>
        };
        self.client
            .post(format!("{}/v2/invoices/hodl", self.base_url))
            .body(body)
            .header(MACAROON_HEADER, self.macaroon.expose_secret())
            .await*/
        todo!()
    }

    async fn cancel_hold_invoice(&self, payment_hash: String) -> Result<(), anyhow::Error> {
        todo!()
    }

    async fn settle_hold_invoice(&self, ticker_preimage: String) -> Result<(), anyhow::Error> {
        todo!()
    }
}
