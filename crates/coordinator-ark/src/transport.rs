//! The calls this crate makes to an Arkade server: a kickoff's batch, and an offchain spend.
//!
//! [`ark_grpc::Client`] implements this. Tests replace it with a scripted server.

use ark_core::intent::Intent;
use ark_core::server::{GetVtxosRequest, StreamEvent, VirtualTxOutPoint};
use ark_core::ArkAddress;
use async_trait::async_trait;
use bitcoin::{Psbt, Txid};
use futures::stream::BoxStream;
use futures::StreamExt;

use crate::Error;

/// Batch events for the topics subscribed to.
pub type EventStream<'a> = BoxStream<'a, Result<StreamEvent, Error>>;

#[async_trait]
pub trait ArkTransport: Send + Sync {
    /// Register an intent for the next batch, returning its ID.
    async fn register_intent(&self, intent: Intent) -> Result<String, Error>;

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

#[async_trait]
impl ArkTransport for ark_grpc::Client {
    async fn register_intent(&self, intent: Intent) -> Result<String, Error> {
        Ok(ark_grpc::Client::register_intent(self, intent).await?)
    }

    async fn confirm_registration(&self, intent_id: String) -> Result<(), Error> {
        Ok(ark_grpc::Client::confirm_registration(self, intent_id).await?)
    }

    async fn event_stream(&self, topics: Vec<String>) -> Result<EventStream<'_>, Error> {
        let stream = self.get_event_stream(topics).await?;
        Ok(stream.map(|event| event.map_err(Error::from)).boxed())
    }

    async fn submit_forfeits(&self, forfeits: Vec<Psbt>) -> Result<(), Error> {
        Ok(self.submit_signed_forfeit_txs(forfeits, None).await?)
    }

    async fn submit_offchain(
        &self,
        ark_tx: Psbt,
        checkpoints: Vec<Psbt>,
    ) -> Result<OffchainSubmission, Error> {
        let response = self
            .submit_offchain_transaction_request(ark_tx, checkpoints)
            .await?;
        Ok(OffchainSubmission {
            ark_tx: response.signed_ark_tx,
            checkpoints: response.signed_checkpoint_txs,
        })
    }

    async fn finalize_offchain(&self, ark_txid: Txid, checkpoints: Vec<Psbt>) -> Result<(), Error> {
        self.finalize_offchain_transaction(ark_txid, checkpoints)
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
            .list_vtxos(GetVtxosRequest::new_for_addresses(addresses.into_iter()))
            .await?
            .vtxos)
    }
}
