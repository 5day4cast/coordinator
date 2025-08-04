use anyhow::anyhow;
use log::{debug, error, info, warn};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{domain::PaymentStatus, Coordinator, Ln};

const MAX_PAYMENT_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 2000;

pub struct PayoutWatcher {
    coordinator: Arc<Coordinator>,
    ln: Arc<dyn Ln>,
    sync_interval: Duration,
    cancel_token: CancellationToken,
}

impl PayoutWatcher {
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
        info!("Starting Payout watcher");

        loop {
            if self.cancel_token.is_cancelled() {
                info!("Payout watcher received cancellation");
                break;
            }

            match self.handle_pending_payouts().await {
                Ok(_) => {
                    debug!("Payout handling completed successfully");
                }
                Err(e) => {
                    error!("Payout handling error: {}", e);
                }
            }

            tokio::select! {
                _ = sleep(self.sync_interval) => continue,
                _ = self.cancel_token.cancelled() => {
                    info!("Payout watcher cancelled during sleep");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn handle_pending_payouts(&self) -> Result<(), anyhow::Error> {
        let pending_payouts = self
            .coordinator
            .competition_store
            .get_pending_payouts()
            .await?;

        debug!("Checking {} pending payouts", pending_payouts.len());

        for payout in pending_payouts {
            let payment_hash =
                match crate::ln_client::extract_payment_hash_from_invoice(&payout.ln_invoice) {
                    Ok(hash) => hash,
                    Err(e) => {
                        error!("Invalid lightning invoice, {}: {}", payout.entry_id, e);
                        continue;
                    }
                };

            match self.ln.lookup_payment(&payout.payment_hash).await {
                Ok(payment) => {
                    debug!(
                        "Payout {}: payment status: {:?}",
                        payout.entry_id, payment.status
                    );

                    match payment.status {
                        PaymentStatus::Succeeded => {
                            info!(
                                "Payment succeeded for payout {}, marking as paid out",
                                payout.entry_id
                            );

                            match self
                                .coordinator
                                .competition_store
                                .mark_entry_paid_out(payout.entry_id, OffsetDateTime::now_utc())
                                .await
                            {
                                Ok(_) => {
                                    info!(
                                        "Successfully marked entry {} as paid out",
                                        payout.entry_id
                                    );
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to mark entry {} as paid out: {}",
                                        payout.entry_id, e
                                    );
                                }
                            }
                        }
                        PaymentStatus::Failed => {
                            let error_msg = payment
                                .payment_error
                                .unwrap_or_else(|| "Payment failed".to_string());

                            warn!(
                                "Payment failed for payout {}: {}. Will resolve via onchain transaction.",
                                payout.entry_id, error_msg
                            );

                            info!(
                                "Payout {} will be resolved via onchain sellback or reclaim transaction",
                                payout.entry_id
                            );
                        }
                        PaymentStatus::InFlight => {
                            debug!("Payment still in flight for payout {}", payout.entry_id);
                        }
                        PaymentStatus::Initiated => {
                            debug!("Payment initiated for payout {}", payout.entry_id);
                        }
                        PaymentStatus::Unknown => {
                            warn!("Payment status unknown for payout {}", payout.entry_id);
                        }
                    }
                }
                Err(e) => {
                    debug!(
                        "Failed to lookup payment for payout {}: {}",
                        payout.entry_id, e
                    );
                }
            }
        }

        Ok(())
    }
}

// Data structure for pending payouts
#[derive(Debug, Clone)]
pub struct PendingPayout {
    pub entry_id: uuid::Uuid,
    pub payment_hash: String,
    pub ln_invoice: String,
    pub amount_sats: u64,
    pub retry_count: u32,
    pub initiated_at: OffsetDateTime,
}
