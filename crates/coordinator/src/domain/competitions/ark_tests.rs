//! Arkade tickets against a scripted arkd, ark-swapd, and Keymeld: when a swap pays for a
//! ticket, and how the escrows of a competition that died are refunded.

use super::*;
use crate::infra::{
    ark_swap::{
        BoardFailure, EscrowSwaps, RefundSwap as MintedRefund, Swap, SwapState, SwapWallet,
    },
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
use bitcoin::key::Keypair;
use bitcoin::{Amount, Network, OutPoint, Txid};
use coordinator_ark::testing::{keypair, mock_info, xonly, MockArkd};
use coordinator_ark::ArkServer;
use coordinator_ark_escrow::{EntryEscrow, RefundSwap, SwapTerms};
use coordinator_core::RegistrationAssignment;
use coordinator_escrow::ark::ArkEscrowSpend;
use coordinator_escrow::authorization::{ArkEscrowPolicy, PayoutPolicy};
use keymeld_core::authorization::EnclaveRecipientAuthorization;
use keymeld_sdk::dlctix::{dlctix::SigningData, DlcSignatureResults};
use keymeld_sdk::prelude::UserId;
use keymeld_sdk::types::{EnclaveId, SessionAuthorizationManifest, SignedRoster};
use keymeld_sdk::{AuthorizationCredentials, SessionCredentials, SessionId};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// The ticket price: a 5,000 sat entry fee and the coordinator's 10%.
const PRICE: u64 = 5_500;

/// ark-swapd, answering for the swaps a test gives it and minting refund swaps.
struct Swaps {
    server: ArkServer,
    swaps: Mutex<HashMap<Uuid, Swap>>,
    refunds: Mutex<HashMap<Uuid, MintedRefund>>,
    minted: AtomicUsize,
    /// Refunds whose preimage was reported, so the service could claim them.
    claimed: Mutex<Vec<Uuid>>,
    /// Lose the answer to the next report of a refund's preimage.
    lose_next_claim: AtomicBool,
    /// How its boards last went, or `None` for a wallet it cannot read.
    wallet: Mutex<Option<SwapWallet>>,
}

impl Swaps {
    fn new(server: ArkServer) -> Self {
        Self {
            server,
            swaps: Mutex::default(),
            refunds: Mutex::default(),
            minted: AtomicUsize::new(0),
            claimed: Mutex::default(),
            lose_next_claim: AtomicBool::new(false),
            wallet: Mutex::new(Some(SwapWallet::default())),
        }
    }

    /// A refund swap for `payment_hash` that the player can take back from `deadline`.
    fn refund_swap(
        &self,
        player: bitcoin::XOnlyPublicKey,
        payment_hash: [u8; 32],
        deadline: u32,
    ) -> RefundSwap {
        let rules = self.server.rules();
        let deadline = bitcoin::absolute::LockTime::from_time(deadline).unwrap();
        let created_at = OffsetDateTime::now_utc().unix_timestamp() as u32;
        RefundSwap::new(SwapTerms {
            player,
            swapper: xonly(&keypair(30)),
            server: rules.signer,
            payment_hash,
            deadline,
            exit_delay: rules.min_exit_delay,
            unilateral_reclaim_delay: SwapTerms::unilateral_reclaim_delay_for(
                deadline,
                rules.min_exit_delay,
                created_at.min(deadline.to_consensus_u32()),
            )
            .unwrap(),
        })
        .unwrap()
    }

    /// Move a minted refund's deadline to `deadline`, as if it had been minted long ago.
    fn age(&self, id: Uuid, deadline: u32) {
        let mut refunds = self.refunds.lock().unwrap();
        let refund = refunds.get_mut(&id).unwrap();
        let old = RefundSwap::from_vtxo_script(
            &coordinator_ark_escrow::VtxoScript::decode_tap_tree(
                &hex::decode(&refund.swap_tap_tree).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let swap = self.refund_swap(old.terms().player, old.terms().payment_hash, deadline);
        refund.swap_tap_tree = hex::encode(swap.vtxo_script().encode_tap_tree());
        refund.swap_address = swap.address(self.server.hrp()).unwrap().encode();
    }
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

    async fn mint_refund(
        &self,
        payment_hash: &str,
        amount_sat: u64,
        player_key: &str,
        deadline: u32,
    ) -> anyhow::Result<MintedRefund> {
        let hash: [u8; 32] = hex::decode(payment_hash).unwrap().try_into().unwrap();
        let swap = self.refund_swap(player_key.parse().unwrap(), hash, deadline);
        let minted = MintedRefund {
            id: Uuid::now_v7(),
            payment_hash: payment_hash.into(),
            amount_sat,
            swap_tap_tree: hex::encode(swap.vtxo_script().encode_tap_tree()),
            swap_address: swap.address(self.server.hrp()).unwrap().encode(),
            state: crate::infra::ark_swap::RefundState::Minted,
            swap_vtxo: None,
            claim_txid: None,
            error: None,
        };
        self.minted.fetch_add(1, Ordering::SeqCst);
        self.refunds
            .lock()
            .unwrap()
            .insert(minted.id, minted.clone());
        Ok(minted)
    }

    async fn refund_paid(&self, id: Uuid, preimage: &[u8; 32]) -> anyhow::Result<MintedRefund> {
        let mut refunds = self.refunds.lock().unwrap();
        let refund = refunds.get_mut(&id).unwrap();
        assert_eq!(
            hex::encode(bitcoin::hashes::sha256::Hash::hash(preimage).to_byte_array()),
            refund.payment_hash,
            "the preimage settles the refund's invoice"
        );
        refund.state = crate::infra::ark_swap::RefundState::Paid;
        let mut claimed = self.claimed.lock().unwrap();
        if !claimed.contains(&id) {
            claimed.push(id);
        }
        if self.lose_next_claim.swap(false, Ordering::SeqCst) {
            anyhow::bail!("ark-swapd restarted before answering");
        }
        Ok(refund.clone())
    }

    async fn refund(&self, id: Uuid) -> anyhow::Result<MintedRefund> {
        self.refunds
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("no refund {id}"))
    }

    async fn wallet(&self) -> anyhow::Result<SwapWallet> {
        self.wallet
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("ark-swapd answered 502 Bad Gateway: arkd is away"))
    }
}

/// Keymeld, holding each player's entry key once their entry is registered.
#[derive(Default)]
struct Enclaves {
    /// Each registered player's entry key, escrow, and consent to its refunds.
    players: Mutex<HashMap<UserId, (Keypair, EntryEscrow, ArkEscrowPolicy)>>,
    registered: Mutex<Vec<UserId>>,
    /// Whether a refund was signed, which sends Keymeld the roster: no one can join after.
    roster_sent: AtomicBool,
    signatures: AtomicUsize,
    /// Why Keymeld refuses to sign refunds, if it does.
    refusal: Mutex<Option<String>>,
    /// Refuse proofs deleting an intent, as a verifier from before they were allowed does.
    refuse_deletes: AtomicBool,
    /// How many delete proofs Keymeld signed.
    deletes: AtomicUsize,
    /// Refuse a refund's batch intent, as a verifier from before they were allowed does.
    refuse_recoveries: AtomicBool,
    /// How many refund intents Keymeld was asked to sign, and how many it signed.
    recovery_requests: AtomicUsize,
    recoveries: AtomicUsize,
}

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
        user: UserId,
        registration: &ParticipantRegistrationData,
    ) -> Result<(), KeymeldError> {
        assert!(
            registration.payout_policy.is_some(),
            "an entry registers with the payout policy it accepted"
        );
        let mut registered = self.registered.lock().unwrap();
        if !registered.contains(&user) {
            if self.roster_sent.load(Ordering::SeqCst) {
                return Err(KeymeldError::RosterFixed(user.to_string()));
            }
            registered.push(user);
        }
        Ok(())
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
    /// Sign as the player with their entry key, over the digest the verifier derives.
    async fn sign_ark_refund(
        &self,
        _: &DlcKeygenSession,
        user: UserId,
        spend: ArkEscrowSpend,
        _: String,
        _: u64,
    ) -> Result<[u8; 64], KeymeldError> {
        if let Some(refusal) = self.refusal.lock().unwrap().clone() {
            return Err(KeymeldError::Signing(refusal));
        }
        assert!(
            self.registered.lock().unwrap().contains(&user),
            "Keymeld signs only for a registered entry"
        );
        self.roster_sent.store(true, Ordering::SeqCst);
        let players = self.players.lock().unwrap();
        let (key, escrow, policy) = players.get(&user).expect("a known player");
        let (_, refund) = coordinator_escrow::ark::refund_from(escrow, policy, &spend).unwrap();
        self.signatures.fetch_add(1, Ordering::SeqCst);
        Ok(bitcoin::secp256k1::Secp256k1::new()
            .sign_schnorr_no_aux_rand(
                &bitcoin::secp256k1::Message::from_digest(refund.digests[0].1),
                key,
            )
            .serialize())
    }
    /// Sign as the player both inputs of the intent the verifier derives for a refund in a
    /// batch.
    async fn sign_ark_refund_intent(
        &self,
        _: &DlcKeygenSession,
        user: UserId,
        spend: ArkEscrowSpend,
        _: String,
        _: u64,
    ) -> Result<Vec<(usize, [u8; 64])>, KeymeldError> {
        self.recovery_requests.fetch_add(1, Ordering::SeqCst);
        if self.refuse_recoveries.load(Ordering::SeqCst) {
            return Err(KeymeldError::Signing(
                "Unsupported Coordinator verifier action".into(),
            ));
        }
        assert!(
            self.registered.lock().unwrap().contains(&user),
            "Keymeld signs only for a registered entry"
        );
        self.roster_sent.store(true, Ordering::SeqCst);
        let players = self.players.lock().unwrap();
        let (key, escrow, policy) = players.get(&user).expect("a known player");
        let (_, refund) = coordinator_escrow::ark::refund_from(escrow, policy, &spend).unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp() as u64;
        coordinator_escrow::ark::check_refund_intent_fresh(escrow, &spend, now).unwrap();
        self.recoveries.fetch_add(1, Ordering::SeqCst);
        let secp = bitcoin::secp256k1::Secp256k1::new();
        Ok(refund
            .digests
            .into_iter()
            .map(|(input, digest)| {
                let message = bitcoin::secp256k1::Message::from_digest(digest);
                (
                    input,
                    secp.sign_schnorr_no_aux_rand(&message, key).serialize(),
                )
            })
            .collect())
    }
    /// Sign as the player each input the verifier derives for a delete proof.
    async fn sign_ark_intent_delete(
        &self,
        _: &DlcKeygenSession,
        user: UserId,
        spend: ArkEscrowSpend,
    ) -> Result<Vec<(usize, [u8; 64])>, KeymeldError> {
        if self.refuse_deletes.load(Ordering::SeqCst) {
            return Err(KeymeldError::Signing(
                "Unsupported Coordinator verifier action".into(),
            ));
        }
        assert!(
            self.registered.lock().unwrap().contains(&user),
            "Keymeld signs only for a registered entry"
        );
        let players = self.players.lock().unwrap();
        let (key, escrow, _) = players.get(&user).expect("a known player");
        let digests = coordinator_escrow::ark::intent_delete_digests(escrow, &spend).unwrap();
        self.deletes.fetch_add(1, Ordering::SeqCst);
        let secp = bitcoin::secp256k1::Secp256k1::new();
        Ok(digests
            .into_iter()
            .map(|(input, digest)| {
                let message = bitcoin::secp256k1::Message::from_digest(digest);
                (
                    input,
                    secp.sign_schnorr_no_aux_rand(&message, key).serialize(),
                )
            })
            .collect())
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

/// What a ticket's player sent Keymeld.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sent {
    Nothing,
    /// The registration their browser sends before showing the invoice, and no entry.
    Registration,
    /// An entry.
    Entry,
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
    enclaves: Arc<Enclaves>,
    ln: Arc<MockLnClient>,
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
        let swaps = Arc::new(Swaps::new(server.clone()));
        let enclaves = Arc::new(Enclaves::default());
        let ln = Arc::new(MockLnClient::new());
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database.clone()),
            Arc::new(MockBitcoinClient::new(Network::Regtest)),
            ln.clone(),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            enclaves.clone(),
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
            refund_after_start_secs: crate::config::DEFAULT_REFUND_AFTER_START_SECS,
            escrow_expiry_margin_secs: crate::config::DEFAULT_ESCROW_EXPIRY_MARGIN_SECS,
            max_refund_fee_sats: 100,
        }))
        .unwrap();
        // The tests check a swap several times in a row, so every check lists the escrows, and
        // a dropped subscription is opened again at once.
        coordinator.escrow_watch.set_immediate();

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
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(10),
            total_competition_pool: 15_000,
            relative_locktime_block_delta: Some(72),
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
        });
        assert_eq!(competition.calculate_invoice_amount(), PRICE);
        let competition_id = competition.id;
        let store = &coordinator.competition_store;
        // No tickets up front: its tickets are the ones a test reserves. Keymeld's roster is
        // the competition's tickets, so it is complete once each has an entry, as when a
        // competition fills. A ticket reserved without an entry models one that never filled.
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
            enclaves,
            ln,
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
        let now = OffsetDateTime::now_utc().unix_timestamp() as u32;
        self.ticket_refundable_from(player, amount_sat, now + 24 * 60 * 60)
            .await
            .0
    }

    /// A ticket as [`Fixture::ticket`] makes it, whose escrow's refund leaf opens at `refund_at`,
    /// and the escrow.
    async fn ticket_refundable_from(
        &self,
        player: u8,
        amount_sat: u64,
        refund_at: u32,
    ) -> (ArkTicket, EntryEscrow) {
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
                coordinator_key(),
                refund_at,
                refund_at.min(now) - 60 * 60,
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
        (
            ArkTicket {
                id,
                hash,
                escrow_address,
                swap_id,
            },
            escrow,
        )
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

    /// Arkade says the VTXO at `outpoint` expires at `expires_at` (UNIX seconds), swept or not.
    fn arkade_expires(&self, outpoint: OutPoint, expires_at: i64, swept: bool) {
        let mut state = self.arkd.state.lock().unwrap();
        let vtxo = state
            .vtxos
            .iter_mut()
            .find(|vtxo| vtxo.outpoint == outpoint)
            .expect("a listed VTXO");
        vtxo.expires_at = expires_at;
        vtxo.is_swept = swept;
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

    /// The competition's Keymeld session, as created with the competition.
    async fn keymeld_session(&self) -> DlcKeygenSession {
        self.keymeld_session_for(&[UserId::new_v7()]).await
    }

    /// The competition's Keymeld session, whose manifest names `players`.
    async fn keymeld_session_for(&self, players: &[UserId]) -> DlcKeygenSession {
        let session = session_for(players);
        self.coordinator
            .store_keymeld_session(self.competition_id, session.clone())
            .await
            .unwrap();
        session
    }

    /// A ticket of test key `player` whose escrow ark-swapd funded with the ticket price, and
    /// whose refund leaf is open. With `entered`, the player used it for an entry that gave a
    /// Lightning Address; without, they paid but never entered, nor sent a registration.
    async fn funded(&self, session: &DlcKeygenSession, player: u8, entered: bool) -> ArkTicket {
        let sent = if entered { Sent::Entry } else { Sent::Nothing };
        self.funded_after(session, player, sent).await
    }

    /// A ticket as [`Fixture::funded`] makes it, whose player sent Keymeld `sent`.
    async fn funded_after(&self, session: &DlcKeygenSession, player: u8, sent: Sent) -> ArkTicket {
        let now = OffsetDateTime::now_utc().unix_timestamp() as u32;
        let (ticket, escrow) = self
            .ticket_refundable_from(player, PRICE, now - 60 * 60)
            .await;
        let (registration, policy) = self.registration(session, &ticket, &escrow, player);
        if sent == Sent::Registration {
            // Sent before paying, with the payout policy accepted for the ticket.
            self.store()
                .store_ticket_payout_policy(
                    ticket.id,
                    ticket.hash.clone(),
                    registration.ephemeral_pubkey.clone(),
                    serde_json::to_string(&policy).unwrap(),
                )
                .await
                .unwrap();
            self.register(&ticket, &registration).await;
        }
        let vtxo = outpoint(player, 0);
        self.arkade_lists(&ticket.escrow_address, vtxo, PRICE, false);
        self.swap_reports(&ticket, SwapState::Settled, Some(vtxo), Some(vtxo.txid));
        self.coordinator.check_ark_swaps().await.unwrap();
        assert!(self.paid_by(&ticket).await.is_some());
        if sent != Sent::Entry {
            return ticket;
        }

        let entry_id = ticket.id;
        let submission = serde_json::to_string(&crate::infra::oracle::AddEventEntry {
            id: entry_id,
            event_id: self.competition_id,
            expected_observations: vec![],
        })
        .unwrap();
        let (competition_id, ticket_id) = (self.competition_id.to_string(), ticket.id.to_string());
        let entry_key = registration.ephemeral_pubkey.clone();
        let context = serde_json::to_string(&registration.keymeld_registration_context).unwrap();
        self.database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey,
                        payout_hash, entry_submission, keymeld_registration_context,
                        encrypted_keymeld_private_key, keymeld_auth_pubkey)
                     VALUES (?, ?, ?, 'player', ?, ?, ?, ?, 'sealed-to-the-enclave', ?)",
                )
                .bind(entry_id.to_string())
                .bind(competition_id)
                .bind(ticket_id)
                .bind(entry_key)
                .bind(hex::encode([player; 32]))
                .bind(submission)
                .bind(context)
                .bind(hex::encode([2; 33]))
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
        self.store()
            .store_entry_payout_policy(entry_id, serde_json::to_string(&policy).unwrap())
            .await
            .unwrap();
        ticket
    }

    /// The registration test key `player`'s browser seals for `ticket`, and the payout policy it
    /// accepts; Keymeld is told the key, to sign as the player.
    fn registration(
        &self,
        session: &DlcKeygenSession,
        ticket: &ArkTicket,
        escrow: &EntryEscrow,
        player: u8,
    ) -> (TicketRegistration, PayoutPolicy) {
        let key = keypair(player);
        let user = UserId::from(ticket.id);
        let ark_escrow = ArkEscrowPolicy {
            escrow_tap_tree: hex::encode(escrow.vtxo_script().encode_tap_tree()),
            max_fee_sats: PRICE - 5_000,
            max_refund_fee_sats: 100,
            checkpoint_exit_script: hex::encode(self.server.info().checkpoint_tapscript.as_bytes()),
        };
        let policy = PayoutPolicy {
            queued_entry: None,
            automatic_lightning_address: Some(format!("player{player}@mock-wallet.dev")),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            contract_terms: "{}".into(),
            ark_escrow: Some(ark_escrow.clone()),
        };
        let context = RegistrationContext {
            keygen_session_id: session.session_id.clone(),
            manifest_hash: session.authorization_manifest.digest().unwrap(),
            user_id: user.clone(),
            enclave_id: EnclaveId::new(1),
            enclave_key_epoch: 1,
            public_key: key.public_key().serialize().to_vec(),
            auth_pubkey: vec![2; 33],
            require_signing_approval: false,
        };
        self.enclaves
            .players
            .lock()
            .unwrap()
            .insert(user, (key, escrow.clone(), ark_escrow));
        let registration = TicketRegistration {
            ephemeral_pubkey: key.public_key().to_string(),
            encrypted_keymeld_private_key: "sealed-to-the-enclave".into(),
            keymeld_auth_pubkey: hex::encode([2; 33]),
            keymeld_registration_context: context,
            keymeld_escrow_policy: None,
        };
        (registration, policy)
    }

    /// Keep `registration` for the ticket, as its player's browser sends it before paying.
    async fn register(&self, ticket: &ArkTicket, registration: &TicketRegistration) {
        assert_eq!(
            self.store()
                .store_ticket_registration(
                    ticket.id,
                    ticket.hash.clone(),
                    "player".into(),
                    serde_json::to_string(registration).unwrap(),
                )
                .await
                .unwrap(),
            RegistrationStored::Stored
        );
    }

    /// An unpaid ticket of test key `player`, whose player sent their registration.
    async fn registered_unpaid(&self, session: &DlcKeygenSession, player: u8) -> ArkTicket {
        let now = OffsetDateTime::now_utc().unix_timestamp() as u32;
        let (ticket, escrow) = self
            .ticket_refundable_from(player, PRICE, now - 60 * 60)
            .await;
        let (registration, _) = self.registration(session, &ticket, &escrow, player);
        self.register(&ticket, &registration).await;
        ticket
    }

    /// The ticket's invoice expires at `offset` from now, as SQLite reads it: "+1 hour".
    async fn invoice_expires(&self, ticket: &ArkTicket, offset: &str) {
        let (ticket_id, offset) = (ticket.id.to_string(), offset.to_string());
        self.database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE tickets SET invoice_expires_at = datetime('now', ?) WHERE id = ?",
                )
                .bind(offset)
                .bind(ticket_id)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// How many registrations players sent before paying are kept.
    async fn registrations(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM ticket_keymeld_registrations")
            .fetch_one(self.database.read())
            .await
            .unwrap()
    }

    /// The competition is cancelled before a batch funds its pool.
    async fn cancel(&self) {
        let mut competition = self
            .store()
            .get_competition(self.competition_id)
            .await
            .unwrap();
        competition.cancelled_at = Some(OffsetDateTime::now_utc());
        self.store()
            .update_competitions(vec![competition])
            .await
            .unwrap();
    }

    async fn clean_up(&self) {
        self.coordinator.clean_up_competitions().await.unwrap();
    }

    async fn awaiting_cleanup(&self) -> bool {
        self.store()
            .get_competitions_pending_cleanup(false)
            .await
            .unwrap()
            .contains(&self.competition_id)
    }

    async fn refund(&self, ticket: &ArkTicket) -> Option<TicketArkRefund> {
        self.store().ticket_ark_refund(ticket.id).await.unwrap()
    }

    /// A kickoff's batch intent, never confirmed, holds the escrows of `players`' tickets.
    fn intent_holds(&self, players: &[u8]) {
        let signers = players
            .iter()
            .map(|player| {
                let keys = [xonly(&keypair(*player)), coordinator_key()];
                (outpoint(*player, 0), keys)
            })
            .collect();
        self.arkd.queue_intent("kickoff-intent", signers);
    }

    /// The offchain spends Arkade took, and how many of them were finalized.
    fn spends(&self) -> (usize, usize) {
        let state = self.arkd.state.lock().unwrap();
        (
            state.offchain.len(),
            state
                .offchain
                .iter()
                .filter(|spend| !spend.finalized.is_empty())
                .count(),
        )
    }
}

