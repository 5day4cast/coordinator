use std::{sync::Arc, time::Duration};

use log::{debug, error, info};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{Coordinator, InvoiceState, Ln};

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

            match self.check_pending_invoices().await {
                Ok(_) => {
                    debug!("Invoice check completed successfully");
                }
                Err(e) => {
                    error!("Invoice check error: {}", e);
                }
            }

            tokio::select! {
                _ = sleep(self.sync_interval) => continue,
                _ = self.cancel_token.cancelled() => {
                    info!("Invoice watcher cancelled during sleep");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn check_pending_invoices(&self) -> Result<(), anyhow::Error> {
        // Get all tickets that are reserved but not yet paid
        let pending_tickets = self
            .coordinator
            .competition_store
            .get_pending_tickets()
            .await?;
        info!("pending tickets: {:?}", pending_tickets);
        for ticket in pending_tickets {
            match self.ln.lookup_invoice(&ticket.hash).await {
                Ok(invoice) => {
                    info!("invoice: {:?}", invoice);
                    if invoice.state == InvoiceState::Accepted {
                        match self
                            .coordinator
                            .handle_invoice_accepted(ticket.competition_id, &ticket.hash)
                            .await
                        {
                            Ok(_) => debug!(
                                "Marked ticket {} as paid for competition {}",
                                ticket.id, ticket.competition_id
                            ),
                            Err(e) => error!("Failed to mark ticket {} as paid: {}", ticket.id, e),
                        }
                    }
                }
                Err(e) => {
                    debug!("Failed to lookup invoice for ticket {}: {}", ticket.id, e);
                }
            }
        }

        Ok(())
    }
}
