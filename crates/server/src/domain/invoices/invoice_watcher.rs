use anyhow::anyhow;
use bdk_wallet::bitcoin::{consensus::encode::deserialize, Transaction};
use log::{debug, error, info};
use std::{sync::Arc, time::Duration};
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

            match self.handle_pending_invoices().await {
                Ok(_) => {
                    debug!("Invoice handling completed successfully");
                }
                Err(e) => {
                    error!("Invoice handling error: {}", e);
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
                    if invoice.state == InvoiceState::Accepted {
                        info!("Invoice accepted for ticket {}, creating escrow", ticket.id);

                        let Some(escrow_transaction) = ticket.escrow_transaction else {
                            error!("Ticket {} has no escrow transaction", ticket.id);
                            continue;
                        };
                        let transaction = hex::decode(escrow_transaction.clone())
                            .map_err(|e| anyhow!("Failed to decode escrow transaction: {}", e))?;
                        let transaction: Transaction = deserialize(&transaction).map_err(|e| {
                            anyhow!("Failed to deserialize escrow transaction: {}", e)
                        })?;

                        debug!("Marking ticket as Paid {}: ", ticket.id);

                        match self
                            .coordinator
                            .competition_store
                            .mark_ticket_paid(&ticket.hash, ticket.competition_id)
                            .await
                        {
                            //TODO(@tee8z): add the ability to retry broadcasting the transaction
                            Ok(_) => match self.coordinator.bitcoin.broadcast(&transaction).await {
                                Ok(_) => {
                                    info!("Broadcasted escrow transaction {} for ticket {} in competition {}", escrow_transaction, ticket.id, ticket.competition_id);
                                }
                                Err(e) => error!(
                                    "Failed to broadcast escrow transaction for ticket {}: {}",
                                    ticket.id, e
                                ),
                            },
                            Err(e) => error!(
                                "Failed to create escrow transaction for ticket {}: {}",
                                ticket.id, e
                            ),
                        }

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
                                    Err(e) => error!(
                                        "Failed to mark ticket {} as settled: {}",
                                        ticket.id, e
                                    ),
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
                }
                Err(e) => {
                    debug!("Failed to lookup invoice for ticket {}: {}", ticket.id, e);
                }
            }
        }

        Ok(())
    }
}