/// A Keymeld session like the one made with each competition, whose manifest names `players`,
/// one per ticket.
fn session_for(players: &[UserId]) -> DlcKeygenSession {
    let coordinator = UserId::from(Uuid::from_u128(999));
    let creator = AuthorizationCredentials::from_secret(&[11; 32]).unwrap();
    let signing = AuthorizationCredentials::from_secret(&[12; 32]).unwrap();
    let credentials = SessionCredentials::from_session_secret(&[13; 32]).unwrap();
    let registrations: BTreeMap<_, _> = std::iter::once(coordinator.clone())
        .chain(players.iter().cloned())
        .enumerate()
        .map(|(i, user)| {
            (
                user,
                AuthorizationCredentials::from_secret(&[20 + i as u8; 32]).unwrap(),
            )
        })
        .collect();
    let session_id = SessionId::new_v7();
    let manifest = SignedSessionManifest::sign(
        SessionAuthorizationManifest {
            keygen_session_id: session_id.clone(),
            coordinator_user_id: coordinator,
            creator_pubkey: creator.public_key_bytes(),
            signing_pubkey: signing.public_key_bytes(),
            session_public_key: credentials.public_key_bytes(),
            participant_verifiers: registrations
                .iter()
                .map(|(user, authority)| (user.clone(), authority.public_key_bytes()))
                .collect(),
            timeout_secs: 300,
            max_signing_sessions: None,
            encrypted_taproot_tweak: "test".into(),
            subset_definitions: vec![],
            deposit_scope: None,
        },
        &creator.export_secret(),
    )
    .unwrap();
    let enclave = EnclaveId::new(1);
    let recipients = EnclaveRecipientAuthorization::sign(
        &manifest,
        registrations
            .keys()
            .map(|user| (user.clone(), enclave))
            .collect(),
        BTreeMap::from([(
            enclave,
            AuthorizationCredentials::from_secret(&[6; 32])
                .unwrap()
                .public_key_bytes(),
        )]),
        &creator.export_secret(),
    )
    .unwrap();
    DlcKeygenSession {
        session_id,
        session_secret: [13; 32],
        authorization_manifest: manifest,
        recipient_authorization: recipients,
        signing_authority: signing,
        registration_authorities: registrations,
        aggregate_key: vec![],
        outcome_subset_ids: BTreeMap::new(),
    }
}

