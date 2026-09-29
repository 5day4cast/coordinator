//! The calls this crate makes to an Arkade server: a kickoff's batch, and an offchain spend.
//!
//! [`ArkClient`] implements this over [`ark_grpc::Client`]. Tests replace it with a scripted
//! server.

use ark_core::intent::Intent;
use ark_core::server::{GetVtxosRequest, StreamEvent, VirtualTxOutPoint};
use ark_core::ArkAddress;
use async_trait::async_trait;
use bitcoin::{Psbt, Txid};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::sync::{Arc, RwLock};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::Error;

/// Batch events for the topics subscribed to.
pub type EventStream<'a> = BoxStream<'a, Result<StreamEvent, Error>>;

#[async_trait]
pub trait ArkTransport: Send + Sync {
    /// Register an intent for the next batch, returning its ID.
    async fn register_intent(&self, intent: Intent) -> Result<String, Error>;

    /// Delete every queued intent that spends an input of `proof`, a BIP322 proof over a
    /// `delete` message.
    ///
    /// arkd keeps an intent queued until a batch confirms it or its owner deletes it, and until
    /// then refuses any other spend of its VTXOs. A proof over any one input deletes the whole
    /// intent. [`Error::NoMatchingIntent`] means nothing was queued.
    async fn delete_intent(&self, proof: Intent) -> Result<(), Error>;

    /// Confirm a registered intent once a batch has selected it.
    async fn confirm_registration(&self, intent_id: String) -> Result<(), Error>;

    /// Subscribe to batch events.
    ///
    /// Topics are the VTXO outpoints being spent and the intent's cosigner keys.
    async fn event_stream(&self, topics: Vec<String>) -> Result<EventStream<'_>, Error>;

    /// Hand the server the signed forfeit transactions.
    async fn submit_forfeits(&self, forfeits: Vec<Psbt>) -> Result<(), Error>;

    /// Submit an offchain spend for the server to co-sign, leaving it pending.
    ///
    /// The owner signs the Ark transaction first, because the server's signatures come back with
    /// this call; the checkpoints it returns are then signed and handed to
    /// [`ArkTransport::finalize_offchain`].
    async fn submit_offchain(
        &self,
        ark_tx: Psbt,
        checkpoints: Vec<Psbt>,
    ) -> Result<OffchainSubmission, Error>;

    /// Finalize a submitted spend, once its checkpoints carry every signature.
    async fn finalize_offchain(&self, ark_txid: Txid, checkpoints: Vec<Psbt>) -> Result<(), Error>;

    /// The server's view of the VTXOs at `addresses`, encoded, including the spent ones.
    async fn vtxos(&self, addresses: Vec<String>) -> Result<Vec<VirtualTxOutPoint>, Error>;
}

/// What the server returns for a submitted offchain spend: its own signatures on the Ark
/// transaction, and the checkpoints still waiting for the owner's.
pub struct OffchainSubmission {
    pub ark_tx: Psbt,
    pub checkpoints: Vec<Psbt>,
}

/// An arkd connection: [`ark_grpc::Client`], and a channel of its own for `DeleteIntent`.
///
/// ark-grpc 0.11 generates the `DeleteIntent` call but keeps its generated code private, and
/// keeps its channel to itself. So this opens a second channel to the same server, with the same
/// TLS roots and headers, and makes that one call with hand-written messages whose field numbers
/// match `ark.v1`'s. Every other call goes through ark-grpc.
#[derive(Clone)]
pub struct ArkClient {
    grpc: ark_grpc::Client,
    channel: Channel,
    /// The server's `/v1/info` digest, which arkd may require on every call.
    digest: Arc<RwLock<String>>,
}

