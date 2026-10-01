use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::SubscriptionHealth;
use crate::{
    domain::Coordinator,
    infra::lightning::{InvoiceState, InvoiceUpdate, Ln},
};

pub struct InvoiceSubscriber {
    coordinator: Arc<Coordinator>,
    ln: Arc<dyn Ln>,
    cancel_token: CancellationToken,
    health: Arc<SubscriptionHealth>,
}

impl InvoiceSubscriber {
    pub fn new(
        coordinator: Arc<Coordinator>,
        ln: Arc<dyn Ln>,
        cancel_token: CancellationToken,
        health: Arc<SubscriptionHealth>,
    ) -> Self {
        Self {
            coordinator,
            ln,
            cancel_token,
            health,
        }
    }

    pub async fn subscribe(&self) -> Result<(), anyhow::Error> {
        info!("Starting invoice subscriber");

        loop {
            if self.cancel_token.is_cancelled() {
                break;
            }

            if let Err(e) = self.run_subscription().await {
                error!("Invoice subscription error: {}", e);
            }
            if !self.cancel_token.is_cancelled() {
                self.health.set_down();
            }

            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {}
                _ = self.cancel_token.cancelled() => break,
            }
        }

        info!("Invoice subscriber stopped");
        Ok(())
    }

    async fn run_subscription(&self) -> Result<(), anyhow::Error> {
        let mut rx = self.ln.subscribe_invoices().await?;
        self.health.set_up();

        loop {
            tokio::select! {
                update = rx.recv() => {
                    let Some(update) = update else {
                        break;
                    };
                    self.handle_invoice_update(update).await;
                }
                _ = self.cancel_token.cancelled() => break,
            }
        }

        Ok(())
    }

    async fn handle_invoice_update(&self, update: InvoiceUpdate) {
        if update.state != InvoiceState::Accepted {
            return;
        }

        let ticket = match self
            .coordinator
            .competition_store
            .get_ticket_by_hash(&update.payment_hash)
            .await
        {
            Ok(Some(ticket)) => ticket,
            Ok(None) => {
                debug!("No ticket for hash {}", update.payment_hash);
                return;
            }
            Err(e) => {
                warn!("Error looking up ticket for {}: {}", update.payment_hash, e);
                return;
            }
        };

        info!("Invoice accepted for ticket {} (subscription)", ticket.id);

        match self
            .coordinator
            .competition_store
            .mark_ticket_paid(&ticket.hash, ticket.competition_id)
            .await
        {
            Ok(_) => {
                self.coordinator.wake_competition(ticket.competition_id);
                // The invoice watcher publishes the escrow and settles the invoice.
                self.health.wake();
            }
            Err(e) => error!("Failed to mark ticket {} as paid: {}", ticket.id, e),
        }
    }
}