/// The coordinator's key, which the mock Bitcoin client derives as 1.
fn coordinator_key() -> bitcoin::XOnlyPublicKey {
    let mut secret = [0; 32];
    secret[31] = 1;
    let secret = bitcoin::secp256k1::SecretKey::from_slice(&secret).unwrap();
    xonly(&Keypair::from_secret_key(
        &bitcoin::secp256k1::Secp256k1::new(),
        &secret,
    ))
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
    assert_eq!(
        f.paid_by(&ticket).await,
        None,
        "Arkade has not listed it yet"
    );
    assert_eq!(f.pending().await, 1);

    f.arkade_lists(&ticket.escrow_address, paid, PRICE, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, Some((paid.to_string(), PRICE)));
    assert_eq!(f.pending().await, 0, "a paid ticket's swap is done");
    f.database.close().await.unwrap();
}

/// The output script, hex, of the escrow at `address`, as the subscription watches it.
fn escrow_script(address: &str) -> String {
    coordinator_ark::ArkAddress::decode(address)
        .unwrap()
        .to_p2tr_script_pubkey()
        .to_hex_string()
}

/// Wait until `check` holds, for up to five seconds.
async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if check().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("waited in vain until {what}");
}

#[tokio::test]
async fn the_escrow_subscription_pays_a_ticket_as_soon_as_arkade_reports_its_escrow() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let paid = outpoint(0xc1, 0);
    let events = crate::metrics::ESCROW_EVENTS.get();
    let cancel = tokio_util::sync::CancellationToken::new();
    let driver = async {
        until("the escrow is watched", || async {
            f.arkd.subscribed_scripts() == vec![escrow_script(&ticket.escrow_address)]
        })
        .await;
        f.swap_reports(&ticket, SwapState::Settled, Some(paid), Some(paid.txid));
        f.arkade_lists(&ticket.escrow_address, paid, PRICE, false);
        assert!(f.arkd.announce_vtxo(paid));
        until("the ticket is paid", || async {
            f.paid_by(&ticket).await.is_some()
        })
        .await;
        until("its escrow is no longer watched", || async {
            f.arkd.subscribed_scripts().is_empty()
        })
        .await;
        cancel.cancel();
    };
    let (watched, ()) = tokio::join!(f.coordinator.watch_ark_escrows(cancel.clone()), driver);
    watched.unwrap();
    assert_eq!(f.paid_by(&ticket).await, Some((paid.to_string(), PRICE)));
    assert!(
        f.arkd.listings().is_empty(),
        "the VTXO the event carried was verified without listing the escrow"
    );
    assert!(crate::metrics::ESCROW_EVENTS.get() > events);
    assert_eq!(f.pending().await, 0);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn an_escrow_reported_before_ark_swapd_settles_is_paid_on_the_next_check_without_a_listing() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let paid = outpoint(0xc2, 0);
    let cancel = tokio_util::sync::CancellationToken::new();
    let driver = async {
        until("the escrow is watched", || async {
            !f.arkd.subscribed_scripts().is_empty()
        })
        .await;
        // ark-swapd paid the escrow, and Arkade reported it, but the player's invoice is not
        // settled yet.
        f.swap_reports(&ticket, SwapState::EscrowPaid, Some(paid), Some(paid.txid));
        f.arkade_lists(&ticket.escrow_address, paid, PRICE, false);
        assert!(f.arkd.announce_vtxo(paid));
        until("the event is taken in", || async {
            !f.coordinator
                .escrow_watch
                .seen(&ticket.escrow_address)
                .is_empty()
        })
        .await;
        assert_eq!(f.paid_by(&ticket).await, None);
        cancel.cancel();
    };
    let (watched, ()) = tokio::join!(f.coordinator.watch_ark_escrows(cancel.clone()), driver);
    watched.unwrap();

    f.swap_reports(&ticket, SwapState::Settled, Some(paid), Some(paid.txid));
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, Some((paid.to_string(), PRICE)));
    assert!(f.arkd.listings().is_empty());
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_dropped_escrow_subscription_resubscribes_with_the_swaps_pending_then() {
    let f = Fixture::new().await;
    let first = f.ticket(21, PRICE).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let driver = async {
        until("the first escrow is watched", || async {
            f.arkd.subscribed_scripts() == vec![escrow_script(&first.escrow_address)]
        })
        .await;
        // Another swap is made meanwhile, as by another coordinator process, and the server
        // drops the subscription.
        let second = f.ticket(23, PRICE).await;
        f.arkd.drop_subscription();
        let mut both = vec![
            escrow_script(&first.escrow_address),
            escrow_script(&second.escrow_address),
        ];
        both.sort();
        until("a new subscription watches both escrows", || async {
            f.arkd.state.lock().unwrap().subscriptions == 2 && f.arkd.subscribed_scripts() == both
        })
        .await;

        // The new stream delivers.
        let paid = outpoint(0xc3, 0);
        f.swap_reports(&second, SwapState::Settled, Some(paid), Some(paid.txid));
        f.arkade_lists(&second.escrow_address, paid, PRICE, false);
        assert!(f.arkd.announce_vtxo(paid));
        until("the second ticket is paid", || async {
            f.paid_by(&second).await.is_some()
        })
        .await;
        cancel.cancel();
    };
    let (watched, ()) = tokio::join!(f.coordinator.watch_ark_escrows(cancel.clone()), driver);
    watched.unwrap();
    assert_eq!(f.paid_by(&first).await, None);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn the_escrow_listing_finds_a_payment_the_subscription_missed() {
    let f = Fixture::new().await;
    let ticket = f.ticket(21, PRICE).await;
    let paid = outpoint(0xc4, 0);
    let cancel = tokio_util::sync::CancellationToken::new();
    let driver = async {
        until("the escrow is watched", || async {
            !f.arkd.subscribed_scripts().is_empty()
        })
        .await;
        // Arkade lists the escrow VTXO, but the event never came.
        f.swap_reports(&ticket, SwapState::Settled, Some(paid), Some(paid.txid));
        f.arkade_lists(&ticket.escrow_address, paid, PRICE, false);
        f.coordinator.check_ark_swaps().await.unwrap();
        assert_eq!(f.paid_by(&ticket).await, Some((paid.to_string(), PRICE)));
        until("its escrow is no longer watched", || async {
            f.arkd.subscribed_scripts().is_empty()
        })
        .await;
        cancel.cancel();
    };
    let (watched, ()) = tokio::join!(f.coordinator.watch_ark_escrows(cancel.clone()), driver);
    watched.unwrap();
    assert_eq!(f.arkd.listings(), vec![vec![ticket.escrow_address.clone()]]);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn pending_escrows_are_listed_together_not_one_by_one() {
    let f = Fixture::new().await;
    // One Ark transaction paid two escrows, and ark-swapd names only that transaction.
    let batch = outpoint(0xc5, 0).txid;
    let first = f.ticket(21, PRICE).await;
    let second = f.ticket(23, PRICE).await;
    f.swap_reports(&first, SwapState::Settled, None, Some(batch));
    f.swap_reports(&second, SwapState::Settled, None, Some(batch));
    f.arkade_lists(&first.escrow_address, OutPoint::new(batch, 0), PRICE, false);
    f.arkade_lists(
        &second.escrow_address,
        OutPoint::new(batch, 1),
        PRICE,
        false,
    );
    let third = f.ticket(25, PRICE).await;
    let paid = outpoint(0xc6, 0);
    f.swap_reports(&third, SwapState::Settled, Some(paid), Some(paid.txid));
    f.arkade_lists(&third.escrow_address, paid, PRICE, false);
    // One whose player has not paid is not looked up.
    let unpaid = f.ticket(27, PRICE).await;

    f.coordinator.check_ark_swaps().await.unwrap();
    let listings = f.arkd.listings();
    assert_eq!(
        listings.len(),
        1,
        "one listing for every escrow: {listings:?}"
    );
    let mut listed = listings[0].clone();
    listed.sort();
    let mut expected = vec![
        first.escrow_address.clone(),
        second.escrow_address.clone(),
        third.escrow_address.clone(),
    ];
    expected.sort();
    assert_eq!(listed, expected);
    assert_eq!(
        f.paid_by(&first).await,
        Some((OutPoint::new(batch, 0).to_string(), PRICE))
    );
    assert_eq!(
        f.paid_by(&second).await,
        Some((OutPoint::new(batch, 1).to_string(), PRICE))
    );
    assert_eq!(f.paid_by(&third).await, Some((paid.to_string(), PRICE)));
    assert_eq!(f.paid_by(&unpaid).await, None);
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
    f.swap_reports(
        &ticket,
        SwapState::Settled,
        None,
        Some(outpoint(0xa3, 0).txid),
    );
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
async fn a_coin_that_expires_before_the_refund_could_finish_does_not_pay_the_ticket() {
    let f = Fixture::new().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let margin = crate::config::DEFAULT_ESCROW_EXPIRY_MARGIN_SECS as i64;
    // Its escrow's refund leaf opens in an hour.
    let (ticket, _) = f
        .ticket_refundable_from(21, PRICE, now as u32 + 3_600)
        .await;
    let paid = outpoint(0xaa, 0);
    f.swap_reports(&ticket, SwapState::Settled, Some(paid), Some(paid.txid));
    f.arkade_lists(&ticket.escrow_address, paid, PRICE, false);

    // ark-swapd paid it from a coin with two hours of life left: a preconfirmed VTXO expires
    // with the coin it was paid from. A refund opening in an hour would find it expired or
    // nearly so.
    f.arkade_expires(paid, now + 7_200, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(
        f.paid_by(&ticket).await,
        None,
        "a short-lived escrow never reaches a pool"
    );
    assert_eq!(f.pending().await, 1);

    // One Arkade already swept is refused whatever expiry it lists.
    f.arkade_expires(paid, now + 30 * 86_400, true);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, None);

    // A coin that outlives the refund locktime by the margin pays the ticket.
    f.arkade_expires(paid, now + 3_600 + margin, false);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.paid_by(&ticket).await, Some((paid.to_string(), PRICE)));
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

#[tokio::test]
async fn a_cancelled_arkade_competition_refunds_each_funded_escrow_once() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let first = f.funded(&session, 21, true).await;
    let second = f.funded(&session, 23, true).await;
    assert!(
        !f.awaiting_cleanup().await,
        "a live competition is not cleaned up"
    );
    // The refund leaves opened an hour ago, and nothing is refunded: only a competition that
    // will never kick off refunds its escrows, however early their locktime.
    f.clean_up().await;
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.store()
            .ark_refunds_started(f.competition_id)
            .await
            .unwrap(),
        0
    );
    f.cancel().await;
    assert!(
        f.awaiting_cleanup().await,
        "its paid and settled Arkade tickets still hold escrows"
    );

    f.clean_up().await;
    for ticket in [&first, &second] {
        let refund = f.refund(ticket).await.unwrap();
        assert_eq!(refund.state, ArkRefundState::Settled);
        assert!(refund.ark_txid.is_some() && refund.checkpoint_psbt.is_some());
    }
    assert_eq!(
        f.enclaves.registered.lock().unwrap().clone(),
        vec![UserId::from(first.id), UserId::from(second.id)],
        "a competition that filled but died before its contract registers its entries before \
         signing their refunds"
    );
    assert_eq!(
        f.spends(),
        (2, 2),
        "each escrow was spent into its swap once"
    );
    assert_eq!(f.enclaves.signatures.load(Ordering::SeqCst), 4);
    assert_eq!(f.ln.payments_sent(), 2);
    assert_eq!(f.swaps.claimed.lock().unwrap().len(), 2);
    assert!(!f.awaiting_cleanup().await, "nothing is left to refund");
    assert_eq!(
        f.store()
            .ark_refunds_started(f.competition_id)
            .await
            .unwrap(),
        2,
        "a kickoff would refuse to fund a pool whose escrows went back"
    );

    // Later passes find nothing to do.
    f.coordinator.refund_ark_escrows(f.competition_id).await;
    f.clean_up().await;
    assert_eq!(f.spends(), (2, 2));
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 2);
    assert_eq!(f.ln.payments_sent(), 2, "no player is paid twice");
    f.database.close().await.unwrap();
}

