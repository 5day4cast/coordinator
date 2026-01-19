use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::Coordinator,
    infra::lightning::{InvoiceState, InvoiceUpdate, Ln},
};

/// InvoiceSubscriber listens to LND invoice updates via streaming subscription.
/// This provides faster payment detection than polling, with polling as a fallback.
pub struct InvoiceSubscriber {
    coordinator: Arc<Coordinator>,
    ln: Arc<dyn Ln>,
    cancel_token: CancellationToken,
}

impl InvoiceSubscriber {
    pub fn new(
        coordinator: Arc<Coordinator>,
        ln: Arc<dyn Ln>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            coordinator,
            ln,
            cancel_token,
        }
    }

    pub async fn subscribe(&self) -> Result<(), anyhow::Error> {
        info!("Starting Invoice subscriber");

        loop {
            if self.cancel_token.is_cancelled() {
                info!("Invoice subscriber received cancellation");
                break;
            }

            match self.run_subscription().await {
                Ok(_) => {
                    info!("Invoice subscription ended normally");
                }
                Err(e) => {
                    error!("Invoice subscription error: {}", e);
                }
            }

            // Check cancellation before reconnecting
            if self.cancel_token.is_cancelled() {
                break;
            }

            // Wait before reconnecting
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                    info!("Reconnecting invoice subscription...");
                }
                _ = self.cancel_token.cancelled() => {
                    info!("Invoice subscriber cancelled during reconnect wait");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn run_subscription(&self) -> Result<(), anyhow::Error> {
        let mut rx = self.ln.subscribe_invoices().await?;
        info!("Invoice subscription connected");

        loop {
            tokio::select! {
                update = rx.recv() => {
                    match update {
                        Some(invoice_update) => {
                            self.handle_invoice_update(invoice_update).await;
                        }
                        None => {
                            info!("Invoice subscription channel closed");
                            break;
                        }
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    info!("Invoice subscriber cancelled");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn handle_invoice_update(&self, update: InvoiceUpdate) {
        debug!(
            "Received invoice update: hash={}, state={:?}",
            update.payment_hash, update.state
        );

        // We only care about Accepted state for hold invoices (payment received, waiting to settle)
        if update.state != InvoiceState::Accepted {
            return;
        }

        // Look up the ticket by payment hash
        let ticket = match self
            .coordinator
            .competition_store
            .get_ticket_by_hash(&update.payment_hash)
            .await
        {
            Ok(Some(ticket)) => ticket,
            Ok(None) => {
                debug!(
                    "No ticket found for payment hash {}, ignoring",
                    update.payment_hash
                );
                return;
            }
            Err(e) => {
                warn!(
                    "Error looking up ticket for hash {}: {}",
                    update.payment_hash, e
                );
                return;
            }
        };

        info!("Invoice accepted for ticket {} (subscription)", ticket.id);

        // Mark ticket as paid
        match self
            .coordinator
            .competition_store
            .mark_ticket_paid(&ticket.hash, ticket.competition_id)
            .await
        {
            Ok(_) => {
                info!("Ticket {} marked as paid via subscription", ticket.id);

                // Handle escrow if enabled (same logic as polling watcher)
                if self.coordinator.is_escrow_enabled() {
                    // Escrow handling is complex and done by the polling watcher
                    // The subscription just provides faster initial detection
                    debug!(
                        "Escrow enabled - escrow broadcast will be handled by polling watcher for ticket {}",
                        ticket.id
                    );
                } else {
                    info!(
                        "Escrow disabled, ticket {} marked as paid (invoice stays in-flight)",
                        ticket.id
                    );
                }
            }
            Err(e) => {
                error!(
                    "Failed to mark ticket {} as paid via subscription: {}",
                    ticket.id, e
                );
            }
        }
    }
}
