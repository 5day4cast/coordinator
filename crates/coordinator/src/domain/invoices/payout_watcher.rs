use log::{debug, error, info, warn};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{competitions::PayoutError, CompetitionStore, Coordinator, PaymentStatus},
    infra::lightning::{extract_payment_hash_from_invoice, Ln, PaymentNotFound},
};

pub struct PayoutWatcher {
    competition_store: CompetitionStore,
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
            competition_store: coordinator.competition_store.clone(),
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
        let pending_payouts = self.competition_store.get_all_pending_payouts().await?;

        debug!("Checking {} pending payouts", pending_payouts.len());

        for payout in pending_payouts {
            let payment_hash =
                match extract_payment_hash_from_invoice(&payout.payout_payment_request) {
                    Ok(hash) => hash,
                    Err(e) => {
                        error!("Invalid lightning invoice for payout {}: {}", payout.id, e);

                        // Mark payout as failed due to invalid invoice
                        if let Err(mark_err) = self
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
                Err(e) if e.is::<PaymentNotFound>() => {
                    // Recover a crash between persisting the payout and sending it.
                    // Keep using the same invoice/hash: LND deduplicates attempts
                    // if the original request is accepted concurrently.
                    if let Err(error) = self
                        .ln
                        .send_payment(
                            payout.payout_payment_request,
                            payout.payout_amount_sats,
                            60,
                            1000,
                        )
                        .await
                    {
                        warn!(
                            "Payout {} remains pending after send error: {}",
                            payout.id, error
                        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::{
        db::{DBConnection, DatabasePoolConfig, DatabaseType},
        lightning::LnClient,
    };
    use axum::{
        extract::State,
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use bitcoin::{
        hashes::{sha256, Hash},
        secp256k1::{Secp256k1, SecretKey},
    };
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    #[tokio::test]
    async fn payout_outbox_recovers_an_unsent_payment_and_keeps_ambiguous_errors_locked() {
        let directory = tempfile::tempdir().unwrap();
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let store = CompetitionStore::new(database.clone());
        let event_id = Uuid::now_v7();
        let ticket_id = Uuid::now_v7();
        let entry_id = Uuid::now_v7();
        database.execute_write(move |pool| async move {
            sqlx::query("INSERT INTO competitions (id, created_at, event_submission) VALUES (?, datetime('now'), '{}')")
                .bind(event_id.to_string()).execute(&pool).await?;
            sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'preimage', 'hash')")
                .bind(ticket_id.to_string()).bind(event_id.to_string()).execute(&pool).await?;
            sqlx::query("INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey, ephemeral_privatekey_encrypted, payout_preimage_encrypted, payout_hash, entry_submission) VALUES (?, ?, ?, 'owner', 'key', '', '', 'hash', '{}')")
                .bind(entry_id.to_string()).bind(event_id.to_string()).bind(ticket_id.to_string()).execute(&pool).await?;
            Ok(())
        }).await.unwrap();
        let invoice = InvoiceBuilder::new(Currency::Regtest)
            .description("payout".into())
            .payment_hash(sha256::Hash::from_byte_array([7; 32]))
            .payment_secret(PaymentSecret([8; 32]))
            .amount_milli_satoshis(10_000)
            .current_timestamp()
            .min_final_cltv_expiry_delta(18)
            .build_signed(|hash| {
                Secp256k1::new()
                    .sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[9; 32]).unwrap())
            })
            .unwrap()
            .to_string();
        // Simulate a process stopping after committing the payout, before the RPC.
        let payout_id = store
            .store_payout_info_pending(
                entry_id,
                "preimage".into(),
                "private".into(),
                invoice.clone(),
                10,
            )
            .await
            .unwrap();
        let sends = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/v2/router/track/{hash}", get(|State(sends): State<Arc<AtomicUsize>>| async move {
                if sends.load(Ordering::SeqCst) == 0 {
                    return reqwest::StatusCode::NOT_FOUND.into_response();
                }
                Json(serde_json::json!({"result": {
                    "payment_hash": hex::encode([7; 32]), "status": "SUCCEEDED",
                    "value": "10", "creation_date": "0", "fee": "0",
                    "value_sat": "10", "value_msat": "10000", "payment_request": "invoice",
                    "fee_sat": "0", "fee_msat": "0", "creation_time_ns": "0", "failure_reason": "FAILURE_REASON_NONE"
                }})).into_response()
            }))
            .route("/v2/router/send", post(|State(sends): State<Arc<AtomicUsize>>| async move {
                sends.fetch_add(1, Ordering::SeqCst);
                // LND paid, but the proxy lost its successful response.
                reqwest::StatusCode::BAD_GATEWAY
            }))
            .with_state(sends.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let watcher = PayoutWatcher {
            competition_store: store.clone(),
            ln: Arc::new(LnClient {
                base_url: reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
                client: reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build(),
                macaroon: secrecy::SecretString::from("test-macaroon"),
            }),
            sync_interval: Duration::from_secs(1),
            cancel_token: CancellationToken::new(),
        };
        tokio::time::timeout(Duration::from_secs(5), watcher.handle_pending_payouts())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert_eq!(store.get_all_pending_payouts().await.unwrap().len(), 1);
        assert!(store
            .store_payout_info_pending(entry_id, "preimage".into(), "private".into(), invoice, 10)
            .await
            .is_err());
        tokio::time::timeout(Duration::from_secs(5), watcher.handle_pending_payouts())
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .get_payout(payout_id)
            .await
            .unwrap()
            .unwrap()
            .succeed_at
            .is_some());
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        server.abort();
        database.close().await.unwrap();
    }
}