/// 2026-09-30: the Arkade server failed every batch, then every boarding, for hours. A ticket
/// sold then is paid into an escrow that can neither kick off nor be refunded, so while the
/// server is failing no new ticket is issued; one already invoiced goes on. A refund the server
/// takes lifts the pause.
#[tokio::test]
async fn no_ticket_is_issued_while_the_arkade_server_is_failing() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let entered = f.funded(&session, 21, true).await;
    let id = Uuid::now_v7();
    let competition_id = f.competition_id.to_string();
    f.database
        .execute_write(move |pool| async move {
            sqlx::query(
                "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_at,
                    reserved_by)
                 VALUES (?, ?, ?, ?, datetime('now'), 'player')",
            )
            .bind(id.to_string())
            .bind(competition_id)
            .bind(hex::encode([40; 32]))
            .bind(hex::encode([41; 32]))
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();
    let competition = f.store().get_competition(f.competition_id).await.unwrap();
    let fresh = f.store().get_ticket(id).await.unwrap();
    let invoiced = f.store().get_ticket(entered.id).await.unwrap();
    assert!(fresh.payment_request.is_none());

    f.coordinator.arkade_health().failed(
        "INTERNAL_ERROR (0): failed to rescan boarding utxos: HTTP 500",
        OffsetDateTime::now_utc(),
    );
    assert!(f.coordinator.arkade_unavailable());
    assert!(
        f.coordinator
            .network_fee_quote()
            .await
            .unwrap()
            .arkade_unavailable
    );
    let refused = f
        .coordinator
        .ticket_network_fee(&competition, &fresh)
        .await
        .unwrap_err();
    assert!(matches!(refused, Error::ArkadeUnavailable), "{refused}");
    assert_eq!(
        refused.to_string(),
        "Entries are paused while the Arkade network recovers; try again in a little while"
    );
    f.coordinator
        .ticket_network_fee(&competition, &invoiced)
        .await
        .expect("a ticket already invoiced goes on");

    // The refund of a cancelled competition's escrow goes through, so the server is working.
    // The unpaid ticket is let go first: refunds wait while a ticket can still be paid.
    f.database
        .execute_write(move |pool| async move {
            sqlx::query("DELETE FROM tickets WHERE id = ?")
                .bind(id.to_string())
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    f.cancel().await;
    f.clean_up().await;
    assert_eq!(
        f.refund(&entered).await.unwrap().state,
        ArkRefundState::Settled
    );
    assert!(!f.coordinator.arkade_unavailable());
    assert!(
        !f.coordinator
            .network_fee_quote()
            .await
            .unwrap()
            .arkade_unavailable
    );
    f.database.close().await.unwrap();
}

/// While no batch step of the coordinator's own runs, ark-swapd's failed boards show the Arkade
/// server is failing; its next board lifts the pause. An unreadable wallet changes nothing.
#[tokio::test]
async fn ark_swapds_failed_boards_pause_entries() {
    let f = Fixture::new().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let rescan = BoardFailure {
        at: now - 60,
        message: "board: Failed to join batch: request failed code: 'Internal error', message: \
                  \"INTERNAL_ERROR (0): failed to rescan boarding utxos: HTTP 500\""
            .into(),
    };
    *f.swaps.wallet.lock().unwrap() = Some(SwapWallet {
        last_board_failure: Some(rescan.clone()),
        last_board_success_at: Some(now - 600),
    });
    f.coordinator.read_ark_swap_boards().await;
    assert!(f.coordinator.arkade_unavailable());

    *f.swaps.wallet.lock().unwrap() = None;
    f.coordinator.read_ark_swap_boards().await;
    assert!(
        f.coordinator.arkade_unavailable(),
        "an unreadable wallet changes nothing"
    );

    *f.swaps.wallet.lock().unwrap() = Some(SwapWallet {
        last_board_failure: Some(rescan),
        last_board_success_at: Some(now),
    });
    f.coordinator.read_ark_swap_boards().await;
    assert!(!f.coordinator.arkade_unavailable());
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_paid_ticket_never_entered_is_refunded_with_the_registration_sent_before_paying() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let entered = f.funded(&session, 21, true).await;
    let never_entered = f.funded_after(&session, 23, Sent::Registration).await;
    f.cancel().await;

    f.clean_up().await;
    for ticket in [&entered, &never_entered] {
        assert_eq!(
            f.refund(ticket).await.unwrap().state,
            ArkRefundState::Settled
        );
    }
    assert_eq!(
        f.enclaves.registered.lock().unwrap().clone(),
        vec![UserId::from(entered.id), UserId::from(never_entered.id)],
        "Keymeld registers the entry, and the ticket that was paid and never entered"
    );
    assert_eq!(f.spends(), (2, 2));
    assert_eq!(f.ln.payments_sent(), 2);
    assert_eq!(
        f.registrations().await,
        0,
        "with nothing left to refund, the registrations are deleted"
    );
    assert!(!f.awaiting_cleanup().await);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_player_keymeld_cannot_register_holds_back_signing_but_not_a_refund_under_way() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let first = f.funded(&session, 21, true).await;
    f.cancel().await;
    // The first refund is paid, and its claim is lost, so it is still under way.
    f.swaps.lose_next_claim.store(true, Ordering::SeqCst);
    f.clean_up().await;
    assert_eq!(f.refund(&first).await.unwrap().state, ArkRefundState::Paid);

    // A second entry turns up whose sealed key is missing, so it cannot be registered.
    let broken = f.funded(&session, 23, true).await;
    let ticket_id = broken.id.to_string();
    f.database
        .execute_write(move |pool| async move {
            sqlx::query(
                "UPDATE entries SET encrypted_keymeld_private_key = NULL WHERE ticket_id = ?",
            )
            .bind(ticket_id)
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();
    f.clean_up().await;
    assert_eq!(
        f.refund(&first).await.unwrap().state,
        ArkRefundState::Settled,
        "the refund under way needs nothing more from Keymeld"
    );
    assert!(
        f.refund(&broken).await.is_none(),
        "nothing is minted for a refund Keymeld cannot sign"
    );
    assert_eq!(f.ln.payments_sent(), 1);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_paid_ticket_whose_player_sent_no_registration_is_left_for_an_operator() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, false).await;
    f.cancel().await;
    assert!(f.awaiting_cleanup().await);

    f.clean_up().await;
    // Its player sealed no entry key to Keymeld, so nothing can sign its refund leaf.
    assert!(f.refund(&ticket).await.is_none());
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 0);
    assert_eq!(f.spends(), (0, 0));
    assert!(
        f.awaiting_cleanup().await,
        "it stays listed as holding a buy-in"
    );
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn unpaid_tickets_registrations_never_reach_keymeld() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let entered = f.funded(&session, 21, true).await;
    // Two players registered and never paid. Their invoices expired, and the watchers have not
    // released their reservations yet.
    for player in [23, 25] {
        let ticket = f.registered_unpaid(&session, player).await;
        f.invoice_expires(&ticket, "-1 minute").await;
    }
    assert_eq!(f.registrations().await, 2);
    f.cancel().await;

    f.clean_up().await;
    assert_eq!(
        f.refund(&entered).await.unwrap().state,
        ArkRefundState::Settled
    );
    assert_eq!(
        f.enclaves.registered.lock().unwrap().clone(),
        vec![UserId::from(entered.id)],
        "only a paid ticket's player is registered with Keymeld"
    );
    assert_eq!(
        f.registrations().await,
        0,
        "a dead competition keeps no registration of a ticket that can no longer be paid"
    );
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn registrations_go_with_their_released_reservations() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let mut unpaid = Vec::new();
    for player in [23, 25] {
        let ticket = f.registered_unpaid(&session, player).await;
        f.invoice_expires(&ticket, "+1 hour").await;
        unpaid.push(ticket);
    }
    f.clean_up().await;
    assert_eq!(
        f.registrations().await,
        2,
        "reservations still held keep theirs"
    );

    // The first invoice is cancelled, and the invoice watcher releases the reservation.
    let cancelled = f.store().get_ticket(unpaid[0].id).await.unwrap();
    assert!(f
        .store()
        .clear_ticket_reservation(&cancelled)
        .await
        .unwrap());
    assert!(f
        .store()
        .ticket_registration(unpaid[0].id, &unpaid[0].hash)
        .await
        .unwrap()
        .is_none());
    assert_eq!(f.registrations().await, 1);

    // The second one's swap expires unpaid, and the swap watcher releases the reservation.
    f.swap_reports(&unpaid[1], SwapState::Expired, None, None);
    f.coordinator.check_ark_swaps().await.unwrap();
    assert_eq!(f.registrations().await, 0);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_taken_over_reservation_loses_its_registration() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let now = OffsetDateTime::now_utc().unix_timestamp() as u32;
    let (ticket, escrow) = f.ticket_refundable_from(23, PRICE, now + 3_600).await;
    let (registration, _) = f.registration(&session, &ticket, &escrow, 23);
    f.register(&ticket, &registration).await;
    let ticket_id = ticket.id.to_string();
    f.database
        .execute_write(move |pool| async move {
            sqlx::query(
                "UPDATE tickets SET reserved_at = datetime('now', '-11 minutes') WHERE id = ?",
            )
            .bind(ticket_id)
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();

    let taken = f
        .store()
        .get_and_reserve_ticket(f.competition_id, "someone else")
        .await
        .unwrap();
    assert_eq!(taken.ticket.id, ticket.id);
    assert_ne!(
        taken.ticket.hash, ticket.hash,
        "a takeover rotates the hash"
    );
    assert_eq!(f.registrations().await, 0);
    // The first player's registration cannot come back for the new reservation.
    assert_eq!(
        f.store()
            .store_ticket_registration(
                ticket.id,
                ticket.hash.clone(),
                "player".into(),
                serde_json::to_string(&registration).unwrap(),
            )
            .await
            .unwrap(),
        RegistrationStored::ReservationChanged
    );
    f.database.close().await.unwrap();
}

/// Store `registration` for `ticket` as `player` sends it.
async fn store_as(
    f: &Fixture,
    ticket: &ArkTicket,
    registration: &TicketRegistration,
    player: &str,
) -> RegistrationStored {
    f.store()
        .store_ticket_registration(
            ticket.id,
            ticket.hash.clone(),
            player.into(),
            serde_json::to_string(registration).unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_registration_is_fixed_once_its_ticket_is_paid() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let now = OffsetDateTime::now_utc().unix_timestamp() as u32;
    let (ticket, escrow) = f.ticket_refundable_from(23, PRICE, now + 3_600).await;
    let (registration, _) = f.registration(&session, &ticket, &escrow, 23);
    let mut resealed = registration.clone();
    resealed.encrypted_keymeld_private_key = "sealed-again".into();

    assert_eq!(
        store_as(&f, &ticket, &registration, "mallory").await,
        RegistrationStored::ReservationChanged,
        "only the ticket's holder registers it"
    );
    assert_eq!(
        store_as(&f, &ticket, &registration, "player").await,
        RegistrationStored::Stored
    );
    assert_eq!(
        store_as(&f, &ticket, &registration, "player").await,
        RegistrationStored::Unchanged
    );
    assert_eq!(
        store_as(&f, &ticket, &resealed, "player").await,
        RegistrationStored::Stored,
        "an unpaid ticket's player may send it again"
    );

    let vtxo = outpoint(23, 0);
    f.arkade_lists(&ticket.escrow_address, vtxo, PRICE, false);
    f.swap_reports(&ticket, SwapState::Settled, Some(vtxo), Some(vtxo.txid));
    f.coordinator.check_ark_swaps().await.unwrap();
    assert!(f.paid_by(&ticket).await.is_some());
    assert_eq!(
        store_as(&f, &ticket, &registration, "player").await,
        RegistrationStored::Fixed
    );
    assert_eq!(
        store_as(&f, &ticket, &resealed, "player").await,
        RegistrationStored::Unchanged
    );
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_completed_competition_keeps_no_registrations() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    f.funded_after(&session, 21, Sent::Registration).await;
    f.clean_up().await;
    assert_eq!(
        f.registrations().await,
        1,
        "a live competition keeps its own"
    );

    let mut competition = f.store().get_competition(f.competition_id).await.unwrap();
    competition.completed_at = Some(OffsetDateTime::now_utc());
    f.store()
        .update_competitions(vec![competition])
        .await
        .unwrap();
    f.clean_up().await;
    assert_eq!(f.registrations().await, 0);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_refund_whose_claim_was_lost_after_paying_finishes_without_paying_again() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    f.cancel().await;

    f.swaps.lose_next_claim.store(true, Ordering::SeqCst);
    f.clean_up().await;
    assert_eq!(f.refund(&ticket).await.unwrap().state, ArkRefundState::Paid);
    assert_eq!(f.ln.payments_sent(), 1);

    f.clean_up().await;
    assert_eq!(
        f.refund(&ticket).await.unwrap().state,
        ArkRefundState::Settled
    );
    assert_eq!(
        f.ln.payments_sent(),
        1,
        "the payment was looked up, not sent again"
    );
    assert_eq!(f.spends(), (1, 1));
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_minted_refund_that_expired_unsigned_is_minted_again() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    f.cancel().await;

    // Keymeld cannot sign yet, as when its enclave cannot be reached.
    *f.enclaves.refusal.lock().unwrap() = Some("the enclave did not answer".into());
    f.clean_up().await;
    let stale = f.refund(&ticket).await.unwrap();
    assert_eq!(stale.state, ArkRefundState::Minted);
    assert_eq!(f.spends(), (0, 0));
    assert_eq!(f.ln.payments_sent(), 0);

    // An hour on, its swap's deadline is too close for the verifier to sign it.
    let now = OffsetDateTime::now_utc().unix_timestamp();
    f.swaps.age(stale.refund_id, now as u32 + 60);
    *f.enclaves.refusal.lock().unwrap() = None;
    f.clean_up().await;
    assert_eq!(
        f.refund(&ticket).await.unwrap().state,
        ArkRefundState::Minted,
        "a refund is minted again at most hourly"
    );
    let ticket_id = ticket.id.to_string();
    f.database
        .execute_write(move |pool| async move {
            sqlx::query(
                "UPDATE ticket_ark_refunds SET created_at = created_at - 3600 WHERE ticket_id = ?",
            )
            .bind(ticket_id)
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();

    f.clean_up().await;
    let refund = f.refund(&ticket).await.unwrap();
    assert_ne!(
        refund.refund_id, stale.refund_id,
        "a fresh swap and invoice"
    );
    assert_eq!(refund.state, ArkRefundState::Settled);
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 2);
    assert_eq!(f.spends(), (1, 1));
    assert_eq!(f.ln.payments_sent(), 1);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn the_escrows_of_a_pool_a_batch_funded_are_not_refunded() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    f.funded(&session, 21, true).await;
    f.store()
        .store_ark_commitment(
            f.competition_id,
            ArkCommitment {
                batch_id: "batch-7".into(),
                commitment_tx: "00".into(),
                funding_vout: 0,
            },
        )
        .await
        .unwrap();
    f.cancel().await;
    assert!(
        !f.awaiting_cleanup().await,
        "the escrows were spent into the pool"
    );
    f.coordinator.refund_ark_escrows(f.competition_id).await;
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 0);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn an_escrow_spent_by_something_else_pays_no_refund() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    f.cancel().await;
    f.arkd
        .state
        .lock()
        .unwrap()
        .vtxos
        .iter_mut()
        .for_each(|vtxo| {
            vtxo.is_spent = true;
            vtxo.spent_by = Some(Txid::from_byte_array([0xee; 32]));
        });

    f.clean_up().await;
    assert!(f.refund(&ticket).await.is_none());
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 0);
    assert_eq!(f.ln.payments_sent(), 0);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_competition_that_never_filled_refunds_the_players_who_entered() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let entered = f.funded(&session, 21, true).await;
    // A second ticket was reserved but its player never paid, so the competition never filled.
    f.ticket(23, PRICE).await;
    f.cancel().await;
    assert!(f.awaiting_cleanup().await);

    f.clean_up().await;
    let refund = f.refund(&entered).await.unwrap();
    assert_eq!(refund.state, ArkRefundState::Settled);
    assert_eq!(
        f.enclaves.registered.lock().unwrap().clone(),
        vec![UserId::from(entered.id)],
        "only the player who paid and entered is registered"
    );
    assert_eq!(f.spends(), (1, 1));
    assert_eq!(f.ln.payments_sent(), 1);
    assert!(!f.awaiting_cleanup().await, "nothing is left to refund");
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn refunds_wait_while_a_ticket_can_still_be_paid() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let entered = f.funded(&session, 21, true).await;
    // Another player holds a ticket whose invoice has not expired. Keymeld's roster cannot
    // change once it signs a refund, so signing now could leave them out if they pay.
    let unpaid = f.ticket(23, PRICE).await;
    f.invoice_expires(&unpaid, "+1 hour").await;
    f.cancel().await;

    f.clean_up().await;
    assert!(f.refund(&entered).await.is_none());
    assert!(f.enclaves.registered.lock().unwrap().is_empty());
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 0);

    // Its invoice expires unpaid.
    f.invoice_expires(&unpaid, "-1 minute").await;
    f.clean_up().await;
    assert_eq!(
        f.refund(&entered).await.unwrap().state,
        ArkRefundState::Settled
    );
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_ticket_counted_after_keymeld_has_the_roster_is_left_for_an_operator() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let first = f.funded(&session, 21, true).await;
    f.cancel().await;
    f.clean_up().await;
    assert_eq!(
        f.refund(&first).await.unwrap().state,
        ArkRefundState::Settled
    );

    // ark-swapd reports a second player's payment only now, after the first refund sent
    // Keymeld the roster.
    let late = f.funded(&session, 23, true).await;
    f.clean_up().await;
    assert!(
        f.refund(&late).await.is_none(),
        "nothing is minted for a refund Keymeld cannot sign"
    );
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 1);
    assert_eq!(f.ln.payments_sent(), 1);
    assert!(
        f.awaiting_cleanup().await,
        "its escrow stays listed as holding a buy-in"
    );
    f.database.close().await.unwrap();
}

/// The registration a player sends before paying is checked like an entry's, kept only for the
/// ticket's holder, and must be the one the entry registers.
#[tokio::test]
async fn the_registration_sent_before_paying_is_the_one_the_entry_registers() {
    let f = Fixture::new().await;
    let now = OffsetDateTime::now_utc().unix_timestamp() as u32;
    let (ticket, escrow) = f.ticket_refundable_from(23, PRICE, now + 3_600).await;
    let session = f.keymeld_session_for(&[UserId::from(ticket.id)]).await;
    let (registration, _) = f.registration(&session, &ticket, &escrow, 23);
    let register = |player: &str, registration: TicketRegistration| {
        f.coordinator
            .register_ticket(player.into(), f.competition_id, ticket.id, registration)
    };

    assert!(matches!(
        register("mallory", registration.clone()).await,
        Err(Error::BadRequest(_))
    ));
    let mut another_key = registration.clone();
    another_key.ephemeral_pubkey = keypair(24).public_key().to_string();
    assert!(
        matches!(
            register("player", another_key).await,
            Err(Error::BadRequest(_))
        ),
        "the envelope's context names the key it registers"
    );
    let mut another_slot = registration.clone();
    another_slot.keymeld_registration_context.user_id = UserId::new_v7();
    assert!(matches!(
        register("player", another_slot).await,
        Err(Error::BadRequest(_))
    ));
    assert_eq!(f.registrations().await, 0);
    register("player", registration.clone()).await.unwrap();
    assert_eq!(f.registrations().await, 1);

    let vtxo = outpoint(23, 0);
    f.arkade_lists(&ticket.escrow_address, vtxo, PRICE, false);
    f.swap_reports(&ticket, SwapState::Settled, Some(vtxo), Some(vtxo.txid));
    f.coordinator.check_ark_swaps().await.unwrap();

    let entry = |sealed: &str| AddEntry {
        id: Uuid::now_v7(),
        ticket_id: ticket.id,
        ephemeral_pubkey: registration.ephemeral_pubkey.clone(),
        payout_hash: hex::encode([23; 32]),
        event_id: f.competition_id,
        expected_observations: vec![],
        encrypted_keymeld_private_key: Some(sealed.into()),
        keymeld_auth_pubkey: Some(registration.keymeld_auth_pubkey.clone()),
        keymeld_registration_context: Some(registration.keymeld_registration_context.clone()),
        keymeld_escrow_policy: None,
    };
    assert!(
        matches!(
            f.coordinator
                .add_entry("player".into(), entry("sealed-again"))
                .await,
            Err(Error::BadRequest(_))
        ),
        "an entry cannot swap in another envelope than the one the ticket was paid with"
    );
    f.coordinator
        .add_entry(
            "player".into(),
            entry(&registration.encrypted_keymeld_private_key),
        )
        .await
        .unwrap();
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_kickoff_intent_left_queued_is_deleted_so_the_refunds_complete() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let first = f.funded(&session, 21, true).await;
    let second = f.funded(&session, 23, true).await;
    f.intent_holds(&[21, 23]);
    f.cancel().await;

    f.clean_up().await;
    for ticket in [&first, &second] {
        assert_eq!(
            f.refund(ticket).await.unwrap().state,
            ArkRefundState::Settled
        );
    }
    assert!(f.arkd.queued().is_empty());
    assert_eq!(
        f.arkd.state.lock().unwrap().deleted,
        vec!["kickoff-intent"],
        "the first refund's proof deleted the whole intent, freeing both escrows"
    );
    assert_eq!(f.enclaves.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.spends(),
        (2, 2),
        "each escrow was spent into its swap once"
    );
    // The held refund was not signed again to be resubmitted.
    assert_eq!(f.enclaves.signatures.load(Ordering::SeqCst), 4);
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 2);
    assert_eq!(f.ln.payments_sent(), 2);
    assert!(!f.awaiting_cleanup().await);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_refund_held_by_an_intent_that_cannot_be_deleted_waits_without_minting_again() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    f.intent_holds(&[21]);
    f.enclaves.refuse_deletes.store(true, Ordering::SeqCst);
    f.cancel().await;

    f.clean_up().await;
    let held = f.refund(&ticket).await.unwrap();
    assert_eq!(held.state, ArkRefundState::Minted);
    assert_eq!(
        held.error.as_deref(),
        Some(super::ark_refund::HELD_BY_INTENT)
    );
    assert_eq!(f.spends(), (0, 0), "Arkade refused it");
    assert_eq!(f.enclaves.signatures.load(Ordering::SeqCst), 1);

    // Hours on, its swap is stale, but a fresh one would be refused as well.
    let now = OffsetDateTime::now_utc().unix_timestamp();
    f.swaps.age(held.refund_id, now as u32 + 60);
    let ticket_id = ticket.id.to_string();
    f.database
        .execute_write(move |pool| async move {
            sqlx::query(
                "UPDATE ticket_ark_refunds SET created_at = created_at - 7200 WHERE ticket_id = ?",
            )
            .bind(ticket_id)
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();
    for _ in 0..3 {
        f.clean_up().await;
    }
    let still = f.refund(&ticket).await.unwrap();
    assert_eq!(still.refund_id, held.refund_id, "not minted again");
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.enclaves.signatures.load(Ordering::SeqCst),
        1,
        "nothing is signed while the escrow is held"
    );
    assert_eq!(f.ln.payments_sent(), 0);

    // Once Keymeld signs the delete, the escrow is freed, the stale refund minted again, and paid.
    f.enclaves.refuse_deletes.store(false, Ordering::SeqCst);
    f.clean_up().await;
    let refund = f.refund(&ticket).await.unwrap();
    assert_eq!(refund.state, ArkRefundState::Settled);
    assert_ne!(refund.refund_id, held.refund_id);
    assert_eq!(refund.error, None);
    assert!(f.arkd.queued().is_empty());
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 2);
    assert_eq!(f.spends(), (1, 1));
    assert_eq!(f.ln.payments_sent(), 1);
    f.database.close().await.unwrap();
}

impl Fixture {
    /// An hour passes for the ticket's minted refund: its swap's deadline is too close to sign
    /// for, and it was minted long enough ago to be minted again.
    async fn refund_goes_stale(&self, ticket: &ArkTicket) {
        let refund = self.refund(ticket).await.unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.swaps.age(refund.refund_id, now as u32 + 60);
        let ticket_id = ticket.id.to_string();
        self.database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE ticket_ark_refunds SET created_at = created_at - 7200 WHERE ticket_id = ?",
                )
                .bind(ticket_id)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
    }
}

