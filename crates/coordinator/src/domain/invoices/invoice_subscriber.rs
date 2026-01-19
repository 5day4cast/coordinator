use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::Coordinator,
    infra::lightning::{InvoiceState, InvoiceUpdate, Ln},
};

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
        info!("Starting invoice subscriber");

        loop {
            if self.cancel_token.is_cancelled() {
                break;
            }

            if let Err(e) = self.run_subscription().await {
                error!("Invoice subscription error: {}", e);
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
        info!("Invoice subscription connected");

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

        if let Err(e) = self
            .coordinator
            .competition_store
            .mark_ticket_paid(&ticket.hash, ticket.competition_id)
            .await
        {
            error!("Failed to mark ticket {} as paid: {}", ticket.id, e);
        }
    }
}
