use anyhow::anyhow;
use bitcoin::{
    consensus::encode::deserialize,
    hashes::{sha256, Hash},
    PublicKey, Transaction,
};
use log::{debug, error, info, warn};
use std::{str::FromStr, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{competitions::Ticket, Coordinator},
    infra::{
        db::DatabaseWriteError,
        escrow::generate_escrow_tx,
        lightning::{InvoiceState, Ln},
    },
};

const MAX_BROADCAST_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;

pub struct InvoiceWatcher {
    coordinator: Arc<Coordinator>,
    ln: Arc<dyn Ln>,
    sync_interval: Duration,
    cancel_token: CancellationToken,
}

impl InvoiceWatcher {
    pub fn new(
        coordinator: Arc<Coordinator>,
        ln: Arc<dyn Ln>,
        cancel_token: CancellationToken,
        sync_interval: Duration,
    ) -> Self {
        Self {
            coordinator,
            ln,
            sync_interval,
            cancel_token,
        }
    }

    pub async fn watch(&self) -> Result<(), anyhow::Error> {
        info!("Starting Invoice watcher");

        loop {
            if self.cancel_token.is_cancelled() {
                info!("Invoice watcher received cancellation");
                break;
            }

            // Settling and broadcasting run in one coordinator at a time.
            match self
                .coordinator
                .worker_leases()
                .tick("invoice-watcher", self.handle_pending_invoices())
                .await
            {
                Some(Ok(_)) => {
                    debug!("Invoice handling completed successfully");
                }
                Some(Err(e)) => {
                    error!("Invoice handling error: {}", e);
                }
                None => debug!("Another coordinator watches invoices"),
            }

            tokio::select! {
                _ = sleep(self.sync_interval) => continue,
                _ = self.cancel_token.cancelled() => {
                    info!("Invoice watcher cancelled during sleep");
                    break;
                }
            }
        }

        self.coordinator
            .worker_leases()
            .release("invoice-watcher")
            .await;
        Ok(())
    }

    async fn handle_pending_invoices(&self) -> Result<(), anyhow::Error> {
        let pending_tickets = self
            .coordinator
            .competition_store
            .get_pending_tickets()
            .await?;

        debug!("Checking {} pending tickets", pending_tickets.len());

        for ticket in pending_tickets {
            match self.ln.lookup_invoice(&ticket.hash).await {
                Ok(invoice) => {
                    debug!("Ticket {}: invoice state: {:?}", ticket.id, invoice.state);

                    // Handle expired/canceled invoices - clear reservation so ticket can be reused
                    if invoice.state == InvoiceState::Canceled {
                        if ticket.paid_at.is_some() || ticket.escrow_transaction.is_some() {
                            // The ticket keeps its paid and escrow state for recovery. Recording
                            // the cancellation ends the polling: a cancelled invoice never
                            // changes again, and cleanup reclaims any escrow.
                            match self
                                .coordinator
                                .competition_store
                                .mark_ticket_invoice_cancelled(ticket.id)
                                .await
                            {
                                Ok(_) => warn!(
                                    "Invoice of paid ticket {} was cancelled; its escrow, if any, is left to cleanup",
                                    ticket.id
                                ),
                                Err(e) => error!(
                                    "Failed to record the cancelled invoice of ticket {}: {}",
                                    ticket.id, e
                                ),
                            }
                            continue;
                        }
                        info!(
                            "Invoice expired/canceled for ticket {}, clearing reservation",
                            ticket.id
                        );
                        match self
                            .coordinator
                            .competition_store
                            .clear_ticket_reservation(&ticket)
                            .await
                        {
                            Ok(_) => {
                                info!(
                                    "Successfully cleared reservation for expired ticket {}",
                                    ticket.id
                                );
                            }
                            Err(e) => {
                                error!(
                                    "Failed to clear reservation for expired ticket {}: {}",
                                    ticket.id, e
                                );
                            }
                        }
                        continue;
                    }

                    if matches!(
                        invoice.state,
                        InvoiceState::Accepted | InvoiceState::Settled
                    ) {
                        // The subscription may already have marked this ticket
                        // paid, or a previous process may have stopped after that
                        // commit. Continue from current durable state either way.
                        if let Err(error) = self
                            .coordinator
                            .competition_store
                            .mark_ticket_paid(&ticket.hash, ticket.competition_id)
                            .await
                        {
                            error!("Failed to mark ticket {} paid: {}", ticket.id, error);
                            continue;
                        }
                        self.coordinator.wake_competition(ticket.competition_id);
                        let current = self
                            .coordinator
                            .competition_store
                            .get_ticket(ticket.id)
                            .await?;
                        if current.hash != ticket.hash
                            || current.paid_at.is_none()
                            || current.settled_at.is_some()
                        {
                            continue;
                        }
                        if self.coordinator.is_escrow_enabled() {
                            if let Err(error) = self.broadcast_persisted_escrow(&current).await {
                                // Publication can have succeeded despite an RPC
                                // error. Retain the exact transaction, ticket hash
                                // and held invoice for reconciliation on retry.
                                warn!("Escrow for ticket {} remains pending: {}", ticket.id, error);
                                continue;
                            }
                            if invoice.state == InvoiceState::Accepted {
                                self.settle_invoice_and_mark_ticket(&current).await;
                            } else {
                                self.coordinator
                                    .competition_store
                                    .mark_ticket_settled(current.id)
                                    .await?;
                            }
                        } else if invoice.state == InvoiceState::Settled {
                            self.coordinator
                                .competition_store
                                .mark_ticket_settled(current.id)
                                .await?;
                        }
                        // Without escrow, an accepted invoice remains held until
                        // the competition's funding transaction is published.
                    }
                }
                Err(e) => {
                    debug!("Failed to lookup invoice for ticket {}: {}", ticket.id, e);
                }
            }
        }

        Ok(())
    }