/// Production, 2026-09-30: ark-swapd paid escrows from coins with a day left, and their refund
/// leaves opened after they expired. Arkade refused every refund with VTXO_RECOVERABLE, the
/// swap went stale unsigned, and the refund was minted again every hour.
#[tokio::test]
async fn a_refund_arkade_refuses_as_expired_is_held_once_and_not_minted_again() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    f.cancel().await;
    // Arkade's list still shows the escrow alive, but the server refuses to spend it.
    f.arkd.state.lock().unwrap().expired.insert(outpoint(21, 0));

    f.clean_up().await;
    let held = f.refund(&ticket).await.unwrap();
    assert_eq!(held.state, ArkRefundState::Minted);
    assert_eq!(held.error.as_deref(), Some(super::ark_refund::HELD_EXPIRED));
    assert_eq!(f.spends(), (0, 0), "Arkade refused it");
    assert_eq!(f.enclaves.signatures.load(Ordering::SeqCst), 1);
    let logged = f.reported(&ticket).expect("the held refund is logged");
    assert!(logged.contains("VTXO_RECOVERABLE"), "{logged}");

    // Hours on, its swap is stale. A fresh one would be refused as well, so none is minted.
    f.refund_goes_stale(&ticket).await;
    for _ in 0..3 {
        f.clean_up().await;
    }
    let still = f.refund(&ticket).await.unwrap();
    assert_eq!(still.refund_id, held.refund_id, "not minted again");
    assert_eq!(
        still.error.as_deref(),
        Some(super::ark_refund::HELD_EXPIRED)
    );
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.enclaves.signatures.load(Ordering::SeqCst),
        1,
        "nothing is signed for an escrow Arkade will not spend"
    );
    assert_eq!(f.ln.payments_sent(), 0);
    assert!(
        f.awaiting_cleanup().await,
        "its escrow stays listed as holding a buy-in"
    );
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn an_expired_escrow_waits_unminted_until_arkade_sweeps_it_and_is_then_recovered() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    // Expired, but Arkade has not swept it yet: a batch would still want a forfeit for it.
    f.arkade_expires(outpoint(21, 0), now - 60, false);
    f.cancel().await;

    for _ in 0..2 {
        f.clean_up().await;
    }
    assert!(f.refund(&ticket).await.is_none());
    let logged = f.reported(&ticket).expect("the held refund is logged");
    assert!(logged.contains("expired on Arkade"), "{logged}");
    assert_eq!(
        f.swaps.minted.load(Ordering::SeqCst),
        0,
        "the player's provider is not asked for an invoice nothing can pay yet"
    );
    assert_eq!(f.enclaves.recovery_requests.load(Ordering::SeqCst), 0);

    // Arkade sweeps the expired coins, and a batch gives the escrow's value back.
    f.arkade_expires(outpoint(21, 0), now - 60, true);
    f.clean_up().await;
    let refund = f.refund(&ticket).await.unwrap();
    assert_eq!(refund.state, ArkRefundState::Settled);
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 1);
    assert_eq!(f.recovered(), vec![outpoint(21, 0)]);
    assert_eq!(f.spends(), (0, 0), "nothing went offchain");
    assert_eq!(f.ln.payments_sent(), 1);
    assert!(!f.awaiting_cleanup().await);
    f.database.close().await.unwrap();
}

