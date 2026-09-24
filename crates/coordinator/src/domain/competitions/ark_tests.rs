//! Arkade tickets against a scripted arkd and ark-swapd: when a swap pays for a ticket.

use super::*;
use crate::infra::{
    ark_swap::{EscrowSwaps, RefundSwap as MintedRefund, Swap, SwapState},
    bitcoin_mock::MockBitcoinClient,
    db::{DBConnection, DatabasePoolConfig, DatabaseType},
    keymeld::{
        ContractCommitment, DlcKeygenSession, DlcSubsetInfo, KeygenSessionStatus, Keymeld,
        KeymeldError, ParticipantRegistrationData, PayoutCapabilities, PayoutContractBoundResponse,
        PayoutPreparedResponse, PayoutSecrets, PreparePayoutRequest, ReleasePayoutRequest,
    },
    lightning_mock::MockLnClient,
    lnurl_mock::MockLnurlPay,
    oracle_mock::MockOracle,
};
use async_trait::async_trait;
use bitcoin::hashes::Hash;
use bitcoin::{Amount, Network, OutPoint, Txid};
use coordinator_ark::testing::{keypair, mock_info, xonly, MockArkd};
use coordinator_ark::ArkServer;
use coordinator_core::RegistrationAssignment;
use coordinator_escrow::authorization::PayoutPolicy;
use keymeld_sdk::dlctix::{dlctix::SigningData, DlcSignatureResults};
use keymeld_sdk::prelude::UserId;
use keymeld_sdk::types::SignedRoster;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// The ticket price: a 5,000 sat entry fee and the coordinator's 10%.
const PRICE: u64 = 5_500;

/// ark-swapd, answering for the swaps a test gives it.
#[derive(Default)]
struct Swaps {
    swaps: Mutex<HashMap<Uuid, Swap>>,
}

#[async_trait]
impl EscrowSwaps for Swaps {
    async fn create_swap(&self, _: &str, _: u64, _: &[u8; 32]) -> anyhow::Result<Swap> {
        panic!("each ticket here already has its swap")
    }

    async fn swap(&self, id: Uuid) -> anyhow::Result<Swap> {
        self.swaps
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("no swap {id}"))
    }

    async fn mint_refund(&self, _: &str, _: u64, _: &str, _: u32) -> anyhow::Result<MintedRefund> {
        panic!("no refunds here")
    }

    async fn refund_paid(&self, _: Uuid, _: &[u8; 32]) -> anyhow::Result<MintedRefund> {
        panic!("no refunds here")
    }

    async fn refund(&self, _: Uuid) -> anyhow::Result<MintedRefund> {
        panic!("no refunds here")
    }
}

/// Keymeld, enabled for automatic payouts, which an Arkade competition needs.
struct Enclaves;

