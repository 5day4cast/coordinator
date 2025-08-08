use log::{debug, error, info, warn};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{competitions::PayoutError, PaymentStatus},
    Coordinator, Ln,
};

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
            .get_all_pending_payouts()
            .await?;

        debug!("Checking {} pending payouts", pending_payouts.len());

        for payout in pending_payouts {
            let payment_hash = match crate::ln_client::extract_payment_hash_from_invoice(
                &payout.payout_payment_request,
            ) {
                Ok(hash) => hash,
                Err(e) => {
                    error!("Invalid lightning invoice for payout {}: {}", payout.id, e);

                    // Mark payout as failed due to invalid invoice
                    if let Err(mark_err) = self
                        .coordinator
                        .competition_store
                        .mark_payout_failed(
                            payout.id,
                            OffsetDateTime::now_utc(),
                            PayoutError::FailedToPayOut(e.to_string()),
                        )
                        .await
                    {
                        error!(
                            "Failed to mark payout {} as failed: {}",
                            payout.id, mark_err
                        );
                    }
                    continue;
                }
            };

            match self.ln.lookup_payment(&payment_hash).await {
                Ok(payment) => {
                    debug!("Payout {}: payment status: {:?}", payout.id, payment.status);

                    match payment.status {
                        PaymentStatus::Succeeded => {
                            info!(
                                "Payment succeeded for payout {}, marking as succeeded",
                                payout.id
                            );

                            match self
                                .coordinator
                                .competition_store
                                .mark_payout_succeeded(payout.id, OffsetDateTime::now_utc())
                                .await
                            {
                                Ok(_) => {
                                    info!("Successfully marked payout {} as succeeded", payout.id);
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to mark payout {} as succeeded: {}",
                                        payout.id, e
                                    );
                                }
                            }
                        }
                        PaymentStatus::Failed => {
                            let error_msg = payment.failure_reason;

                            warn!(
                                "Payment failed for payout {} (entry {}): {}. Will resolve via onchain transaction.",
                                payout.id, payout.entry_id, error_msg
                            );

                            // Mark the payout as failed
                            if let Err(e) = self
                                .coordinator
                                .competition_store
                                .mark_payout_failed(
                                    payout.id,
                                    OffsetDateTime::now_utc(),
                                    PayoutError::FailedToPayOut(error_msg),
                                )
                                .await
                            {
                                error!("Failed to mark payout {} as failed: {}", payout.id, e);
                            } else {
                                info!(
                                    "Payout {} will be resolved via onchain sellback or reclaim transaction for entry {}",
                                    payout.id, payout.entry_id
                                );
                            }
                        }
                        PaymentStatus::InFlight => {
                            debug!("Payment still in flight for payout {}", payout.id);
                        }
                        PaymentStatus::Initiated => {
                            debug!("Payment initiated for payout {}", payout.id);
                        }
                        PaymentStatus::Unknown => {
                            warn!("Payment status unknown for payout {}", payout.id);
                        }
                    }
                }
                Err(e) => {
                    debug!("Failed to lookup payment for payout {}: {}", payout.id, e);
                }
            }
        }

        Ok(())
    }
}