/// The escrows of 2026-09-30: refused all morning as expired, their refunds minted and stale,
/// and since swept by Arkade. A batch recovers each into a fresh swap, and the refund goes on
/// as if the offchain spend had gone through.
#[tokio::test]
async fn a_held_refund_of_a_swept_escrow_is_recovered_in_a_batch_and_its_player_paid() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    f.cancel().await;
    f.arkd.state.lock().unwrap().expired.insert(outpoint(21, 0));
    f.clean_up().await;
    let held = f.refund(&ticket).await.unwrap();
    assert_eq!(held.error.as_deref(), Some(super::ark_refund::HELD_EXPIRED));
    assert_eq!(f.enclaves.signatures.load(Ordering::SeqCst), 1);

    // Hours on, its swap is stale, and Arkade lists the escrow as swept.
    f.refund_goes_stale(&ticket).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    f.arkade_expires(outpoint(21, 0), now - 3_600, true);
    f.clean_up().await;

    let refund = f.refund(&ticket).await.unwrap();
    assert_eq!(refund.state, ArkRefundState::Settled);
    assert_ne!(
        refund.refund_id, held.refund_id,
        "a fresh swap and invoice for the batch to pay"
    );
    assert_eq!(refund.error, None);
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 2);
    {
        // The batch settled the escrow, and its only new VTXO is the fresh swap's, holding
        // the whole ticket price.
        let state = f.arkd.state.lock().unwrap();
        let [(escrow, swap_vtxo)] = state.recovered.as_slice() else {
            panic!("one recovery: {:?}", state.recovered);
        };
        assert_eq!(*escrow, outpoint(21, 0));
        assert_eq!(
            refund.ark_txid,
            state.commitment_txid.map(|txid| txid.to_string()),
            "the refund names the batch's commitment transaction"
        );
        let swap_address = f.swaps.refunds.lock().unwrap()[&refund.refund_id]
            .swap_address
            .clone();
        let paid = state
            .vtxos
            .iter()
            .find(|vtxo| vtxo.outpoint == *swap_vtxo)
            .unwrap();
        assert_eq!(
            paid.script,
            coordinator_ark::ArkAddress::decode(&swap_address)
                .unwrap()
                .to_p2tr_script_pubkey()
        );
        assert_eq!(paid.amount, Amount::from_sat(PRICE));
        assert!(state.queued.is_empty(), "no intent is left on the escrow");
    }
    assert_eq!(f.spends(), (0, 0), "nothing went offchain");
    assert_eq!(f.enclaves.recoveries.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.enclaves.signatures.load(Ordering::SeqCst),
        1,
        "no offchain refund was signed again"
    );
    assert_eq!(f.ln.payments_sent(), 1);
    assert_eq!(f.swaps.claimed.lock().unwrap().len(), 1);
    assert!(!f.awaiting_cleanup().await, "nothing is left to refund");

    // Later passes find nothing to do.
    f.clean_up().await;
    assert_eq!(f.enclaves.recoveries.load(Ordering::SeqCst), 1);
    assert_eq!(f.ln.payments_sent(), 1, "no player is paid twice");
    f.database.close().await.unwrap();
}

