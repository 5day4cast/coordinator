use log::{debug, error, info, warn};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{competitions::PayoutError, Coordinator, PaymentStatus},
    infra::lightning::{Ln, PaymentUpdate},
};

/// PaymentSubscriber listens to LND payment updates via streaming subscription.
/// This provides faster payout confirmation than polling, with polling as a fallback.
pub struct PaymentSubscriber {
    coordinator: Arc<Coordinator>,
    ln: Arc<dyn Ln>,
    cancel_token: CancellationToken,
}

impl PaymentSubscriber {
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
        info!("Starting Payment subscriber");

        loop {
            if self.cancel_token.is_cancelled() {
                info!("Payment subscriber received cancellation");
                break;
            }

            match self.run_subscription().await {
                Ok(_) => {
                    info!("Payment subscription ended normally");
                }
                Err(e) => {
                    error!("Payment subscription error: {}", e);
                }
            }

            // Check cancellation before reconnecting
            if self.cancel_token.is_cancelled() {
                break;
            }

            // Wait before reconnecting
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                    info!("Reconnecting payment subscription...");
                }
                _ = self.cancel_token.cancelled() => {
                    info!("Payment subscriber cancelled during reconnect wait");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn run_subscription(&self) -> Result<(), anyhow::Error> {
        let mut rx = self.ln.subscribe_payments().await?;
        info!("Payment subscription connected");

        loop {
            tokio::select! {
                update = rx.recv() => {
                    match update {
                        Some(payment_update) => {
                            self.handle_payment_update(payment_update).await;
                        }
                        None => {
                            info!("Payment subscription channel closed");
                            break;
                        }
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    info!("Payment subscriber cancelled");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn handle_payment_update(&self, update: PaymentUpdate) {
        debug!(
            "Received payment update: hash={}, status={:?}",
            update.payment_hash, update.status
        );

        // We only care about terminal states
        match update.status {
            PaymentStatus::Succeeded | PaymentStatus::Failed => {}
            _ => return,
        }

        // Look up the payout by payment hash
        let payout = match self
            .coordinator
            .competition_store
            .get_payout_by_payment_hash(&update.payment_hash)
            .await
        {
            Ok(Some(payout)) => payout,
            Ok(None) => {
                debug!(
                    "No payout found for payment hash {}, ignoring",
                    update.payment_hash
                );
                return;
            }
            Err(e) => {
                warn!(
                    "Error looking up payout for hash {}: {}",
                    update.payment_hash, e
                );
                return;
            }
        };

        match update.status {
            PaymentStatus::Succeeded => {
                info!("Payment succeeded for payout {} (subscription)", payout.id);

                match self
                    .coordinator
                    .competition_store
                    .mark_payout_succeeded(payout.id, OffsetDateTime::now_utc())
                    .await
                {
                    Ok(_) => {
                        info!("Payout {} marked as succeeded via subscription", payout.id);
                    }
                    Err(e) => {
                        error!("Failed to mark payout {} as succeeded: {}", payout.id, e);
                    }
                }
            }
            PaymentStatus::Failed => {
                let error_msg = update
                    .failure_reason
                    .unwrap_or_else(|| "Unknown failure".to_string());

                warn!(
                    "Payment failed for payout {} (entry {}): {}. Will resolve via onchain transaction.",
                    payout.id, payout.entry_id, error_msg
                );

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
            _ => {}
        }
    }
}