    async fn settle_invoice_and_mark_ticket(&self, ticket: &Ticket) {
        match self
            .ln
            .settle_hold_invoice(ticket.encrypted_preimage.clone())
            .await
        {
            Ok(_) => {
                match self
                    .coordinator
                    .competition_store
                    .mark_ticket_settled(ticket.id)
                    .await
                {
                    Ok(_) => info!(
                        "Ticket {} settled for competition {}",
                        ticket.id, ticket.competition_id
                    ),
                    Err(e) => error!("Failed to mark ticket {} as settled: {}", ticket.id, e),
                }
            }
            Err(e) => {
                error!(
                    "Failed to settle HODL invoice for ticket {}: {}",
                    ticket.id, e
                );
            }
        }
    }

    async fn broadcast_persisted_escrow(&self, ticket: &Ticket) -> Result<String, anyhow::Error> {
        let transaction = if let Some(encoded) = &ticket.escrow_transaction {
            // Corrupt stored bytes fail closed; they never authorize fresh funds.
            deserialize::<Transaction>(&hex::decode(encoded)?)?
        } else {
            let competition = self
                .coordinator
                .competition_store
                .get_competition(ticket.competition_id)
                .await?;
            if competition.cancelled_at.is_some()
                || competition.failed_at.is_some()
                || competition.completed_at.is_some()
            {
                return Err(anyhow!("Competition no longer accepts escrow funding"));
            }
            let key = ticket
                .ephemeral_pubkey
                .as_ref()
                .ok_or_else(|| anyhow!("Ticket has no escrow public key"))?;
            let user_pubkey = PublicKey::from_str(key)?;
            let preimage = hex::decode(&ticket.encrypted_preimage)?;
            let payment_hash = sha256::Hash::hash(&preimage).to_byte_array();
            if hex::encode(payment_hash) != ticket.hash {
                return Err(anyhow!("Ticket preimage does not match its invoice hash"));
            }
            let prepared = generate_escrow_tx(
                self.coordinator.bitcoin.clone(),
                ticket.id,
                user_pubkey,
                payment_hash,
                competition.event_submission.entry_fee as u64,
                competition.funding_reservation_deadline(OffsetDateTime::now_utc())?,
            )
            .await?;
            let encoded = hex::encode(bitcoin::consensus::encode::serialize(&prepared.transaction));
            let stored = self
                .coordinator
                .competition_store
                .update_ticket_escrow_transaction(ticket, &encoded)
                .await;
            let definite_failure = matches!(
                &stored,
                Ok(false)
                    | Err(DatabaseWriteError::QueueFull
                        | DatabaseWriteError::ChannelClosed
                        | DatabaseWriteError::Sqlx(
                            sqlx::Error::Database(_)
                                | sqlx::Error::RowNotFound
                                | sqlx::Error::Protocol(_)
                        ))
            );
            if definite_failure {
                if let Err(error) = self
                    .coordinator
                    .bitcoin
                    .release_psbt_inputs(&prepared.funded_psbt)
                    .await
                {
                    warn!(
                        "Failed to release unpublished escrow inputs for ticket {}: {}",
                        ticket.id, error
                    );
                }
            }
            if !stored? {
                return Err(anyhow!(
                    "Ticket changed before escrow could be persisted; retry its stored state"
                ));
            }
            prepared.transaction
        };
        self.broadcast_with_retries(&transaction, ticket.id).await?;
        Ok(transaction.compute_txid().to_string())
    }

    async fn broadcast_with_retries(
        &self,
        transaction: &Transaction,
        ticket_id: uuid::Uuid,
    ) -> Result<(), anyhow::Error> {
        let mut last_error = None;

        for attempt in 1..=MAX_BROADCAST_RETRIES {
            match self.coordinator.bitcoin.broadcast(transaction).await {
                Ok(_) => {
                    info!(
                        "Successfully broadcasted transaction for ticket {} (attempt {}/{})",
                        ticket_id, attempt, MAX_BROADCAST_RETRIES
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        "Failed to broadcast transaction for ticket {} (attempt {}/{}): {}",
                        ticket_id, attempt, MAX_BROADCAST_RETRIES, e
                    );
                    // A lost publication response or an already-known error
                    // is reconciled by fetching the exact transaction. Failure
                    // to observe it is inconclusive and leaves persistence intact.
                    if self
                        .coordinator
                        .bitcoin
                        .get_raw_transaction(&transaction.compute_txid())
                        .await
                        .is_ok_and(|known| known == *transaction)
                    {
                        return Ok(());
                    }
                    last_error = Some(e);

                    if attempt < MAX_BROADCAST_RETRIES {
                        // Exponential backoff: 1s, 2s, 4s
                        let delay = Duration::from_millis(RETRY_DELAY_MS * (1 << (attempt - 1)));
                        sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow!(
                "Failed to broadcast after {} attempts",
                MAX_BROADCAST_RETRIES
            )
        }))
    }
}

#[cfg(test)]
#[path = "invoice_watcher_tests.rs"]
mod tests;