/// Install ring as rustls's process-wide crypto provider, unless the process has already chosen.
///
/// tonic's TLS uses that provider, even to build a config, and panics without one. This
/// workspace compiles in both ring and aws-lc-rs, so rustls cannot pick one itself. Call this
/// before anything opens a TLS channel, ark-grpc's included.
pub(crate) fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl ArkClient {
    /// Wrap `grpc`, connected to `url`, whose `/v1/info` has `digest`.
    ///
    /// The second channel connects on its first call, so this makes no request.
    pub fn new(grpc: ark_grpc::Client, url: &str, digest: String) -> Result<Self, Error> {
        let invalid = |error: tonic::transport::Error| {
            Error::ServerInfo(format!("cannot reach {url}: {error}"))
        };
        install_crypto_provider();
        let tls = ClientTlsConfig::new().with_webpki_roots();
        let channel = Endpoint::from_shared(url.to_string())
            .map_err(invalid)?
            .tls_config(tls)
            .map_err(invalid)?
            .connect_lazy();
        Ok(Self {
            grpc,
            channel,
            digest: Arc::new(RwLock::new(digest)),
        })
    }

    /// The ark-grpc client, for the calls [`ArkTransport`] does not make.
    pub fn grpc(&self) -> &ark_grpc::Client {
        &self.grpc
    }

    fn set_digest(&self, digest: String) {
        *self
            .digest
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = digest;
    }

    async fn call_delete_intent(&self, request: DeleteIntentRequest) -> Result<(), tonic::Status> {
        let mut request = tonic::Request::new(request);
        let metadata = request.metadata_mut();
        // The headers ark-grpc sends with every call; arkd may refuse a call without them.
        metadata.insert(
            "x-build-version",
            MetadataValue::from_static(ark_core::server::TARGET_ARKD_VERSION),
        );
        metadata.insert(
            "x-sdk-version",
            MetadataValue::from_static(ark_core::server::SDK_VERSION),
        );
        let digest = self
            .digest
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if !digest.is_empty() {
            if let Ok(value) = MetadataValue::try_from(digest.as_str()) {
                metadata.insert("x-digest", value);
            }
        }
        let mut grpc = tonic::client::Grpc::new(self.channel.clone());
        grpc.ready()
            .await
            .map_err(|error| tonic::Status::unavailable(format!("arkd is not ready: {error}")))?;
        let codec = tonic_prost::ProstCodec::<DeleteIntentRequest, DeleteIntentResponse>::default();
        grpc.unary(
            request,
            PathAndQuery::from_static("/ark.v1.ArkService/DeleteIntent"),
            codec,
        )
        .await?;
        Ok(())
    }
}

/// Whether arkd refused a call for a stale `/v1/info` digest.
fn is_digest_mismatch(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::FailedPrecondition
        && (status.message().contains("DIGEST_MISMATCH")
            || status.message().contains("invalid digest header"))
}

/// `ark.v1.Intent`: a base64 PSBT proof and its JSON message.
#[derive(Clone, PartialEq, prost::Message)]
struct IntentProof {
    #[prost(string, tag = "1")]
    proof: String,
    #[prost(string, tag = "2")]
    message: String,
}

/// `ark.v1.DeleteIntentRequest`.
#[derive(Clone, PartialEq, prost::Message)]
struct DeleteIntentRequest {
    #[prost(message, optional, tag = "1")]
    intent: Option<IntentProof>,
}

/// `ark.v1.DeleteIntentResponse`, which is empty.
#[derive(Clone, PartialEq, prost::Message)]
struct DeleteIntentResponse {}

#[async_trait]
impl ArkTransport for ArkClient {
    async fn register_intent(&self, intent: Intent) -> Result<String, Error> {
        Ok(self.grpc.register_intent(intent).await?)
    }

    async fn delete_intent(&self, proof: Intent) -> Result<(), Error> {
        let request = DeleteIntentRequest {
            intent: Some(IntentProof {
                proof: proof.serialize_proof(),
                message: proof.serialize_message()?,
            }),
        };
        match self.call_delete_intent(request.clone()).await {
            // arkd's configuration changed, so it wants the new digest: learn it, and try once more.
            Err(status) if is_digest_mismatch(&status) => {
                let info = self.grpc.get_info().await?;
                self.set_digest(info.digest);
                Ok(self.call_delete_intent(request).await?)
            }
            result => Ok(result?),
        }
    }

    async fn confirm_registration(&self, intent_id: String) -> Result<(), Error> {
        Ok(self.grpc.confirm_registration(intent_id).await?)
    }

    async fn event_stream(&self, topics: Vec<String>) -> Result<EventStream<'_>, Error> {
        let stream = self.grpc.get_event_stream(topics).await?;
        Ok(stream.map(|event| event.map_err(Error::from)).boxed())
    }

    async fn submit_forfeits(&self, forfeits: Vec<Psbt>) -> Result<(), Error> {
        Ok(self.grpc.submit_signed_forfeit_txs(forfeits, None).await?)
    }

    async fn submit_offchain(
        &self,
        ark_tx: Psbt,
        checkpoints: Vec<Psbt>,
    ) -> Result<OffchainSubmission, Error> {
        let response = self
            .grpc
            .submit_offchain_transaction_request(ark_tx, checkpoints)
            .await?;
        Ok(OffchainSubmission {
            ark_tx: response.signed_ark_tx,
            checkpoints: response.signed_checkpoint_txs,
        })
    }

    async fn finalize_offchain(&self, ark_txid: Txid, checkpoints: Vec<Psbt>) -> Result<(), Error> {
        self.grpc
            .finalize_offchain_transaction(ark_txid, checkpoints)
            .await?;
        Ok(())
    }

    async fn vtxos(&self, addresses: Vec<String>) -> Result<Vec<VirtualTxOutPoint>, Error> {
        let addresses = addresses
            .iter()
            .map(|address| ArkAddress::decode(address))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| Error::ServerInfo(format!("invalid Ark address: {error}")))?;
        Ok(self
            .grpc
            .list_vtxos(GetVtxosRequest::new_for_addresses(addresses.into_iter()))
            .await?
            .vtxos)
    }
}
