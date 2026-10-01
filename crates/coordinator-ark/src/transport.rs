//! The calls this crate makes to an Arkade server: a kickoff's or a recovery's batch, and an
//! offchain spend. It also watches scripts through the server's indexer, so a caller learns of a
//! payment into an address without listing it again and again.
//!
//! [`ArkClient`] implements this over [`ark_grpc::Client`]. Tests replace it with a scripted
//! server.

use ark_core::intent::Intent;
use ark_core::server::{GetVtxosRequest, NoncePks, PartialSigTree, StreamEvent, VirtualTxOutPoint};
use ark_core::ArkAddress;
use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Amount, OutPoint, Psbt, ScriptBuf, Txid};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::sync::{Arc, RwLock};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::Error;

/// Batch events for the topics subscribed to.
pub type EventStream<'a> = BoxStream<'a, Result<StreamEvent, Error>>;

/// What the indexer sends on a script subscription.
pub type SubscriptionStream = BoxStream<'static, Result<SubscriptionEvent, Error>>;

/// One message of a script subscription's stream.
#[derive(Debug, Clone, PartialEq)]
pub enum SubscriptionEvent {
    /// The stream is open for this subscription.
    Started(String),
    /// Nothing happened, but the server is still there.
    Heartbeat,
    /// A transaction touched subscribed scripts.
    Transaction(ScriptTransaction),
}

/// A transaction that created or spent VTXOs at subscribed scripts.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptTransaction {
    pub txid: String,
    /// The subscribed scripts it concerns, as hex output scripts.
    pub scripts: Vec<String>,
    /// The VTXOs it created.
    pub new_vtxos: Vec<VirtualTxOutPoint>,
    /// The VTXOs it spent.
    pub spent_vtxos: Vec<VirtualTxOutPoint>,
}

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

    /// Hand the server `cosigner`'s nonces for the transactions of a batch's VTXO tree.
    ///
    /// An intent that receives a VTXO lists a cosigner key, which signs every tree transaction
    /// on the way to that VTXO with the server: nonces first, then
    /// [`ArkTransport::submit_tree_signatures`].
    async fn submit_tree_nonces(
        &self,
        batch_id: &str,
        cosigner: PublicKey,
        nonces: NoncePks,
    ) -> Result<(), Error>;

    /// Hand the server `cosigner`'s partial signatures for those transactions.
    async fn submit_tree_signatures(
        &self,
        batch_id: &str,
        cosigner: PublicKey,
        signatures: PartialSigTree,
    ) -> Result<(), Error>;

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

    /// Watch `scripts`, hex output scripts, returning the subscription's ID.
    ///
    /// Without `subscription` the server starts a new one; with it, the scripts are added to
    /// that one.
    async fn subscribe_scripts(
        &self,
        scripts: Vec<String>,
        subscription: Option<String>,
    ) -> Result<String, Error>;

    /// Stop watching `scripts` in `subscription`. No scripts is no change.
    async fn unsubscribe_scripts(
        &self,
        subscription: &str,
        scripts: Vec<String>,
    ) -> Result<(), Error>;

    /// The events of `subscription`, until the server ends the stream.
    async fn subscription_events(&self, subscription: &str) -> Result<SubscriptionStream, Error>;
}

/// What the server returns for a submitted offchain spend: its own signatures on the Ark
/// transaction, and the checkpoints still waiting for the owner's.
pub struct OffchainSubmission {
    pub ark_tx: Psbt,
    pub checkpoints: Vec<Psbt>,
}

