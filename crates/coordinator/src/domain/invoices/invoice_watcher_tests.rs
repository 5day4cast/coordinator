use super::*;
use crate::{
    config::KeymeldSettings,
    domain::{competitions::CreateEvent, Competition, CompetitionStore},
    infra::{
        bitcoin::{
            Bitcoin, ForeignUtxo, PayoutOutputStatus, SendOptions, WalletBalance, WalletUtxo,
        },
        db::{DBConnection, DatabasePoolConfig, DatabaseType},
        keymeld::KeymeldService,
        lightning_mock::MockLnClient,
        lnurl_mock::MockLnurlPay,
        oracle_mock::MockOracle,
    },
};
use bitcoin::{Address, Amount, FeeRate, Network, OutPoint, Psbt, ScriptBuf, TxOut, Txid};
use dlctix::secp::Scalar;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};
use tempfile::TempDir;
use uuid::Uuid;

mockall::mock! {
    Wallet {}
    #[async_trait::async_trait]
    impl Bitcoin for Wallet {

    fn get_network(&self) -> Network;
    /// Signs the wallet's inputs through LND and the escrow inputs with the
    /// coordinator's key. Returns whether every input carries a signature.
    async fn sign_psbt_with_escrow_support(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error>;
    /// Finalizes escrow inputs from their signatures, then lets LND sign and
    /// finalize its own inputs. Returns whether every input is final.
    async fn finalize_psbt_with_escrow_support(
        &self,
        psbt: &mut Psbt,
    ) -> Result<bool, anyhow::Error>;
    async fn build_psbt(
        &self,
        script_pubkey: ScriptBuf,
        amount: Amount,
        fee_rate: FeeRate,
        selected_utxos: Vec<OutPoint>,
        foreign_utxos: Vec<ForeignUtxo>,
    ) -> Result<Psbt, anyhow::Error>;
    /// Keep the wallet inputs owned by this packet reserved until the given
    /// Unix timestamp. Persist the packet before giving it to other signers.
    async fn reserve_psbt_inputs_until(
        &self,
        psbt: &Psbt,
        deadline: u64,
    ) -> Result<(), anyhow::Error>;
    /// Release this packet's wallet inputs after a definite pre-publication failure.
    async fn release_psbt_inputs(&self, psbt: &Psbt) -> Result<(), anyhow::Error>;
    async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<WalletUtxo, anyhow::Error>;
    async fn get_current_height(&self) -> Result<u32, anyhow::Error>;
    async fn get_confirmed_blockchain_time(&self, blocks: usize) -> Result<u64, anyhow::Error>;
    async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error>;
    async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error>;
    /// Includes mempool spends; wallet-owned UTXO lists are insufficient for DLC outputs.
    async fn payout_output_status(
        &self,
        outpoint: OutPoint,
        output: TxOut,
    ) -> Result<PayoutOutputStatus, anyhow::Error>;
    async fn broadcast(&self, transaction: &Transaction) -> Result<(), anyhow::Error>;
    async fn get_next_address(&self) -> Result<Address, anyhow::Error>;
    async fn get_public_key(&self) -> Result<PublicKey, anyhow::Error>;
    async fn get_derived_private_key(&self) -> Result<Scalar, anyhow::Error>;
    async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error>;
    /// Signs and finalizes every wallet input through LND.
    async fn sign_psbt(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error>;
    async fn list_utxos(&self) -> Vec<WalletUtxo>;
    async fn sync(&self) -> Result<(), anyhow::Error>;
    async fn get_balance(&self) -> Result<WalletBalance, anyhow::Error>;
    async fn get_outputs(&self) -> Result<Vec<WalletUtxo>, anyhow::Error>;
    async fn send_to_address(
        &self,
        send_options: SendOptions,
        selected_utxos: Vec<OutPoint>,
    ) -> Result<Txid, anyhow::Error>;
    }
}

struct Fixture {
    directory: TempDir,
    database: DBConnection,
    ln: Arc<MockLnClient>,
    wallet: Arc<MockWallet>,
    fail_broadcast: Arc<AtomicBool>,
    broadcasts: Arc<Mutex<Vec<Transaction>>>,
    released: Arc<Mutex<Vec<Psbt>>>,
    ticket: Ticket,
}

async fn open(directory: &TempDir) -> DBConnection {
    DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap()
}

impl Fixture {
    async fn new(storage_failure: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = open(&directory).await;
        let store = CompetitionStore::new(database.clone());
        let now = OffsetDateTime::now_utc();
        let event = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: now + time::Duration::hours(3),
            start_observation_date: now + time::Duration::hours(1),
            end_observation_date: now + time::Duration::hours(2),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 2,
            entry_fee: 50_000,
            coordinator_fee_percentage: 0,
            total_competition_pool: 100_000,
            relative_locktime_block_delta: Some(72),
        });
        let tickets = (1..=2u8)
            .map(|byte| Ticket {
                id: Uuid::now_v7(),
                competition_id: event.id,
                entry_id: None,
                encrypted_preimage: hex::encode([byte; 32]),
                hash: hex::encode(sha256::Hash::hash(&[byte; 32]).to_byte_array()),
                payment_request: None,
                invoice_expires_at: None,
                expiry: now + time::Duration::minutes(10),
                ephemeral_pubkey: None,
                reserved_by: None,
                reserved_at: None,
                paid_at: None,
                settled_at: None,
                escrow_transaction: None,
            })
            .collect();
        let event = store
            .add_competition_with_tickets(event, tickets)
            .await
            .unwrap();
        let reserved = store
            .get_and_reserve_ticket(event.id, "owner")
            .await
            .unwrap();
        let ticket = reserved.ticket;
        let key = PublicKey::from_str(
            &Scalar::from_slice(&[2; 32])
                .unwrap()
                .base_point_mul()
                .to_string(),
        )
        .unwrap();
        store
            .update_ticket_pubkey(&ticket, key.to_string())
            .await
            .unwrap();
        let ln = Arc::new(MockLnClient::new());
        let invoice = ln
            .add_hold_invoice(50_000, 600, ticket.hash.clone(), event.id)
            .await
            .unwrap();
        let id = ticket.id.to_string();
        database
            .execute_write(move |pool| async move {
                sqlx::query("UPDATE tickets SET payment_request = ? WHERE id = ?")
                    .bind(invoice.payment_request)
                    .bind(id)
                    .execute(&pool)
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        ln.accept_invoice(&ticket.hash).unwrap();
        // The live subscription can commit paid_at before polling gets there.
        store
            .mark_ticket_paid(&ticket.hash, event.id)
            .await
            .unwrap();
        let ticket = store.get_ticket(ticket.id).await.unwrap();
        let fail_broadcast = Arc::new(AtomicBool::new(!storage_failure));
        let broadcasts = Arc::new(Mutex::new(Vec::new()));
        let released = Arc::new(Mutex::new(Vec::new()));
        let mut wallet = MockWallet::new();
        wallet.expect_get_network().return_const(Network::Regtest);
        wallet
            .expect_get_derived_private_key()
            .returning(|| Ok(Scalar::from_slice(&[1; 32]).unwrap()));
        wallet.expect_get_public_key().returning(|| {
            Ok(PublicKey::from_str(
                &Scalar::from_slice(&[1; 32])
                    .unwrap()
                    .base_point_mul()
                    .to_string(),
            )
            .unwrap())
        });
        wallet
            .expect_get_estimated_fee_rates()
            .returning(|| Ok(HashMap::from([(1, 1.0)])));
        wallet
            .expect_build_psbt()
            .times(1)
            .returning(|script, amount, _, _, _| {
                let transaction = Transaction {
                    version: bitcoin::transaction::Version::TWO,
                    lock_time: bitcoin::absolute::LockTime::ZERO,
                    input: vec![bitcoin::TxIn {
                        previous_output: OutPoint::new(Txid::from_byte_array([42; 32]), 0),
                        ..Default::default()
                    }],
                    output: vec![bitcoin::TxOut {
                        value: amount,
                        script_pubkey: script,
                    }],
                };
                let mut packet = Psbt::from_unsigned_tx(transaction).unwrap();
                packet.inputs[0].witness_utxo = Some(bitcoin::TxOut {
                    value: amount + Amount::from_sat(1_000),
                    script_pubkey: ScriptBuf::new(),
                });
                packet.proprietary.insert(
                    bitcoin::psbt::raw::ProprietaryKey {
                        prefix: b"test".to_vec(),
                        subtype: 0,
                        key: b"lease".to_vec(),
                    },
                    vec![5; 32],
                );
                Ok(packet)
            });
        wallet
            .expect_reserve_psbt_inputs_until()
            .times(1)
            .returning(|_, _| Ok(()));
        wallet.expect_sign_psbt().times(1).returning(|packet| {
            packet.inputs[0].final_script_witness =
                Some(bitcoin::Witness::from_slice(&[vec![1; 64]]));
            Ok(true)
        });
        let record = broadcasts.clone();
        let failing = fail_broadcast.clone();
        wallet.expect_broadcast().returning(move |transaction| {
            record.lock().unwrap().push(transaction.clone());
            if failing.load(Ordering::SeqCst) {
                Err(anyhow!("publication response lost"))
            } else {
                Ok(())
            }
        });
        wallet
            .expect_get_raw_transaction()
            .returning(|_| Err(anyhow!("observation unavailable")));
        let released_packets = released.clone();
        wallet
            .expect_release_psbt_inputs()
            .times(usize::from(storage_failure))
            .returning(move |packet| {
                assert!(
                    !packet.proprietary.is_empty(),
                    "lease ownership must survive signing"
                );
                released_packets.lock().unwrap().push(packet.clone());
                Ok(())
            });
        Self {
            directory,
            database,
            ln,
            wallet: Arc::new(wallet),
            fail_broadcast,
            broadcasts,
            released,
            ticket,
        }
    }

    async fn watcher(&self) -> InvoiceWatcher {
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(self.database.clone()),
            self.wallet.clone(),
            self.ln.clone(),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            Arc::new(
                KeymeldService::new(KeymeldSettings::default(), Uuid::now_v7(), &[1; 32]).unwrap(),
            ),
            None,
            72,
            1,
            "escrow-recovery-test".into(),
            true,
            1,
        )
        .await
        .unwrap();
        InvoiceWatcher::new(
            Arc::new(coordinator),
            self.ln.clone(),
            CancellationToken::new(),
            Duration::from_millis(5),
        )
    }

    async fn ticket(&self) -> Ticket {
        CompetitionStore::new(self.database.clone())
            .get_ticket(self.ticket.id)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn unknown_publication_and_settlement_recover_exact_escrow_after_restart() {
    let mut f = Fixture::new(false).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        f.watcher().await.handle_pending_invoices(),
    )
    .await
    .unwrap()
    .unwrap();
    let after_error = f.ticket().await;
    assert_eq!(after_error.hash, f.ticket.hash);
    assert!(after_error.paid_at.is_some());
    assert!(after_error.settled_at.is_none());
    let stored_bytes = after_error
        .escrow_transaction
        .clone()
        .expect("persist before unknown publication");
    assert_eq!(f.broadcasts.lock().unwrap().len(), 3);
    assert!(f.released.lock().unwrap().is_empty());
    let store = CompetitionStore::new(f.database.clone());
    assert_eq!(store.get_pending_tickets().await.unwrap().len(), 1);
    assert!(!store
        .update_ticket_escrow_transaction(&after_error, "different transaction")
        .await
        .unwrap());
    drop(store);
    f.database.close().await.unwrap();
    f.database = open(&f.directory).await;
    f.fail_broadcast.store(false, Ordering::SeqCst);

    // LND settles successfully but the local settlement commit fails.
    f.database.execute_write(|pool| async move {
        sqlx::query("CREATE TRIGGER reject_settlement BEFORE UPDATE OF settled_at ON tickets BEGIN SELECT RAISE(ABORT, 'test lost settlement commit'); END").execute(&pool).await?;
        Ok(())
    }).await.unwrap();
    f.watcher().await.handle_pending_invoices().await.unwrap();
    assert_eq!(
        f.ln.get_invoice_state(&f.ticket.hash),
        Some(InvoiceState::Settled)
    );
    assert!(f.ticket().await.settled_at.is_none());
    f.database
        .execute_write(|pool| async move {
            sqlx::query("DROP TRIGGER reject_settlement")
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    f.database.close().await.unwrap();
    f.database = open(&f.directory).await;
    f.watcher().await.handle_pending_invoices().await.unwrap();
    let settled = f.ticket().await;
    assert!(settled.settled_at.is_some());
    assert_eq!(settled.hash, f.ticket.hash);
    assert_eq!(
        settled.escrow_transaction.as_deref(),
        Some(stored_bytes.as_str())
    );
    {
        let publications = f.broadcasts.lock().unwrap();
        assert_eq!(publications.len(), 5);
        assert!(publications
            .iter()
            .all(|tx| hex::encode(bitcoin::consensus::serialize(tx)) == stored_bytes));
    }
    assert!(CompetitionStore::new(f.database.clone())
        .get_pending_tickets()
        .await
        .unwrap()
        .is_empty());
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn definite_storage_failure_releases_owned_leases_without_publication_or_ticket_reset() {
    let f = Fixture::new(true).await;
    f.database.execute_write(|pool| async move {
        sqlx::query("CREATE TRIGGER reject_escrow BEFORE UPDATE OF escrow_transaction ON tickets BEGIN SELECT RAISE(ABORT, 'test storage failure'); END").execute(&pool).await?;
        Ok(())
    }).await.unwrap();
    f.watcher().await.handle_pending_invoices().await.unwrap();
    let ticket = f.ticket().await;
    assert_eq!(ticket.hash, f.ticket.hash);
    assert!(ticket.paid_at.is_some());
    assert!(ticket.settled_at.is_none());
    assert!(ticket.escrow_transaction.is_none());
    assert_eq!(
        f.ln.get_invoice_state(&ticket.hash),
        Some(InvoiceState::Accepted)
    );
    assert!(f.broadcasts.lock().unwrap().is_empty());
    assert_eq!(f.released.lock().unwrap().len(), 1);
    f.database.close().await.unwrap();
}