/// A recovery that cannot be signed, as while Keymeld's verifier predates refunds in a batch,
/// is tried again after a pause, and asks for a new invoice at most every six hours.
#[tokio::test]
async fn a_recovery_that_fails_waits_before_it_is_tried_or_minted_again() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    f.arkade_expires(outpoint(21, 0), now - 3_600, true);
    f.enclaves.refuse_recoveries.store(true, Ordering::SeqCst);
    f.cancel().await;

    f.clean_up().await;
    let held = f.refund(&ticket).await.unwrap();
    assert_eq!(held.state, ArkRefundState::Minted);
    let note = held.error.clone().expect("the failed recovery is noted");
    assert!(
        note.starts_with(super::ark_refund::HELD_EXPIRED) && note.contains("tried again"),
        "{note}"
    );
    assert_eq!(f.enclaves.recovery_requests.load(Ordering::SeqCst), 1);
    assert!(f.recovered().is_empty());
    assert!(
        f.arkd.queued().is_empty(),
        "no intent is left on the escrow"
    );

    // The next passes wait out the pause, and say the same.
    let logged = f.reported(&ticket);
    for _ in 0..3 {
        f.clean_up().await;
    }
    assert_eq!(f.enclaves.recovery_requests.load(Ordering::SeqCst), 1);
    assert_eq!(f.reported(&ticket), logged, "logged once");

    // After the pause it is tried again, with the swap it has, which is still fresh.
    f.refund_clock(&ticket, "updated_at", 3_600).await;
    for _ in 0..2 {
        f.clean_up().await;
    }
    assert_eq!(f.enclaves.recovery_requests.load(Ordering::SeqCst), 2);
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 1);

    // By the pause after that its swap is stale. While recoveries fail, a new one is minted
    // only every six hours, so the player's provider is not asked every hour.
    f.refund_clock(&ticket, "updated_at", 3_600).await;
    f.refund_goes_stale(&ticket).await;
    for _ in 0..2 {
        f.clean_up().await;
    }
    assert_eq!(f.refund(&ticket).await.unwrap().refund_id, held.refund_id);
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 1);
    assert_eq!(f.enclaves.recovery_requests.load(Ordering::SeqCst), 2);
    assert_eq!(f.ln.payments_sent(), 0);

    // Six hours on Keymeld signs, and the escrow is recovered into a fresh swap.
    f.refund_clock(&ticket, "created_at", 6 * 3_600).await;
    f.enclaves.refuse_recoveries.store(false, Ordering::SeqCst);
    f.clean_up().await;
    let refund = f.refund(&ticket).await.unwrap();
    assert_eq!(refund.state, ArkRefundState::Settled);
    assert_ne!(refund.refund_id, held.refund_id);
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 2);
    assert_eq!(f.recovered(), vec![outpoint(21, 0)]);
    assert_eq!(f.ln.payments_sent(), 1);
    f.database.close().await.unwrap();
}

/// Production, 2026-09-29: Arkade failed every batch ("failed to estimate fee") for a day, and
/// each stale refund was minted again every six hours, each time asking the player's provider
/// for an invoice. A fresh swap does not help a batch Arkade cannot make, so while recoveries
/// keep failing each remint waits twice as long as the last, up to a day.
#[tokio::test]
async fn a_recovery_that_keeps_failing_is_minted_again_less_and_less_often() {
    let hour = 3_600;
    assert_eq!(
        [0, 1, 2, 3, 9].map(
            |remints| super::ark_refund::recovery_remint_interval(remints).as_secs() / hour as u64
        ),
        [6, 12, 24, 24, 24]
    );

    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    f.arkade_expires(outpoint(21, 0), now - hour, true);
    f.enclaves.refuse_recoveries.store(true, Ordering::SeqCst);
    f.cancel().await;
    f.clean_up().await;
    assert_eq!(f.refund(&ticket).await.unwrap().recovery_remints, 0);
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 1);

    for (remints, hours) in [(1, 6), (2, 12), (3, 24), (4, 24)] {
        // Past the pause after the last failure, with the swap stale (two hours older).
        f.refund_clock(&ticket, "updated_at", hour).await;
        f.refund_goes_stale(&ticket).await;
        f.refund_clock(&ticket, "created_at", (hours - 3) * hour)
            .await;
        f.clean_up().await;
        assert_eq!(
            f.swaps.minted.load(Ordering::SeqCst),
            remints,
            "not minted again within {hours} hours"
        );
        f.refund_clock(&ticket, "created_at", hour).await;
        f.clean_up().await;
        assert_eq!(f.swaps.minted.load(Ordering::SeqCst), remints + 1);
        let refund = f.refund(&ticket).await.unwrap();
        assert_eq!(refund.recovery_remints, remints as u32);
        assert!(failed_again(&refund), "{:?}", refund.error);
    }
    assert_eq!(f.ln.payments_sent(), 0);

    // Once Arkade makes the batch, the refund goes through with the swap it has.
    f.enclaves.refuse_recoveries.store(false, Ordering::SeqCst);
    f.refund_clock(&ticket, "updated_at", hour).await;
    f.clean_up().await;
    assert_eq!(
        f.refund(&ticket).await.unwrap().state,
        ArkRefundState::Settled
    );
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 5);
    f.database.close().await.unwrap();
}