/// An arkd connection: [`ark_grpc::Client`], and a channel of its own for the calls it lacks.
///
/// ark-grpc 0.11 generates the `DeleteIntent` call but keeps its generated code private, keeps
/// its channel to itself, and has no call for the indexer's script subscriptions. So this opens
/// a second channel to the same server, with the same TLS roots and headers, and makes those
/// calls with hand-written messages whose field numbers match `ark.v1`'s. Every other call goes
/// through ark-grpc.
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

    /// `message`, with the headers ark-grpc sends with every call; arkd may refuse a call
    /// without them.
    fn request<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        let metadata = request.metadata_mut();
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
        request
    }

    async fn ready(&self) -> Result<tonic::client::Grpc<Channel>, tonic::Status> {
        let mut grpc = tonic::client::Grpc::new(self.channel.clone());
        grpc.ready()
            .await
            .map_err(|error| tonic::Status::unavailable(format!("arkd is not ready: {error}")))?;
        Ok(grpc)
    }

    async fn call_unary<Req, Resp>(
        &self,
        message: Req,
        path: &'static str,
    ) -> Result<Resp, tonic::Status>
    where
        Req: prost::Message + Send + Sync + 'static,
        Resp: prost::Message + Default + Send + Sync + 'static,
    {
        let request = self.request(message);
        let mut grpc = self.ready().await?;
        let codec = tonic_prost::ProstCodec::<Req, Resp>::default();
        let response = grpc
            .unary(request, PathAndQuery::from_static(path), codec)
            .await?;
        Ok(response.into_inner())
    }

    /// Make a hand-written unary call at `path`.
    async fn unary<Req, Resp>(&self, message: Req, path: &'static str) -> Result<Resp, Error>
    where
        Req: prost::Message + Clone + Send + Sync + 'static,
        Resp: prost::Message + Default + Send + Sync + 'static,
    {
        match self.call_unary(message.clone(), path).await {
            // arkd's configuration changed, so it wants the new digest: learn it, and try once more.
            Err(status) if is_digest_mismatch(&status) => {
                self.refresh_digest().await?;
                Ok(self.call_unary(message, path).await?)
            }
            result => Ok(result?),
        }
    }

    async fn refresh_digest(&self) -> Result<(), Error> {
        let info = self.grpc.get_info().await?;
        self.set_digest(info.digest);
        Ok(())
    }

    async fn open_subscription(
        &self,
        message: GetSubscriptionRequest,
    ) -> Result<tonic::Streaming<GetSubscriptionResponse>, tonic::Status> {
        let request = self.request(message);
        let mut grpc = self.ready().await?;
        let codec =
            tonic_prost::ProstCodec::<GetSubscriptionRequest, GetSubscriptionResponse>::default();
        let response = grpc
            .server_streaming(
                request,
                PathAndQuery::from_static("/ark.v1.IndexerService/GetSubscription"),
                codec,
            )
            .await?;
        Ok(response.into_inner())
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

/// `ark.v1.SubscribeForScriptsRequest`. An empty `subscription_id` starts a new subscription.
#[derive(Clone, PartialEq, prost::Message)]
struct SubscribeForScriptsRequest {
    #[prost(string, repeated, tag = "1")]
    scripts: Vec<String>,
    #[prost(string, tag = "2")]
    subscription_id: String,
}

/// `ark.v1.SubscribeForScriptsResponse`.
#[derive(Clone, PartialEq, prost::Message)]
struct SubscribeForScriptsResponse {
    #[prost(string, tag = "1")]
    subscription_id: String,
}

/// `ark.v1.UnsubscribeForScriptsRequest`.
#[derive(Clone, PartialEq, prost::Message)]
struct UnsubscribeForScriptsRequest {
    #[prost(string, tag = "1")]
    subscription_id: String,
    #[prost(string, repeated, tag = "2")]
    scripts: Vec<String>,
}

/// `ark.v1.UnsubscribeForScriptsResponse`, which is empty.
#[derive(Clone, PartialEq, prost::Message)]
struct UnsubscribeForScriptsResponse {}

/// `ark.v1.GetSubscriptionRequest`, without the filter this crate does not use.
#[derive(Clone, PartialEq, prost::Message)]
struct GetSubscriptionRequest {
    #[prost(string, tag = "1")]
    subscription_id: String,
}

/// `ark.v1.GetSubscriptionResponse`.
#[derive(Clone, PartialEq, prost::Message)]
struct GetSubscriptionResponse {
    #[prost(oneof = "SubscriptionData", tags = "1, 2, 3")]
    data: Option<SubscriptionData>,
}

/// `ark.v1.GetSubscriptionResponse.data`.
#[derive(Clone, PartialEq, prost::Oneof)]
enum SubscriptionData {
    #[prost(message, tag = "1")]
    Heartbeat(IndexerHeartbeat),
    #[prost(message, tag = "2")]
    Event(IndexerSubscriptionEvent),
    #[prost(message, tag = "3")]
    SubscriptionStarted(SubscriptionStartedEvent),
}

/// `ark.v1.IndexerHeartbeat`, which is empty.
#[derive(Clone, PartialEq, prost::Message)]
struct IndexerHeartbeat {}

/// `ark.v1.SubscriptionStartedEvent`.
#[derive(Clone, PartialEq, prost::Message)]
struct SubscriptionStartedEvent {
    #[prost(string, tag = "1")]
    subscription_id: String,
}

/// `ark.v1.IndexerSubscriptionEvent`, without the transactions themselves and the swept VTXOs,
/// which this crate does not use.
#[derive(Clone, PartialEq, prost::Message)]
struct IndexerSubscriptionEvent {
    #[prost(string, tag = "1")]
    txid: String,
    #[prost(string, repeated, tag = "2")]
    scripts: Vec<String>,
    #[prost(message, repeated, tag = "3")]
    new_vtxos: Vec<IndexerVtxo>,
    #[prost(message, repeated, tag = "4")]
    spent_vtxos: Vec<IndexerVtxo>,
}

/// `ark.v1.IndexerOutpoint`.
#[derive(Clone, PartialEq, prost::Message)]
struct IndexerOutpoint {
    #[prost(string, tag = "1")]
    txid: String,
    #[prost(uint32, tag = "2")]
    vout: u32,
}

/// `ark.v1.IndexerVtxo`, with the fields that say where a VTXO is, what it holds, and whether
/// it can still be spent.
#[derive(Clone, PartialEq, prost::Message)]
struct IndexerVtxo {
    #[prost(message, optional, tag = "1")]
    outpoint: Option<IndexerOutpoint>,
    #[prost(int64, tag = "2")]
    created_at: i64,
    #[prost(int64, tag = "3")]
    expires_at: i64,
    #[prost(uint64, tag = "4")]
    amount: u64,
    #[prost(string, tag = "5")]
    script: String,
    #[prost(bool, tag = "6")]
    is_preconfirmed: bool,
    #[prost(bool, tag = "7")]
    is_swept: bool,
    #[prost(bool, tag = "8")]
    is_unrolled: bool,
    #[prost(bool, tag = "9")]
    is_spent: bool,
    #[prost(string, tag = "13")]
    ark_txid: String,
    #[prost(uint32, tag = "15")]
    depth: u32,
}

impl TryFrom<IndexerVtxo> for VirtualTxOutPoint {
    type Error = Error;

    fn try_from(vtxo: IndexerVtxo) -> Result<Self, Error> {
        let invalid = |what: &str| Error::ServerInfo(format!("the indexer sent {what}"));
        let outpoint = vtxo
            .outpoint
            .ok_or_else(|| invalid("a VTXO without its outpoint"))?;
        let txid: Txid = outpoint
            .txid
            .parse()
            .map_err(|_| invalid("an invalid VTXO txid"))?;
        let ark_txid = match vtxo.ark_txid.as_str() {
            "" => None,
            txid => Some(txid.parse().map_err(|_| invalid("an invalid Ark txid"))?),
        };
        Ok(VirtualTxOutPoint {
            outpoint: OutPoint::new(txid, outpoint.vout),
            created_at: vtxo.created_at,
            expires_at: vtxo.expires_at,
            amount: Amount::from_sat(vtxo.amount),
            script: ScriptBuf::from_hex(&vtxo.script)
                .map_err(|_| invalid("an invalid VTXO script"))?,
            is_preconfirmed: vtxo.is_preconfirmed,
            is_swept: vtxo.is_swept,
            is_unrolled: vtxo.is_unrolled,
            is_spent: vtxo.is_spent,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid,
            assets: Vec::new(),
            depth: vtxo.depth,
        })
    }
}

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
        let _: DeleteIntentResponse = self
            .unary(request, "/ark.v1.ArkService/DeleteIntent")
            .await?;
        Ok(())
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

    async fn submit_tree_nonces(
        &self,
        batch_id: &str,
        cosigner: PublicKey,
        nonces: NoncePks,
    ) -> Result<(), Error> {
        Ok(self
            .grpc
            .submit_tree_nonces(batch_id, cosigner, nonces)
            .await?)
    }

    async fn submit_tree_signatures(
        &self,
        batch_id: &str,
        cosigner: PublicKey,
        signatures: PartialSigTree,
    ) -> Result<(), Error> {
        Ok(self
            .grpc
            .submit_tree_signatures(batch_id, cosigner, signatures)
            .await?)
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

    async fn subscribe_scripts(
        &self,
        scripts: Vec<String>,
        subscription: Option<String>,
    ) -> Result<String, Error> {
        let request = SubscribeForScriptsRequest {
            scripts,
            subscription_id: subscription.unwrap_or_default(),
        };
        let response: SubscribeForScriptsResponse = self
            .unary(request, "/ark.v1.IndexerService/SubscribeForScripts")
            .await?;
        Ok(response.subscription_id)
    }

    async fn unsubscribe_scripts(
        &self,
        subscription: &str,
        scripts: Vec<String>,
    ) -> Result<(), Error> {
        // The indexer takes no scripts to mean every one.
        if scripts.is_empty() {
            return Ok(());
        }
        let request = UnsubscribeForScriptsRequest {
            subscription_id: subscription.to_string(),
            scripts,
        };
        let _: UnsubscribeForScriptsResponse = self
            .unary(request, "/ark.v1.IndexerService/UnsubscribeForScripts")
            .await?;
        Ok(())
    }

    async fn subscription_events(&self, subscription: &str) -> Result<SubscriptionStream, Error> {
        let request = GetSubscriptionRequest {
            subscription_id: subscription.to_string(),
        };
        let stream = match self.open_subscription(request.clone()).await {
            Err(status) if is_digest_mismatch(&status) => {
                self.refresh_digest().await?;
                self.open_subscription(request).await?
            }
            result => result?,
        };
        Ok(stream
            .filter_map(|message| async move {
                match message {
                    Ok(response) => subscription_event(response).transpose(),
                    Err(status) => Some(Err(Error::from(status))),
                }
            })
            .boxed())
    }
}

/// The event a subscription's message carries. `None` for a message with no data, which a
/// newer server might send.
fn subscription_event(
    response: GetSubscriptionResponse,
) -> Result<Option<SubscriptionEvent>, Error> {
    let vtxos = |vtxos: Vec<IndexerVtxo>| {
        vtxos
            .into_iter()
            .map(VirtualTxOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(match response.data {
        None => None,
        Some(SubscriptionData::Heartbeat(_)) => Some(SubscriptionEvent::Heartbeat),
        Some(SubscriptionData::SubscriptionStarted(started)) => {
            Some(SubscriptionEvent::Started(started.subscription_id))
        }
        Some(SubscriptionData::Event(event)) => {
            Some(SubscriptionEvent::Transaction(ScriptTransaction {
                txid: event.txid,
                scripts: event.scripts,
                new_vtxos: vtxos(event.new_vtxos)?,
                spent_vtxos: vtxos(event.spent_vtxos)?,
            }))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    /// A length-delimited field: its key, for field `tag` and wire type 2, its length, then
    /// `bytes`.
    fn field(tag: u8, bytes: &[u8]) -> Vec<u8> {
        let mut encoded = vec![tag << 3 | 2];
        let mut length = bytes.len();
        while length >= 0x80 {
            encoded.push((length & 0x7f) as u8 | 0x80);
            length >>= 7;
        }
        encoded.push(length as u8);
        encoded.extend_from_slice(bytes);
        encoded
    }

    /// A varint field of a value below 128, or of a larger one given as its varint bytes.
    fn varint(tag: u8, value: &[u8]) -> Vec<u8> {
        let mut encoded = vec![tag << 3];
        encoded.extend_from_slice(value);
        encoded
    }

    #[test]
    fn subscription_requests_use_the_field_numbers_of_the_indexer_proto() {
        let subscribe = SubscribeForScriptsRequest {
            scripts: vec!["5120aa".into(), "5120bb".into()],
            subscription_id: "sub-1".into(),
        };
        let expected = [field(1, b"5120aa"), field(1, b"5120bb"), field(2, b"sub-1")].concat();
        assert_eq!(subscribe.encode_to_vec(), expected);
        // A new subscription sends no ID at all.
        let new = SubscribeForScriptsRequest {
            scripts: vec!["5120aa".into()],
            subscription_id: String::new(),
        };
        assert_eq!(new.encode_to_vec(), field(1, b"5120aa"));
        assert_eq!(
            SubscribeForScriptsResponse::decode(field(1, b"sub-1").as_slice())
                .unwrap()
                .subscription_id,
            "sub-1"
        );

        let unsubscribe = UnsubscribeForScriptsRequest {
            subscription_id: "sub-1".into(),
            scripts: vec!["5120aa".into()],
        };
        assert_eq!(
            unsubscribe.encode_to_vec(),
            [field(1, b"sub-1"), field(2, b"5120aa")].concat()
        );

        let open = GetSubscriptionRequest {
            subscription_id: "sub-1".into(),
        };
        assert_eq!(open.encode_to_vec(), field(1, b"sub-1"));
    }

    #[test]
    fn subscription_messages_decode_as_the_indexer_sends_them() {
        let decode = |bytes: Vec<u8>| {
            subscription_event(GetSubscriptionResponse::decode(bytes.as_slice()).unwrap()).unwrap()
        };
        assert_eq!(decode(field(1, &[])), Some(SubscriptionEvent::Heartbeat));
        assert_eq!(
            decode(field(3, &field(1, b"sub-1"))),
            Some(SubscriptionEvent::Started("sub-1".into()))
        );
        assert_eq!(decode(Vec::new()), None);

        let txid = "11".repeat(32);
        let script = format!("5120{}", "22".repeat(32));
        let vtxo = [
            field(1, &[field(1, txid.as_bytes()), varint(2, &[3])].concat()),
            // 1,700,000,000 as a varint.
            varint(3, &[0x80, 0xe2, 0xcf, 0xaa, 0x06]),
            // 5,500 sats as a varint.
            varint(4, &[0xfc, 0x2a]),
            field(5, script.as_bytes()),
            varint(6, &[1]),
            varint(9, &[1]),
            field(13, txid.as_bytes()),
            // A field this crate does not model is skipped.
            field(10, b"ignored"),
        ]
        .concat();
        let event = [
            field(1, txid.as_bytes()),
            field(2, script.as_bytes()),
            field(3, &vtxo),
            field(4, &vtxo),
            // So are the transaction and its checkpoints.
            field(5, b"cHNidP8="),
        ]
        .concat();
        let Some(SubscriptionEvent::Transaction(transaction)) = decode(field(2, &event)) else {
            panic!("a transaction event");
        };
        assert_eq!(transaction.txid, txid);
        assert_eq!(transaction.scripts, vec![script.clone()]);
        assert_eq!(transaction.new_vtxos.len(), 1);
        assert_eq!(transaction.spent_vtxos.len(), 1);
        let vtxo = &transaction.new_vtxos[0];
        assert_eq!(vtxo.outpoint, OutPoint::new(txid.parse().unwrap(), 3));
        assert_eq!(vtxo.expires_at, 1_700_000_000);
        assert_eq!(vtxo.amount, Amount::from_sat(5_500));
        assert_eq!(vtxo.script, ScriptBuf::from_hex(&script).unwrap());
        assert!(vtxo.is_preconfirmed && vtxo.is_spent && !vtxo.is_swept);
        assert_eq!(vtxo.ark_txid, Some(txid.parse().unwrap()));
    }
}
