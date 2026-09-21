use log::{debug, error, info, warn};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{competitions::PayoutError, CompetitionStore, Coordinator, PaymentStatus},
    infra::{
        bitcoin::{Bitcoin, PayoutOutputStatus},
        lightning::{
            extract_payment_hash_from_invoice, invoice_is_expired, payout_cltv_limit,
            payout_htlc_expiry_height, Ln, PaymentDeadline, PaymentNotFound,
        },
    },
};

pub struct PayoutWatcher {
    competition_store: Arc<CompetitionStore>,
    ln: Arc<dyn Ln>,
    bitcoin: Arc<dyn Bitcoin>,
    leases: Arc<crate::domain::WorkerLeases>,
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
            bitcoin: coordinator.bitcoin.clone(),
            leases: coordinator.worker_leases().clone(),
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

            // Payments to winners run in one coordinator at a time.
            tokio::select! {
                result = self
                    .leases
                    .tick("payout-watcher", self.handle_pending_payouts()) => match result {
                    Some(Ok(_)) => debug!("Payout handling completed successfully"),
                    Some(Err(e)) => error!("Payout handling error: {}", e),
                    None => debug!("Another coordinator watches payouts"),
                },
                _ = self.cancel_token.cancelled() => break,
            }

            tokio::select! {
                _ = sleep(self.sync_interval) => continue,
                _ = self.cancel_token.cancelled() => {
                    info!("Payout watcher cancelled during sleep");
                    break;
                }
            }
        }

        self.leases.release("payout-watcher").await;
        Ok(())
    }

    async fn send_eligibility(
        &self,
        entry_id: uuid::Uuid,
        invoice: &lightning_invoice::Bolt11Invoice,
    ) -> Result<SendEligibility, anyhow::Error> {
        let entry = self
            .competition_store
            .get_entry_by_id(entry_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Payout entry no longer exists"))?;
        let competition = self
            .competition_store
            .get_competition(entry.event_id)
            .await?;
        if competition.delta_broadcasted_at.is_some()
            || competition.completed_at.is_some()
            || competition.failed_at.is_some()
            || competition.cancelled_at.is_some()
            || entry.sellback_broadcasted_at.is_some()
            || entry.reclaimed_broadcasted_at.is_some()
        {
            return Ok(SendEligibility::Closed(
                "Competition has entered on-chain resolution".into(),
            ));
        }
        let Some(outcome) = competition.outcome_transaction.as_ref() else {
            return Ok(SendEligibility::Deferred("Outcome is not available".into()));
        };
        let params = competition
            .contract_parameters
            .as_ref()
            .or_else(|| {
                competition
                    .signed_contract
                    .as_ref()
                    .map(|signed| signed.params())
            })
            .ok_or_else(|| anyhow::anyhow!("Payout has no persisted contract parameters"))?;
        let [output] = outcome.output.as_slice() else {
            return Err(anyhow::anyhow!("DLC outcome must have exactly one output"));
        };
        let status = tokio::time::timeout(
            Duration::from_secs(20),
            self.bitcoin.payout_output_status(
                bitcoin::OutPoint {
                    txid: outcome.compute_txid(),
                    vout: 0,
                },
                output.clone(),
            ),
        )
        .await??;
        Ok(classify_output(
            status,
            params.relative_locktime_block_delta,
            invoice.min_final_cltv_expiry_delta(),
        ))
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
                                .mark_payout_succeeded(
                                    payout.id,
                                    OffsetDateTime::now_utc(),
                                    payment.payment_preimage.clone(),
                                )
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
                    // A queued invoice can expire while the process is offline.
                    // LND confirms that no payment exists before we release it.
                    let invoice = payout
                        .payout_payment_request
                        .parse::<lightning_invoice::Bolt11Invoice>()
                        .map_err(|error| {
                            anyhow::anyhow!("Invalid stored payout invoice: {error}")
                        })?;
                    if invoice_is_expired(&invoice)
                        || !self
                            .competition_store
                            .payout_send_allowed(payout.id)
                            .await?
                    {
                        self.competition_store
                            .mark_payout_failed(
                                payout.id,
                                OffsetDateTime::now_utc(),
                                PayoutError::FailedToPayOut(
                                    "Invoice expired or on-chain settlement began before payment initiation".into(),
                                ),
                            )
                            .await?;
                        continue;
                    }
                    let deadline = match self.send_eligibility(payout.entry_id, &invoice).await {
                        Ok(SendEligibility::Ready(deadline)) => deadline,
                        Ok(SendEligibility::Closed(reason)) => {
                            // NotFound was established above; accepted/in-flight payments never enter this branch.
                            self.competition_store
                                .mark_payout_failed(
                                    payout.id,
                                    OffsetDateTime::now_utc(),
                                    PayoutError::FailedToPayOut(reason),
                                )
                                .await?;
                            continue;
                        }
                        Ok(SendEligibility::Deferred(reason)) => {
                            debug!(
                                "Payout {} waits for safe chain state: {}",
                                payout.id, reason
                            );
                            continue;
                        }
                        Err(error) => {
                            warn!("Payout {} chain state unavailable: {}", payout.id, error);
                            continue;
                        }
                    };
                    // Recover a crash between persisting the payout and sending it.
                    // Keep using the same invoice/hash: LND deduplicates attempts
                    // if the original request is accepted concurrently.
                    if let Err(error) = self
                        .ln
                        .send_payment_before_height(
                            payout.payout_payment_request,
                            payout.payout_amount_sats,
                            60,
                            1000,
                            deadline,
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

#[derive(Debug)]
enum SendEligibility {
    Ready(PaymentDeadline),
    Deferred(String),
    Closed(String),
}
fn classify_output(status: PayoutOutputStatus, delta: u16, final_cltv: u64) -> SendEligibility {
    let Some(confirmation_height) = status.confirmation_height else {
        return SendEligibility::Deferred("Outcome is not confirmed".into());
    };
    if confirmation_height > status.current_height {
        return SendEligibility::Deferred("Chain observations disagree".into());
    }
    if !status.unspent {
        return SendEligibility::Closed("DLC outcome output has already been spent".into());
    }
    let expiry = match payout_htlc_expiry_height(confirmation_height, delta) {
        Ok(value) => value,
        Err(error) => return SendEligibility::Closed(error.to_string()),
    };
    let deadline = PaymentDeadline {
        max_htlc_expiry_height: expiry,
        minimum_chain_height: status.current_height,
    };
    match payout_cltv_limit(deadline, status.current_height, final_cltv) {
        Ok(_) => SendEligibility::Ready(deadline),
        Err(error) => SendEligibility::Closed(error.to_string()),
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

    #[test]
    fn new_payments_require_confirmed_unspent_outcome_and_enough_cltv() {
        let confirmed = PayoutOutputStatus {
            confirmation_height: Some(100),
            current_height: 103,
            unspent: true,
        };
        assert!(matches!(
            classify_output(confirmed, 72, 40),
            SendEligibility::Ready(_)
        ));
        assert!(matches!(
            classify_output(
                PayoutOutputStatus {
                    confirmation_height: None,
                    ..confirmed
                },
                72,
                40
            ),
            SendEligibility::Deferred(_)
        ));
        assert!(matches!(
            classify_output(
                PayoutOutputStatus {
                    current_height: 99,
                    ..confirmed
                },
                72,
                40
            ),
            SendEligibility::Deferred(_)
        ));
        assert!(matches!(
            classify_output(
                PayoutOutputStatus {
                    unspent: false,
                    ..confirmed
                },
                72,
                40
            ),
            SendEligibility::Closed(_)
        ));
        assert!(matches!(
            classify_output(
                PayoutOutputStatus {
                    current_height: 117,
                    ..confirmed
                },
                72,
                40
            ),
            SendEligibility::Closed(_)
        ));
        assert!(matches!(
            classify_output(confirmed, 1, 18),
            SendEligibility::Closed(_)
        ));
    }

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
        let entry_submission=serde_json::json!({"id":entry_id,"ticket_id":ticket_id,"ephemeral_pubkey":"key","payout_hash":"hash","event_id":event_id,"expected_observations":[]}).to_string();
        let event_submission=serde_json::json!({"id":event_id,"signing_date":"2030-01-01T00:00:00Z","start_observation_date":"2030-01-02T00:00:00Z","end_observation_date":"2030-01-03T00:00:00Z","locations":["KORD"],"number_of_values_per_entry":1,"number_of_places_win":1,"total_allowed_entries":2,"entry_fee":10,"coordinator_fee_percentage":0,"total_competition_pool":20,"relative_locktime_block_delta":72}).to_string();
        let scalar = |n| {
            dlctix::secp::Scalar::from_slice(&[n; 32])
                .unwrap()
                .base_point_mul()
        };
        let params = dlctix::ContractParameters {
            market_maker: dlctix::MarketMaker { pubkey: scalar(1) },
            players: vec![dlctix::Player {
                pubkey: scalar(2),
                ticket_hash: [3; 32],
                payout_hash: [4; 32],
            }],
            event: dlctix::EventLockingConditions {
                locking_points: vec![scalar(5).into()],
                expiry: Some(500_000),
            },
            outcome_payouts: std::collections::BTreeMap::from([(
                dlctix::Outcome::Attestation(0),
                std::collections::BTreeMap::from([(0, 100)]),
            )]),
            funding_value: bitcoin::Amount::from_sat(100_000),
            fee_rate: bitcoin::FeeRate::from_sat_per_vb_u32(1),
            relative_locktime_block_delta: 72,
        };
        let params = serde_json::to_vec(&params).unwrap();
        let outcome = serde_json::to_vec(&bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(100_000),
                script_pubkey: bitcoin::ScriptBuf::new(),
            }],
        })
        .unwrap();
        database.execute_write(move |pool| async move {
            sqlx::query("INSERT INTO competitions (id, created_at, event_submission, contract_parameters, outcome_transaction) VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?, ?, ?)")
                .bind(event_id.to_string()).bind(event_submission).bind(params).bind(outcome).execute(&pool).await?;
            sqlx::query("INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, 'preimage', 'hash')")
                .bind(ticket_id.to_string()).bind(event_id.to_string()).execute(&pool).await?;
            sqlx::query("INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey, payout_hash, entry_submission) VALUES (?, ?, ?, 'owner', 'key', 'hash', ?)")
                .bind(entry_id.to_string()).bind(event_id.to_string()).bind(ticket_id.to_string()).bind(entry_submission).execute(&pool).await?;
            Ok(())
        }).await.unwrap();
        let invoice = |timestamp, hash| {
            InvoiceBuilder::new(Currency::Regtest)
                .description("payout".into())
                .payment_hash(sha256::Hash::from_byte_array([hash; 32]))
                .payment_secret(PaymentSecret([8; 32]))
                .amount_milli_satoshis(10_000)
                .duration_since_epoch(timestamp)
                .min_final_cltv_expiry_delta(18)
                .build_signed(|hash| {
                    Secp256k1::new()
                        .sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[9; 32]).unwrap())
                })
                .unwrap()
                .to_string()
        };
        // This queued invoice expired while the process was stopped.
        let expired_id = store
            .store_payout_info_pending(
                entry_id,
                "preimage".into(),
                "private".into(),
                invoice(Duration::from_secs(1), 6),
                10,
            )
            .await
            .unwrap();
        let invoice = invoice(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap(),
            7,
        );
        let sends = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/v1/getinfo", get(||async{Json(serde_json::json!({"synced_to_chain":true,"block_height":100}))}))
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
            leases: Arc::new(crate::domain::WorkerLeases::new(
                Arc::new(store.clone()),
                "test".into(),
                Duration::from_secs(30),
            )),
            competition_store: Arc::new(store.clone()),
            bitcoin: Arc::new(crate::infra::bitcoin_mock::MockBitcoinClient::new(
                bitcoin::Network::Regtest,
            )),
            ln: Arc::new(LnClient {
                base_url: reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
                client: reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build(),
                payment_client: reqwest::Client::new(),
                macaroon: secrecy::SecretString::from("test-macaroon"),
            }),
            sync_interval: Duration::from_secs(1),
            cancel_token: CancellationToken::new(),
        };
        tokio::time::timeout(Duration::from_secs(5), watcher.handle_pending_payouts())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 0);
        assert!(store
            .get_payout(expired_id)
            .await
            .unwrap()
            .unwrap()
            .failed_at
            .is_some());
        // Simulate another stop after persisting a fresh invoice, before its RPC.
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
        // Closing the on-chain window must not prevent reconciliation of a
        // payment LND already accepted before the HTTP response was lost.
        database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE competitions SET delta_broadcasted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
                )
                .bind(event_id.to_string())
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
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
