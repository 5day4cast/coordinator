use log::{debug, error, info, warn};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{competitions::PayoutError, Coordinator, PaymentStatus},
    infra::lightning::{Ln, PaymentUpdate},
};

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
        info!("Starting payment subscriber");

        loop {
            if self.cancel_token.is_cancelled() {
                break;
            }

            if let Err(e) = self.run_subscription().await {
                error!("Payment subscription error: {}", e);
            }

            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {}
                _ = self.cancel_token.cancelled() => break,
            }
        }

        info!("Payment subscriber stopped");
        Ok(())
    }

    async fn run_subscription(&self) -> Result<(), anyhow::Error> {
        let mut rx = self.ln.subscribe_payments().await?;
        info!("Payment subscription connected");

        loop {
            tokio::select! {
                update = rx.recv() => {
                    let Some(update) = update else {
                        break;
                    };
                    self.handle_payment_update(update).await;
                }
                _ = self.cancel_token.cancelled() => break,
            }
        }

        Ok(())
    }

    async fn handle_payment_update(&self, update: PaymentUpdate) {
        if !matches!(
            update.status,
            PaymentStatus::Succeeded | PaymentStatus::Failed
        ) {
            return;
        }

        let payout = match self
            .coordinator
            .competition_store
            .get_payout_by_payment_hash(&update.payment_hash)
            .await
        {
            Ok(Some(payout)) => payout,
            Ok(None) => {
                debug!("No payout for hash {}", update.payment_hash);
                return;
            }
            Err(e) => {
                warn!("Error looking up payout for {}: {}", update.payment_hash, e);
                return;
            }
        };

        match update.status {
            PaymentStatus::Succeeded => {
                info!("Payment succeeded for payout {} (subscription)", payout.id);
                if let Err(e) = self
                    .coordinator
                    .competition_store
                    .mark_payout_succeeded(payout.id, OffsetDateTime::now_utc())
                    .await
                {
                    error!("Failed to mark payout {} as succeeded: {}", payout.id, e);
                }
            }
            PaymentStatus::Failed => {
                let error_msg = update
                    .failure_reason
                    .unwrap_or_else(|| "Unknown".to_string());
                warn!("Payment failed for payout {}: {}", payout.id, error_msg);

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
                }
            }
            _ => {}
        }
    }
}
