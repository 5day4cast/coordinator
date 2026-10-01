use log::{debug, error, info, warn};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use super::{payout_watcher::record_payment_failure, SubscriptionHealth};
use crate::{
    domain::{Coordinator, PaymentStatus},
    infra::lightning::{Ln, PaymentUpdate},
};

pub struct PaymentSubscriber {
    coordinator: Arc<Coordinator>,
    ln: Arc<dyn Ln>,
    cancel_token: CancellationToken,
    health: Arc<SubscriptionHealth>,
}

impl PaymentSubscriber {
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
        info!("Starting payment subscriber");

        loop {
            if self.cancel_token.is_cancelled() {
                break;
            }

            if let Err(e) = self.run_subscription().await {
                error!("Payment subscription error: {}", e);
            }
            if !self.cancel_token.is_cancelled() {
                self.health.set_down();
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
        self.health.set_up();

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
                    .mark_payout_succeeded(
                        payout.id,
                        OffsetDateTime::now_utc(),
                        update.preimage.clone(),
                    )
                    .await
                {
                    error!("Failed to mark payout {} as succeeded: {}", payout.id, e);
                }
            }
            PaymentStatus::Failed => {
                let reason = update
                    .failure_reason
                    .unwrap_or_else(|| "Unknown".to_string());
                // The payout watcher sends the invoice again when the failure can pass, and
                // records the same failure here at most once between the two.
                if let Err(e) = record_payment_failure(
                    &self.coordinator.competition_store,
                    self.coordinator.bitcoin.as_ref(),
                    &payout,
                    &reason,
                )
                .await
                {
                    error!(
                        "Failed to record the failed payment of payout {}: {}",
                        payout.id, e
                    );
                }
            }
            _ => {}
        }
    }
}