#[async_trait]
impl Keymeld for Enclaves {
    async fn payout_capabilities(&self) -> Result<PayoutCapabilities, KeymeldError> {
        Ok(PayoutCapabilities {
            payout: true,
            lnurl: true,
        })
    }
    async fn bind_payout_contract(
        &self,
        _: &DlcKeygenSession,
        _: &ContractCommitment,
        _: &BTreeMap<UserId, PayoutPolicy>,
    ) -> Result<Vec<PayoutContractBoundResponse>, KeymeldError> {
        panic!("no contract here")
    }
    async fn prepare_payout(
        &self,
        _: &DlcKeygenSession,
        _: UserId,
        _: PreparePayoutRequest,
    ) -> Result<PayoutPreparedResponse, KeymeldError> {
        panic!("no payout here")
    }
    async fn release_payout(
        &self,
        _: &DlcKeygenSession,
        _: UserId,
        _: ReleasePayoutRequest,
    ) -> Result<PayoutSecrets, KeymeldError> {
        panic!("no payout here")
    }
    async fn init_keygen_session(
        &self,
        _: Uuid,
        _: Vec<UserId>,
        _: DlcSubsetInfo,
    ) -> Result<DlcKeygenSession, KeymeldError> {
        panic!("no session here")
    }
    async fn register_participant(
        &self,
        _: &DlcKeygenSession,
        _: UserId,
        _: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        panic!("no registration here")
    }
    async fn wait_for_keygen_completion(
        &self,
        _: &DlcKeygenSession,
    ) -> Result<SignedRoster, KeymeldError> {
        panic!("no keygen here")
    }
    async fn get_keygen_status(
        &self,
        _: &DlcKeygenSession,
    ) -> Result<KeygenSessionStatus, KeymeldError> {
        panic!("no keygen here")
    }
    async fn sign_dlc_batch(
        &self,
        _: &DlcKeygenSession,
        _: &SigningData,
        _: &dlctix::ContractParameters,
        _: Vec<UserId>,
    ) -> Result<DlcSignatureResults, KeymeldError> {
        panic!("no contract here")
    }
    fn is_enabled(&self) -> bool {
        true
    }
    fn coordinator_user_id(&self) -> UserId {
        UserId::from(Uuid::from_u128(999))
    }
    async fn get_registration_assignment(
        &self,
        _: &DlcKeygenSession,
        _: UserId,
    ) -> Result<RegistrationAssignment, KeymeldError> {
        panic!("no registration here")
    }
}

/// A ticket reserved for its escrow swap.
struct ArkTicket {
    id: Uuid,
    hash: String,
    /// The ticket's own escrow address, encoded.
    escrow_address: String,
    swap_id: Uuid,
}

