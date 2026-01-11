use anyhow::anyhow;
use bdk_wallet::bitcoin::{
    consensus::encode::deserialize,
    hashes::{sha256, Hash},
    PublicKey, Transaction,
};
use dlctix::{bitcoin::hex::DisplayHex, hashlock};
use log::{debug, error, info, warn};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::Coordinator,
    infra::{
        escrow::generate_escrow_tx,
        lightning::{InvoiceState, Ln},
    },
};

const MAX_BROADCAST_RETRIES: u32 = 3;
const MAX_ESCROW_REGENERATION_RETRIES: u32 = 2;
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
                        info!("Invoice accepted for ticket {}", ticket.id);

                        debug!("Marking ticket as Paid {}: ", ticket.id);

                        match self
                            .coordinator
                            .competition_store
                            .mark_ticket_paid(&ticket.hash, ticket.competition_id)
                            .await
                        {
                            Ok(_) => {
                                // Check if escrow is enabled
                                if self.coordinator.is_escrow_enabled() {
                                    // Try broadcasting escrow with retries and UTXO regeneration
                                    let broadcast_result =
                                        self.broadcast_escrow_with_utxo_retries(&ticket).await;

                                    match broadcast_result {
                                        Ok(txid) => {
                                            info!("Successfully broadcasted escrow transaction for ticket {} in competition {}: {}",
                                                ticket.id, ticket.competition_id, txid);

                                            // Proceed to settle the HODL invoice
                                            self.settle_invoice_and_mark_ticket(&ticket).await;
                                        }
                                        Err(e) => {
                                            // All broadcast attempts failed, cancel the HODL invoice and reset ticket
                                            error!("Failed to broadcast escrow transaction after all retry attempts for ticket {}: {}",
                                                ticket.id, e);

                                            match self
                                                .cancel_invoice_and_reset_ticket(&ticket)
                                                .await
                                            {
                                                Ok(_) => {
                                                    info!("Successfully cancelled invoice and reset ticket {} for reuse", ticket.id);
                                                }
                                                Err(e) => {
                                                    error!("Failed to cancel invoice and reset ticket {}: {}", ticket.id, e);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    // Escrow disabled - just settle the invoice directly
                                    // The HODL invoice stays in-flight until contract tx is broadcast
                                    info!(
                                        "Escrow disabled, ticket {} marked as paid (invoice stays in-flight)",
                                        ticket.id
                                    );
                                    // Note: We don't settle the invoice here - it stays in-flight
                                    // until the contract/funding tx is broadcast later
                                }
                            }
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

    async fn settle_invoice_and_mark_ticket(&self, ticket: &crate::domain::competitions::Ticket) {
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

    async fn cancel_invoice_and_reset_ticket(
        &self,
        ticket: &crate::domain::competitions::Ticket,
    ) -> Result<(), anyhow::Error> {
        // First cancel the HODL invoice
        match self.ln.cancel_hold_invoice(ticket.hash.clone()).await {
            Ok(_) => {
                info!(
                    "Successfully cancelled HODL invoice for ticket {}",
                    ticket.id
                );
            }
            Err(e) => {
                error!(
                    "Failed to cancel HODL invoice for ticket {}: {}",
                    ticket.id, e
                );
                // Continue anyway to reset the ticket
            }
        }

        let ticket_preimage = hashlock::preimage_random(&mut rand::rng());
        let payment_hash = sha256::Hash::hash(&ticket_preimage).to_byte_array();

        // Reset the ticket with new payment details
        self.coordinator
            .competition_store
            .reset_ticket_after_failed_escrow(
                ticket.id,
                &ticket_preimage.to_lower_hex_string(),
                &payment_hash.to_lower_hex_string(),
            )
            .await
            .map_err(|e| anyhow!("Failed to reset ticket {}: {}", ticket.id, e))?;

        info!(
            "Successfully reset ticket {} with new payment details after escrow broadcast failure",
            ticket.id
        );

        Ok(())
    }

    async fn broadcast_escrow_with_utxo_retries(
        &self,
        ticket: &crate::domain::competitions::Ticket,
    ) -> Result<String, anyhow::Error> {
        // First, try using the existing escrow transaction if available
        if let Some(escrow_transaction_hex) = &ticket.escrow_transaction {
            debug!(
                "Attempting to broadcast existing escrow transaction for ticket {}",
                ticket.id
            );

            match hex::decode(escrow_transaction_hex) {
                Ok(transaction_bytes) => {
                    match deserialize::<Transaction>(&transaction_bytes) {
                        Ok(transaction) => {
                            // Try broadcasting the existing transaction
                            match self.broadcast_with_retries(&transaction, ticket.id).await {
                                Ok(_) => {
                                    info!("Successfully broadcasted existing escrow transaction for ticket {}", ticket.id);
                                    return Ok(transaction.compute_txid().to_string());
                                }
                                Err(e) => {
                                    warn!("Failed to broadcast existing escrow transaction for ticket {}, will try regenerating: {}", ticket.id, e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to deserialize existing escrow transaction for ticket {}: {}", ticket.id, e);
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to decode existing escrow transaction hex for ticket {}: {}",
                        ticket.id, e
                    );
                }
            }
        }

        // Get competition details to regenerate escrow transaction
        let competition = self
            .coordinator
            .competition_store
            .get_competition(ticket.competition_id)
            .await
            .map_err(|e| anyhow!("Failed to get competition {}: {}", ticket.competition_id, e))?;

        // Get user pubkey from ticket reservation
        let user_pubkey_str = ticket
            .reserved_by
            .as_ref()
            .ok_or_else(|| anyhow!("Ticket {} has no reserved_by field", ticket.id))?;

        let user_pubkey = PublicKey::from_str(user_pubkey_str)
            .map_err(|e| anyhow!("Failed to parse user public key {}: {}", user_pubkey_str, e))?;

        // Decode payment hash from ticket
        let preimage = hex::decode(&ticket.encrypted_preimage)
            .map_err(|e| anyhow!("Failed to decode preimage for ticket {}: {}", ticket.id, e))?;
        let payment_hash = sha256::Hash::hash(&preimage).to_byte_array();
        let entry_fee = competition.event_submission.entry_fee as u64;

        // Try regenerating escrow transaction multiple times with different UTXOs
        for attempt in 1..=MAX_ESCROW_REGENERATION_RETRIES {
            info!(
                "Regenerating escrow transaction for ticket {} (attempt {}/{})",
                ticket.id, attempt, MAX_ESCROW_REGENERATION_RETRIES
            );

            // Sync wallet to get latest UTXO state
            if let Err(e) = self.coordinator.bitcoin.sync().await {
                warn!("Failed to sync wallet before escrow regeneration: {}", e);
            }

            match generate_escrow_tx(
                self.coordinator.bitcoin.clone(),
                ticket.id,
                user_pubkey,
                payment_hash,
                entry_fee,
            )
            .await
            {
                Ok(new_transaction) => {
                    info!(
                        "Successfully regenerated escrow transaction for ticket {} (attempt {})",
                        ticket.id, attempt
                    );

                    // Try broadcasting the new transaction
                    match self
                        .broadcast_with_retries(&new_transaction, ticket.id)
                        .await
                    {
                        Ok(_) => {
                            info!("Successfully broadcasted regenerated escrow transaction for ticket {}", ticket.id);

                            // Update the ticket with the new escrow transaction
                            let new_escrow_hex = hex::encode(
                                bdk_wallet::bitcoin::consensus::encode::serialize(&new_transaction),
                            );
                            if let Err(e) = self
                                .coordinator
                                .competition_store
                                .update_ticket_escrow_transaction(ticket.id, &new_escrow_hex)
                                .await
                            {
                                warn!(
                                    "Failed to update ticket {} with new escrow transaction: {}",
                                    ticket.id, e
                                );
                            }

                            return Ok(new_transaction.compute_txid().to_string());
                        }
                        Err(e) => {
                            warn!("Failed to broadcast regenerated escrow transaction for ticket {} (attempt {}): {}",
                                  ticket.id, attempt, e);

                            if attempt < MAX_ESCROW_REGENERATION_RETRIES {
                                // Wait before trying again to allow UTXO state to potentially change
                                let delay =
                                    Duration::from_millis(RETRY_DELAY_MS * 2 * attempt as u64);
                                sleep(delay).await;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to regenerate escrow transaction for ticket {} (attempt {}): {}",
                        ticket.id, attempt, e
                    );

                    if attempt < MAX_ESCROW_REGENERATION_RETRIES {
                        let delay = Duration::from_millis(RETRY_DELAY_MS * attempt as u64);
                        sleep(delay).await;
                    }
                }
            }
        }

        Err(anyhow!(
            "Failed to broadcast escrow transaction after {} regeneration attempts",
            MAX_ESCROW_REGENERATION_RETRIES
        ))
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