/// Whether the refund notes a recovery that failed.
fn failed_again(refund: &TicketArkRefund) -> bool {
    refund.error.as_deref().is_some_and(|note| {
        note.starts_with(super::ark_refund::HELD_EXPIRED) && note.contains("tried again")
    })
}

/// A batch that recovered the escrow can finish after the coordinator stopped following it, as
/// across a restart. The refund then finds its swap funded, and goes on from there.
#[tokio::test]
async fn a_recovery_whose_batch_finished_unseen_is_picked_up_from_its_swap() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, true).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    f.arkade_expires(outpoint(21, 0), now - 3_600, true);
    // The refund is minted, and its first recovery is lost.
    f.enclaves.refuse_recoveries.store(true, Ordering::SeqCst);
    f.cancel().await;
    f.clean_up().await;
    let minted = f.refund(&ticket).await.unwrap();
    assert_eq!(minted.state, ArkRefundState::Minted);

    // Arkade lists the escrow as settled by a batch, which paid nothing to this refund's swap.
    let commitment = Txid::from_byte_array([0xc7; 32]);
    {
        let mut state = f.arkd.state.lock().unwrap();
        let escrow = state
            .vtxos
            .iter_mut()
            .find(|vtxo| vtxo.outpoint == outpoint(21, 0))
            .unwrap();
        escrow.is_spent = true;
        escrow.settled_by = Some(commitment);
    }
    f.clean_up().await;
    assert_eq!(
        f.refund(&ticket).await.unwrap().state,
        ArkRefundState::Minted
    );
    let logged = f.reported(&ticket).unwrap();
    assert!(logged.contains("it needs an operator"), "{logged}");
    assert_eq!(
        f.ln.payments_sent(),
        0,
        "no player is paid for a swap nothing funded"
    );

    // The batch did pay the swap: Arkade lists its VTXO, from the same commitment.
    let swap_address = f.swaps.refunds.lock().unwrap()[&minted.refund_id]
        .swap_address
        .clone();
    let swap_vtxo = outpoint(0xc8, 0);
    f.arkade_lists(&swap_address, swap_vtxo, PRICE, false);
    f.arkd
        .state
        .lock()
        .unwrap()
        .vtxos
        .iter_mut()
        .find(|vtxo| vtxo.outpoint == swap_vtxo)
        .unwrap()
        .commitment_txids = vec![commitment];
    f.clean_up().await;
    let refund = f.refund(&ticket).await.unwrap();
    assert_eq!(refund.state, ArkRefundState::Settled);
    assert_eq!(refund.refund_id, minted.refund_id);
    assert_eq!(refund.ark_txid, Some(commitment.to_string()));
    assert_eq!(f.ln.payments_sent(), 1);
    assert_eq!(
        f.enclaves.recoveries.load(Ordering::SeqCst),
        0,
        "nothing more was signed"
    );
    f.database.close().await.unwrap();
}

impl Fixture {
    /// The escrows Arkade recovered in a batch, in order.
    fn recovered(&self) -> Vec<OutPoint> {
        let state = self.arkd.state.lock().unwrap();
        state.recovered.iter().map(|(escrow, _)| *escrow).collect()
    }

    /// Move the ticket's refund `seconds` back in time, by its `created_at` or `updated_at`.
    async fn refund_clock(&self, ticket: &ArkTicket, column: &str, seconds: i64) {
        let ticket_id = ticket.id.to_string();
        let query = format!(
            "UPDATE ticket_ark_refunds SET {column} = {column} - {seconds} WHERE ticket_id = ?"
        );
        self.database
            .execute_write(move |pool| async move {
                sqlx::query(&query).bind(ticket_id).execute(&pool).await?;
                Ok(())
            })
            .await
            .unwrap();
    }
}

impl Fixture {
    /// What the refund pass last logged about the ticket, while it lasts.
    fn reported(&self, ticket: &ArkTicket) -> Option<String> {
        self.coordinator
            .reported
            .last(super::ark_refund::REFUND_REPORTS, ticket.id)
    }

    async fn progress(&self) -> RefundProgress {
        self.coordinator
            .refund_status(&[self.competition_id])
            .await
            .unwrap()
            .remove(&self.competition_id)
            .unwrap_or_default()
    }

    async fn write_off(&self, ticket: &ArkTicket, force: bool) -> Result<WriteOffReport, Error> {
        self.coordinator
            .write_off_refunds(
                WriteOffTarget::Ticket(ticket.id),
                "the player never sent a registration",
                force,
            )
            .await
    }
}

#[tokio::test]
async fn a_written_off_refund_is_skipped_by_cleanup_without_a_log_line() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let ticket = f.funded(&session, 21, false).await;
    f.cancel().await;
    f.clean_up().await;
    let logged = f.reported(&ticket).expect("the stuck refund is logged");
    assert!(logged.contains("never sent Keymeld"), "{logged}");
    assert!(f.awaiting_cleanup().await);

    let report = f.write_off(&ticket, false).await.unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let [written] = report.written_off.as_slice() else {
        panic!("one refund is written off: {report:?}");
    };
    assert_eq!(written.ticket_id, ticket.id);
    assert_eq!(written.competition_id, f.competition_id);
    assert_eq!(written.vtxo_sats, Some(PRICE));
    assert_eq!(written.reason, "the player never sent a registration");
    assert_eq!(written.refund_state, None, "no refund was ever minted");
    // The player's refund says it was written off, though none was minted.
    let told = f
        .coordinator
        .get_ticket_refund("player".into(), f.competition_id, ticket.id)
        .await
        .unwrap()
        .expect("a written-off refund is reported");
    assert!(told.written_off);
    assert_eq!((told.state.as_str(), told.paid_sats), ("written_off", 0));
    let json = serde_json::to_value(&told).unwrap();
    assert!(json.get("payment_hash").is_none(), "{json}");

    f.coordinator.refund_ark_escrows(f.competition_id).await;
    f.clean_up().await;
    assert_eq!(f.reported(&ticket), None, "cleanup no longer looks at it");
    assert!(f.refund(&ticket).await.is_none());
    assert_eq!(f.swaps.minted.load(Ordering::SeqCst), 0);
    assert_eq!(f.spends(), (0, 0));
    assert!(
        !f.awaiting_cleanup().await,
        "nothing is left to refund, so the competition leaves the cleanup queue"
    );

    // A second write-off is refused, and keeps the first reason.
    let again = f.write_off(&ticket, true).await.unwrap_err();
    assert!(again.to_string().contains("already written off"), "{again}");
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn written_off_escrows_are_no_longer_owed() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let entered = f.funded(&session, 21, true).await;
    let stuck = f.funded(&session, 23, false).await;
    f.cancel().await;
    f.clean_up().await;
    assert_eq!(
        f.refund(&entered).await.unwrap().state,
        ArkRefundState::Settled
    );
    let progress = f.progress().await;
    assert_eq!((progress.refunded, progress.escrowed), (1, 2));
    assert!(
        progress.opens_at.is_some(),
        "the stuck escrow is still owed"
    );

    f.write_off(&stuck, false).await.unwrap();
    let progress = f.progress().await;
    assert_eq!(
        (progress.refunded, progress.escrowed, progress.written_off),
        (1, 1, 1),
        "the written-off escrow leaves the denominator"
    );
    assert_eq!(
        progress.opens_at, None,
        "and no longer says when refunds open"
    );
    assert_eq!(
        f.coordinator
            .refund_progress(Some(f.competition_id))
            .await
            .unwrap()[&f.competition_id],
        progress
    );
    let listed = f
        .coordinator
        .refund_write_offs(Some(f.competition_id))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].ticket_id, stuck.id);
    assert_eq!(listed[0].reason, "the player never sent a registration");
    assert!(!f.awaiting_cleanup().await);
    f.database.close().await.unwrap();
}

#[tokio::test]
async fn a_refund_in_progress_is_written_off_only_with_force() {
    let f = Fixture::new().await;
    let session = f.keymeld_session().await;
    let paid = f.funded(&session, 21, true).await;
    let stuck = f.funded(&session, 23, false).await;

    // Its competition may still run, so nothing is owed back yet.
    let live = f.write_off(&stuck, true).await.unwrap_err();
    assert!(live.to_string().contains("not been cancelled"), "{live}");

    f.cancel().await;
    f.swaps.lose_next_claim.store(true, Ordering::SeqCst);
    f.clean_up().await;
    assert_eq!(f.refund(&paid).await.unwrap().state, ArkRefundState::Paid);

    let refused = f.write_off(&paid, false).await.unwrap_err();
    assert!(
        refused.to_string().contains("in progress (paid)"),
        "{refused}"
    );
    assert!(f
        .coordinator
        .refund_write_offs(None)
        .await
        .unwrap()
        .is_empty());

    // A competition's stuck refunds are written off, and the rest listed as left alone.
    let report = f
        .coordinator
        .write_off_refunds(
            WriteOffTarget::Competition(f.competition_id),
            "the player never sent a registration",
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        report
            .written_off
            .iter()
            .map(|written| written.ticket_id)
            .collect::<Vec<_>>(),
        vec![stuck.id]
    );
    assert_eq!(report.refused.len(), 1);
    assert_eq!(report.refused[0].ticket_id, paid.id);

    let forced = f.write_off(&paid, true).await.unwrap();
    assert_eq!(forced.written_off[0].refund_state.as_deref(), Some("paid"));
    let told = f
        .coordinator
        .get_ticket_refund("player".into(), f.competition_id, paid.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((told.state.as_str(), told.written_off), ("paid", true));
    let other = f
        .coordinator
        .get_ticket_refund("player".into(), f.competition_id, stuck.id)
        .await
        .unwrap()
        .unwrap();
    assert!(other.written_off);
    f.clean_up().await;
    assert_eq!(
        f.refund(&paid).await.unwrap().state,
        ArkRefundState::Paid,
        "cleanup leaves a written-off refund where it stopped"
    );
    assert_eq!(f.ln.payments_sent(), 1);

    let blank = f
        .coordinator
        .write_off_refunds(WriteOffTarget::Ticket(paid.id), "  ", true)
        .await
        .unwrap_err();
    assert!(blank.to_string().contains("reason"), "{blank}");
    f.database.close().await.unwrap();
}