struct Fixture {
    _directory: TempDir,
    database: DBConnection,
    coordinator: Coordinator,
    arkd: Arc<MockArkd>,
    swaps: Arc<Swaps>,
    server: ArkServer,
    competition_id: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        // With RUST_LOG set, a failing test shows what the coordinator logged.
        let _ = env_logger::builder().is_test(true).try_init();
        let directory = tempfile::tempdir().unwrap();
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let info = mock_info(&keypair(7));
        let server = ArkServer::offline(info.clone()).unwrap();
        let arkd = Arc::new(MockArkd::offchain(&info));
        let swaps = Arc::new(Swaps::default());
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database.clone()),
            Arc::new(MockBitcoinClient::new(Network::Regtest)),
            Arc::new(MockLnClient::new()),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            Arc::new(Enclaves),
            None,
            72,
            1,
            "arkade-test".into(),
            false,
            1,
        )
        .await
        .unwrap()
        .with_automatic_payouts(true, 100)
        .unwrap()
        .with_ark(Some(Arkade {
            server: server.clone(),
            transport: arkd.clone(),
            swaps: swaps.clone(),
            refund_after_start_secs: 60 * 60,
            max_refund_fee_sats: 100,
        }))
        .unwrap();

        let now = OffsetDateTime::now_utc();
        let competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: now + time::Duration::hours(3),
            start_observation_date: now + time::Duration::hours(1),
            end_observation_date: now + time::Duration::hours(2),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 3,
            entry_fee: 5_000,
            coordinator_fee_percentage: 10,
            total_competition_pool: 15_000,
            relative_locktime_block_delta: Some(72),
        });
        assert_eq!(competition.calculate_invoice_amount(), PRICE);
        let competition_id = competition.id;
        let store = &coordinator.competition_store;
        store
            .add_competition_with_tickets(competition, vec![])
            .await
            .unwrap();
        store.mark_ark_funded(competition_id).await.unwrap();
        Self {
            _directory: directory,
            database,
            coordinator,
            arkd,
            swaps,
            server,
            competition_id,
        }
    }

    fn store(&self) -> &CompetitionStore {
        &self.coordinator.competition_store
    }

    /// Reserve a ticket whose entry key is test key `player`, fix its escrow, and give it a swap
    /// of `amount_sat` into that escrow.
    async fn ticket(&self, player: u8, amount_sat: u64) -> ArkTicket {
        let id = Uuid::now_v7();
        let preimage = [player; 32];
        let hash = hex::encode(bitcoin::hashes::sha256::Hash::hash(&preimage).to_byte_array());
        let (competition_id, ticket_hash) = (self.competition_id.to_string(), hash.clone());
        self.database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, payment_request,
                        reserved_at, reserved_by)
                     VALUES (?, ?, ?, ?, 'lntbs1swap', datetime('now'), 'player')",
                )
                .bind(id.to_string())
                .bind(competition_id)
                .bind(hex::encode(preimage))
                .bind(ticket_hash)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp() as u32;
        let terms = self
            .server
            .escrow_terms(
                xonly(&keypair(player)),
                xonly(&keypair(22)),
                now + 24 * 60 * 60,
                now,
            )
            .unwrap();
        let escrow = self.server.entry_escrow(terms).unwrap();
        let escrow_address = escrow.address(self.server.hrp()).unwrap().encode();
        self.store()
            .store_ticket_ark_escrow(
                id,
                hash.clone(),
                hex::encode(escrow.vtxo_script().encode_tap_tree()),
                escrow_address.clone(),
            )
            .await
            .unwrap();
        let swap_id = Uuid::now_v7();
        self.store()
            .set_ticket_ark_swap(id, hash.clone(), swap_id)
            .await
            .unwrap();
        self.swaps.swaps.lock().unwrap().insert(
            swap_id,
            Swap {
                id: swap_id,
                escrow_address: escrow_address.clone(),
                amount_sat,
                payment_hash: hash.clone(),
                invoice: "lntbs1swap".into(),
                state: SwapState::AwaitingPayment,
                escrow_vtxo: None,
                ark_txid: None,
                error: None,
                expires_at: i64::from(now) + 600,
            },
        );
        ArkTicket {
            id,
            hash,
            escrow_address,
            swap_id,
        }
    }

    /// ark-swapd reports the ticket's swap in `state`, having paid `vtxo` or in `ark_txid`.
    fn swap_reports(
        &self,
        ticket: &ArkTicket,
        state: SwapState,
        vtxo: Option<OutPoint>,
        ark_txid: Option<Txid>,
    ) {
        let mut swaps = self.swaps.swaps.lock().unwrap();
        let swap = swaps.get_mut(&ticket.swap_id).unwrap();
        swap.state = state;
        swap.escrow_vtxo = vtxo.map(|vtxo| vtxo.to_string());
        swap.ark_txid = ark_txid.map(|txid| txid.to_string());
    }

    /// Arkade lists a VTXO worth `sats` at `address`.
    fn arkade_lists(&self, address: &str, outpoint: OutPoint, sats: u64, spent: bool) {
        let created_at = OffsetDateTime::now_utc().unix_timestamp();
        self.arkd
            .add_vtxo(address, outpoint, Amount::from_sat(sats), created_at, spent);
    }

    /// Whether the ticket is paid, settled, and its escrow recorded as funded by `vtxo`.
    async fn paid_by(&self, ticket: &ArkTicket) -> Option<(String, u64)> {
        let stored = self.store().get_ticket(ticket.id).await.unwrap();
        let escrow = self
            .store()
            .ticket_ark_escrow(ticket.id, &ticket.hash)
            .await
            .unwrap()
            .unwrap();
        match (stored.paid_at, stored.settled_at, escrow.vtxo_outpoint) {
            (Some(_), Some(_), Some(vtxo)) => Some((vtxo, escrow.vtxo_sats.unwrap())),
            (None, None, None) => None,
            other => panic!("a ticket is paid and its escrow funded together, not {other:?}"),
        }
    }

    async fn pending(&self) -> usize {
        self.store().pending_ark_swaps().await.unwrap().len()
    }
}

fn outpoint(byte: u8, vout: u32) -> OutPoint {
    OutPoint::new(Txid::from_byte_array([byte; 32]), vout)
}

