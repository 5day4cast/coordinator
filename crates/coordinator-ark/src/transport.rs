//! The batch calls the kickoff makes to an Arkade server.
//!
//! [`ark_grpc::Client`] implements this. Tests replace it with a scripted server.

use ark_core::intent::Intent;
use ark_core::server::StreamEvent;
use async_trait::async_trait;
use bitcoin::Psbt;
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
}