#[tokio::test]
async fn a_settled_swap_pays_the_ticket_once_arkade_lists_its_escrow_vtxo() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let paid = outpoint(0xa1, 0);
    // ark-swapd settled before its indexer listed the VTXO, so it knows only the Ark
    // transaction that paid it.
    f.swap_reports(&ticket, SwapState::Settled, None, Some(paid.txid));
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, None, "Arkade has not listed it yet");
    assert_eq!(f.pending().await, 1);

    f.arkade_lists(&ticket.escrow_address, paid, PRICE, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, Some((paid.to_string(), PRICE)));
    assert_eq!(f.pending().await, 0, "a paid ticket's swap is done");
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn the_vtxo_a_swap_names_must_be_listed_at_the_escrow_with_the_ticket_price() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let named = outpoint(0xa2, 1);
    f.swap_reports(&ticket, SwapState::Settled, Some(named), Some(named.txid));
    f.arkade_lists(&ticket.escrow_address, named, PRICE, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, Some((named.to_string(), PRICE)));
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_vtxo_from_another_transaction_does_not_pay_the_ticket() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    f.swap_reports(&ticket, SwapState::Settled, None, Some(outpoint(0xa3, 0).txid));
    f.arkade_lists(&ticket.escrow_address, outpoint(0xb3, 0), PRICE, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, None);
    assert_eq!(f.pending().await, 1);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_vtxo_worth_less_than_the_ticket_price_does_not_pay_it() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let paid = outpoint(0xa4, 0);
    f.swap_reports(&ticket, SwapState::Settled, None, Some(paid.txid));
    f.arkade_lists(&ticket.escrow_address, paid, PRICE - 1, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, None);

    // The same holds for a VTXO the swap names.
    let named = f.ticket(23, PRICE).await;
    let short = outpoint(0xa5, 0);
    f.swap_reports(&named, SwapState::Settled, Some(short), None);
    f.arkade_lists(&named.escrow_address, short, PRICE - 1, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&named).await, None);
    assert_eq!(f.pending().await, 2);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_vtxo_at_another_address_does_not_pay_the_ticket() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let other = f.ticket(23, PRICE).await;
    let paid = outpoint(0xa6, 0);
    f.swap_reports(&ticket, SwapState::Settled, Some(paid), Some(paid.txid));
    // The VTXO exists, but in another ticket's escrow.
    f.arkade_lists(&other.escrow_address, paid, PRICE, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, None);
    assert_eq!(f.paid_by(&other).await, None);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_spent_vtxo_does_not_pay_the_ticket() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let paid = outpoint(0xa7, 0);
    f.swap_reports(&ticket, SwapState::Settled, None, Some(paid.txid));
    f.arkade_lists(&ticket.escrow_address, paid, PRICE, true);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, None);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_swap_for_another_amount_than_the_ticket_price_does_not_pay_it() {
    let f = Fixture::new().await;
    // ark-swapd's own amount agrees with the VTXO, but not with what the ticket costs.
    let ticket = f.ticket(21, PRICE - 500).await;
    let paid = outpoint(0xa8, 0);
    f.swap_reports(&ticket, SwapState::Settled, Some(paid), Some(paid.txid));
    f.arkade_lists(&ticket.escrow_address, paid, PRICE - 500, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, None);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn only_a_settled_player_payment_pays_the_ticket() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let paid = outpoint(0xa9, 0);
    f.arkade_lists(&ticket.escrow_address, paid, PRICE, false);
    // The escrow is paid, but the player's invoice is not settled yet.
    f.swap_reports(&ticket, SwapState::EscrowPaid, Some(paid), Some(paid.txid));
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, None);

    // Settling failed and the player's payment went back to them: the ticket is released.
    f.swap_reports(&ticket, SwapState::Unsettled, Some(paid), Some(paid.txid));
    f.coordinator.check_ark_swaps().await.unwrap();
    let released = f.store().get_ticket(ticket.id).await.unwrap();
    assert!(released.paid_at.is_none() && released.reserved_at.is_none());
    assert_eq!(f.pending().await, 0);
    f.database.close().await.unwrap();
}
