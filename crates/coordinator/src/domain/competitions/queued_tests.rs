//! Queued competitions: creation, tickets made on demand, and forming pools at kickoff.
//!
//! The database tests run on a real migrated database with the mock oracle and chain; Keymeld is
//! disabled, so a kickoff forms pools but leaves their sessions for the pools to make.

use super::queued::{self, CompetitionKind, CreateQueuedCompetition};
use super::queued_store::QueuedReservation;
use super::*;
use crate::{
    config::KeymeldSettings,
    infra::{
        bitcoin::BlockSummary,
        bitcoin_mock::MockBitcoinClient,
        db::{DBConnection, DatabasePoolConfig, DatabaseType},
        keymeld::KeymeldService,
        lightning_mock::MockLnClient,
        lnurl_mock::MockLnurlPay,
        oracle::{Oracle, WeatherChoices},
        oracle_mock::MockOracle,
    },
};
use bitcoin::{hashes::Hash, BlockHash, Network};
use coordinator_escrow::pools::{self, Formation, PoolRules};
use dlctix::bitcoin::FeeRate;
use std::sync::Arc;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub(super) fn request(start: OffsetDateTime) -> CreateQueuedCompetition {
    CreateQueuedCompetition {
        id: Uuid::now_v7(),
        signing_date: start + Duration::days(2),
        start_observation_date: start,
        end_observation_date: start + Duration::DAY,
        locations: vec!["KORD".into(), "KSAW".into()],
        number_of_values_per_entry: 2,
        entry_fee: 5_000,
        coordinator_fee: CoordinatorFee::whole_percent(3),
        relative_locktime_block_delta: Some(72),
        min_players: 2,
        max_pool_size: 25,
        max_entries: None,
        max_entries_per_player: 1,
        number_of_places_win: 1,
    }
}

#[test]
fn competition_kinds_round_trip_their_stored_names() {
    for kind in [
        CompetitionKind::Single,
        CompetitionKind::Queued,
        CompetitionKind::Pool,
    ] {
        assert_eq!(CompetitionKind::parse(kind.as_str()), Some(kind));
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            serde_json::json!(kind.as_str())
        );
    }
    assert_eq!(CompetitionKind::parse("other"), None);
    assert_eq!(CompetitionKind::default(), CompetitionKind::Single);
}

#[test]
fn queued_creation_checks_pool_rules_the_cap_and_registration_length() {
    let now = OffsetDateTime::now_utc();
    let valid = request(now + Duration::hours(6));
    let (rules, cap) = valid.settings().unwrap();
    assert_eq!((rules.min_players(), rules.max_players()), (2, 25));
    assert_eq!(cap, queued::DEFAULT_MAX_ENTRIES);
    valid.check_registration(now).unwrap();

    type Change = fn(&mut CreateQueuedCompetition);
    let invalid: &[(&str, Change)] = &[
        ("minimum below two", |r| r.min_players = 1),
        ("maximum above 25", |r| r.max_pool_size = 26),
        ("range too narrow", |r| {
            r.min_players = 13;
            r.max_pool_size = 24
        }),
        ("cap below a pool", |r| {
            r.min_players = 5;
            r.max_pool_size = 9;
            r.max_entries = Some(4)
        }),
        ("cap too large", |r| {
            r.max_entries = Some(queued::MAX_ENTRIES + 1)
        }),
        ("no stake", |r| r.entry_fee = 0),
        ("no places", |r| r.number_of_places_win = 0),
        ("three places", |r| {
            r.number_of_places_win = 3;
            r.max_pool_size = 20
        }),
        ("two places in pools above 20", |r| {
            r.number_of_places_win = 2
        }),
    ];
    for (name, change) in invalid {
        let mut candidate = request(now + Duration::hours(6));
        change(&mut candidate);
        assert!(candidate.settings().is_err(), "{name}");
    }
    let mut capped = request(now + Duration::hours(6));
    capped.max_entries = Some(40);
    assert_eq!(capped.settings().unwrap().1, 40);
    let late = request(now + Duration::days(7));
    assert!(late.check_registration(now).is_err());

    // The default competition: one pool of up to 20, paying two places from ten players.
    let mut default = request(now + Duration::hours(6));
    default.max_pool_size = 20;
    default.max_entries = Some(20);
    default.number_of_places_win = 2;
    let (rules, cap) = default.settings().unwrap();
    assert_eq!((rules.max_players(), cap), (20, 20));
    assert_eq!(default.largest_pool_places(), 2);
    let event = default.reference_event().unwrap();
    assert_eq!(
        (event.total_allowed_entries, event.number_of_places_win),
        (20, 2)
    );
    event.validate_oracle_settings().unwrap();
    // Pools of at most nine never pay a second place.
    default.max_pool_size = 9;
    assert_eq!(default.largest_pool_places(), 1);
}

#[test]
fn the_reference_event_has_one_winner_lines_and_room_for_the_largest_pool() {
    let request = request(OffsetDateTime::now_utc() + Duration::hours(6));
    let event = request.reference_event().unwrap();
    assert_eq!(event.id, request.id);
    assert_eq!(event.total_allowed_entries, 25);
    assert_eq!(event.number_of_places_win, 1);
    assert_eq!(event.total_competition_pool, 25 * 5_000);
    assert_eq!(
        event.scoring_rules,
        Some(crate::infra::oracle::ScoringRules::Lines)
    );
    assert!(event.unlisted);
    event.validate_oracle_settings().unwrap();

    let pool = queued::pool_event(&event, Uuid::now_v7(), 7, 5_000, 1).unwrap();
    assert_eq!(pool.total_allowed_entries, 7);
    assert_eq!(pool.total_competition_pool, 35_000);
    assert_eq!(pool.locations, event.locations);
    assert_eq!(pool.signing_date, event.signing_date);
    assert_eq!(pool.scoring_rules, event.scoring_rules);
    assert_eq!(pool.coordinator_fee, event.coordinator_fee);
    assert_ne!(pool.id, event.id);
    pool.validate_oracle_settings().unwrap();
}

#[test]
fn a_queued_entry_id_must_be_a_recent_uuidv7() {
    let now = OffsetDateTime::now_utc();
    queued::check_entry_id(Uuid::now_v7(), now).unwrap();
    assert!(
        queued::check_entry_id(uuid::Builder::from_random_bytes([7; 16]).into_uuid(), now).is_err()
    );
    let at = |when: OffsetDateTime| {
        Uuid::new_v7(uuid::Timestamp::from_unix(
            uuid::NoContext,
            when.unix_timestamp() as u64,
            when.nanosecond(),
        ))
    };
    queued::check_entry_id(at(now - Duration::minutes(30)), now).unwrap();
    assert!(queued::check_entry_id(at(now - Duration::hours(2)), now).is_err());
    assert!(queued::check_entry_id(at(now + Duration::minutes(10)), now).is_err());
}

/// A queued competition's reference event is off the oracle's list, and its pools copy it, but
/// the public lists show them. A single competition's flag keeps it off the lists.
#[test]
fn queues_and_pools_are_listed_though_their_events_are_unlisted() {
    let event = request(OffsetDateTime::now_utc() + Duration::hours(6))
        .reference_event()
        .unwrap();
    let mut queue = Competition::new(&event);
    queue.kind = CompetitionKind::Queued;
    assert!(queue.is_listed());
    let mut pool =
        Competition::new(&queued::pool_event(&event, Uuid::now_v7(), 7, 5_000, 1).unwrap());
    pool.kind = CompetitionKind::Pool;
    assert!(pool.event_submission.unlisted && pool.is_listed());

    assert!(!Competition::new(&event).is_listed());
    let listed = CreateEvent {
        unlisted: false,
        ..event
    };
    assert!(Competition::new(&listed).is_listed());
}

#[test]
fn a_queued_competition_takes_entries_until_its_pools_form() {
    let start = OffsetDateTime::now_utc() + Duration::hours(1);
    let mut competition = Competition::new(&request(start).reference_event().unwrap());
    competition.kind = CompetitionKind::Queued;
    // Its entry count never fills it: pools take as many as come.
    competition.total_entries = 40;
    competition.total_paid_entries = 40;
    assert_eq!(competition.get_state(), CompetitionState::Created);
    competition
        .require_ticket_admission(start - Duration::minutes(2))
        .unwrap();
    assert_eq!(
        crate::domain::leaderboard::Phase::of(&competition, start + Duration::minutes(1)),
        crate::domain::leaderboard::Phase::Live,
        "a queued competition is not unfilled at its start: it forms pools"
    );
    competition.pools_formed_at = Some(start);
    assert_eq!(competition.get_state(), CompetitionState::PoolsFormed);
    assert_eq!(competition.get_state().to_string(), "pools_formed");
    assert!(competition
        .require_entry_admission(start - Duration::minutes(2))
        .is_err());
    competition.cancelled_at = Some(start);
    assert_eq!(competition.get_state(), CompetitionState::Cancelled);
}

fn block(height: u32, time: i64) -> BlockSummary {
    BlockSummary {
        height,
        hash: BlockHash::hash(&height.to_be_bytes()),
        time: time as u32,
    }
}

#[test]
fn the_closing_block_is_the_first_at_or_after_the_close() {
    let close = 1_900_000_000;
    let chain: Vec<_> = (0..40)
        .map(|height| block(height, close - 20 * 600 + i64::from(height) * 600))
        .collect();
    let found = queued::first_block_at_or_after(&chain, close).unwrap();
    assert_eq!(found.height, 20);
    // Header times may run out of order: the lowest height at or after the close wins.
    let mut jumbled = chain.clone();
    jumbled[18].time = close as u32 + 5;
    assert_eq!(
        queued::first_block_at_or_after(&jumbled, close)
            .unwrap()
            .height,
        18
    );
    // A run of headers that starts too close to the close might miss an earlier block.
    assert!(queued::first_block_at_or_after(&chain[15..], close).is_none());
    // Nothing yet at or after the close.
    assert!(queued::first_block_at_or_after(&chain[..20], close).is_none());
}

/// A queued competition in a migrated database, with the coordinator that runs it.
pub(super) struct Queue {
    _directory: TempDir,
    pub(super) db: DBConnection,
    pub(super) coordinator: Coordinator,
    pub(super) oracle: Arc<MockOracle>,
    pub(super) bitcoin: Arc<MockBitcoinClient>,
    pub(super) competition: Competition,
    pub(super) settings: QueueSettings,
}

impl Queue {
    pub(super) async fn new(start: OffsetDateTime, rules: PoolRules, max_entries: u32) -> Self {
        Self::paying(start, rules, max_entries, 1).await
    }

    /// A queue whose pools of ten or more pay `places`.
    pub(super) async fn paying(
        start: OffsetDateTime,
        rules: PoolRules,
        max_entries: u32,
        places: usize,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let oracle = Arc::new(MockOracle::new([7; 32]));
        let bitcoin = Arc::new(MockBitcoinClient::new(Network::Regtest));
        let coordinator = Coordinator::new(
            oracle.clone(),
            CompetitionStore::new(db.clone()),
            bitcoin.clone(),
            Arc::new(MockLnClient::new()),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            Arc::new(
                KeymeldService::new(KeymeldSettings::default(), Uuid::now_v7(), &[1; 32]).unwrap(),
            ),
            None,
            72,
            1,
            "queued-test".into(),
            false,
            1,
        )
        .await
        .unwrap();

        let mut request = request(start);
        request.min_players = rules.min_players();
        request.max_pool_size = rules.max_players();
        request.number_of_places_win = places;
        let event = request.reference_event().unwrap();
        let created = oracle.create_event(event.clone()).await.unwrap();
        let reference = oracle.get_event_terms(&event.id).await.unwrap();
        let terms = queued::build_terms(
            queued::TermsInputs {
                competition_id: event.id,
                network: Network::Regtest,
                market_maker: dlctix::secp::Scalar::from_slice(&[3; 32])
                    .unwrap()
                    .base_point_mul(),
                oracle_key: oracle.public_key().await.unwrap(),
                pool_rules: rules,
                stake_sats: event.entry_fee as u64,
                relative_locktime_block_delta: 72,
                max_fee_rate: FeeRate::from_sat_per_vb_u32(10),
            },
            &reference,
        )
        .unwrap();
        let settings = QueueSettings {
            competition_id: event.id,
            pool_rules: rules,
            stake_sats: terms.stake_sats,
            max_entries,
            terms_digest: terms.digest().unwrap(),
            terms,
        };
        let mut competition = Competition::new(&event);
        competition.kind = CompetitionKind::Queued;
        competition.event_announcement = Some(created.event_announcement);
        coordinator
            .competition_store
            .add_queued_competition(&competition, &settings)
            .await
            .unwrap();
        Self {
            _directory: directory,
            db,
            coordinator,
            oracle,
            bitcoin,
            competition,
            settings,
        }
    }

    pub(super) fn store(&self) -> &CompetitionStore {
        &self.coordinator.competition_store
    }

    /// Blocks every ten minutes, the `close_height`th at the close.
    pub(super) fn chain_closing_at(&self, close_height: u32, tip: u32) {
        let close = self
            .competition
            .event_submission
            .start_observation_date
            .unix_timestamp();
        self.bitcoin.set_blocks(
            (0..=tip)
                .map(|height| {
                    block(
                        height,
                        close + (i64::from(height) - i64::from(close_height)) * 600,
                    )
                })
                .collect(),
        );
    }

    /// A paid ticket of the queue; with `entered`, also its entry, payout policy, funded escrow
    /// and key deposit, which make it complete.
    pub(super) async fn ticket(&self, player: &str, entered: bool) -> Uuid {
        let ticket = Uuid::now_v7();
        let parent = self.competition.id;
        let player = player.to_string();
        let hash = format!("{:064x}", ticket.as_u128());
        let policy = serde_json::to_string(&coordinator_escrow::authorization::PayoutPolicy {
            automatic_lightning_address: Some(format!("{player}@example.org")),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            contract_terms: String::new(),
            ark_escrow: Some(coordinator_escrow::authorization::ArkEscrowPolicy {
                escrow_tap_tree: "00".into(),
                max_fee_sats: 150,
                max_refund_fee_sats: 100,
                checkpoint_exit_script: "00".into(),
            }),
            queued_entry: Some(
                coordinator_escrow::queued::QueuedEntryTerms {
                    terms: self.settings.terms.clone(),
                    entry_id: ticket,
                    ticket_hash: hex::decode(&hash).unwrap().try_into().unwrap(),
                    payout_hash: dlctix::hashlock::sha256(ticket.as_bytes()),
                }
                .to_json()
                .unwrap(),
            ),
        })
        .unwrap();
        let submission = serde_json::to_vec(&AddEventEntry {
            id: ticket,
            event_id: parent,
            expected_observations: vec![WeatherChoices {
                stations: "KORD".into(),
                temp_high: Some(crate::infra::oracle::ValueOptions::Over),
                temp_low: None,
                wind_speed: None,
            }],
        })
        .unwrap();
        self.db
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_by,
                        reserved_at, paid_at, settled_at)
                     VALUES (?, ?, 'preimage', ?, ?, datetime('now'), datetime('now'), datetime('now'))",
                )
                .bind(ticket.to_string())
                .bind(parent.to_string())
                .bind(&hash)
                .bind(&player)
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO ticket_ark_escrows (ticket_id, ticket_hash, escrow_tap_tree,
                        escrow_address, vtxo_outpoint, vtxo_sats, funded_at)
                     VALUES (?, ?, '00', 'address', ?, 5150, 1)",
                )
                .bind(ticket.to_string())
                .bind(&hash)
                .bind(format!("{}:0", "00".repeat(32)))
                .execute(&pool)
                .await?;
                if entered {
                    sqlx::query(
                        "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey,
                            payout_hash, entry_submission)
                         VALUES (?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(ticket.to_string())
                    .bind(parent.to_string())
                    .bind(ticket.to_string())
                    .bind(&player)
                    .bind(format!("key-{ticket}"))
                    .bind(format!("payout-{ticket}"))
                    .bind(submission)
                    .execute(&pool)
                    .await?;
                    sqlx::query(
                        "INSERT INTO entry_payout_policies (entry_id, policy_json) VALUES (?, ?)",
                    )
                    .bind(ticket.to_string())
                    .bind(policy)
                    .execute(&pool)
                    .await?;
                    sqlx::query(
                        "INSERT INTO ticket_keymeld_registrations (ticket_id, ticket_hash,
                            registration_json) VALUES (?, ?, '{}')",
                    )
                    .bind(ticket.to_string())
                    .bind(&hash)
                    .execute(&pool)
                    .await?;
                }
                Ok(())
            })
            .await
            .unwrap();
        ticket
    }

    pub(super) async fn lease(&self) -> Lease {
        self.store()
            .acquire_lease(
                &Lease::competition_resource(self.competition.id),
                "queued-test",
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap()
            .unwrap()
    }

    pub(super) async fn advance(&self) -> Step {
        let lease = self.lease().await;
        self.coordinator
            .advance_competition(self.competition.id, &lease, &Pacing::default())
            .await
            .unwrap()
    }

    pub(super) async fn event_of(&self, ticket: Uuid) -> (Uuid, Option<Uuid>) {
        let row = sqlx::query(
            "SELECT t.event_id AS ticket_event, e.event_id AS entry_event, e.entry_submission
             FROM tickets t LEFT JOIN entries e ON e.ticket_id = t.id WHERE t.id = ?",
        )
        .bind(ticket.to_string())
        .fetch_one(self.db.read())
        .await
        .unwrap();
        use sqlx::Row;
        let ticket_event = Uuid::parse_str(&row.get::<String, _>("ticket_event")).unwrap();
        let entry_event = row
            .get::<Option<String>, _>("entry_event")
            .map(|id| Uuid::parse_str(&id).unwrap());
        if let Some(submission) = row.get::<Option<Vec<u8>>, _>("entry_submission") {
            let submission: AddEventEntry = serde_json::from_slice(&submission).unwrap();
            assert_eq!(
                Some(submission.event_id),
                entry_event,
                "an entry's oracle submission names the competition it is in"
            );
        }
        (ticket_event, entry_event)
    }
}

#[tokio::test]
async fn queued_terms_come_from_the_reference_event_and_are_stored_with_their_digest() {
    let start = OffsetDateTime::now_utc() + Duration::hours(3);
    let queue = Queue::new(start, PoolRules::new(3, 5).unwrap(), 40).await;
    let terms = &queue.settings.terms;
    assert_eq!(terms.competition_id, queue.competition.id);
    assert_eq!(terms.stake_sats, 5_000);
    assert_eq!(terms.number_of_places_win, 1);
    assert_eq!(
        terms.signing_date,
        queue
            .competition
            .event_submission
            .signing_date
            .unix_timestamp()
    );
    assert_eq!(
        Some(terms.expiry),
        queue
            .competition
            .event_announcement
            .as_ref()
            .unwrap()
            .expiry
    );
    assert_eq!(terms.observation.targets, vec!["KORD", "KSAW"]);
    assert_eq!(
        terms.observation.lines.len(),
        6,
        "each station's three metrics"
    );
    let mut sorted = terms.observation.lines.clone();
    sorted.sort_by(|a, b| (&a.target, &a.metric).cmp(&(&b.target, &b.metric)));
    assert_eq!(
        sorted, terms.observation.lines,
        "lines sorted by target, then metric"
    );

    let stored = queue
        .store()
        .queue_settings(queue.competition.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored, queue.settings);
    assert!(queue
        .store()
        .queue_settings(Uuid::now_v7())
        .await
        .unwrap()
        .is_none());

    // The API shows the queue's settings and how many entered, not a seat count.
    let loaded = queue
        .coordinator
        .get_competition(queue.competition.id)
        .await
        .unwrap();
    assert_eq!(loaded.kind, CompetitionKind::Queued);
    let json = serde_json::to_value(&loaded).unwrap();
    assert_eq!(json["kind"], "queued");
    assert_eq!(json["entries"], 0);
    assert_eq!(json["max_entries"], 40);
    assert_eq!(json["stake_sats"], 5_000);
    assert_eq!(
        json["pool_rules"],
        serde_json::json!({"min_players": 3, "max_players": 5})
    );
    assert_eq!(json["pools"], serde_json::json!([]));
    assert_eq!(json["state"], "created");
    assert!(json.get("parent_id").is_none());

    // A stored digest that is not the terms' own is refused.
    let id = queue.competition.id.to_string();
    queue
        .db
        .execute_write(move |pool| async move {
            sqlx::query("UPDATE queued_competitions SET terms_digest = ? WHERE competition_id = ?")
                .bind("00".repeat(32))
                .bind(id)
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(queue
        .store()
        .queue_settings(queue.competition.id)
        .await
        .is_err());
}

#[tokio::test]
async fn queued_tickets_are_made_on_demand_up_to_the_cap() {
    let start = OffsetDateTime::now_utc() + Duration::hours(3);
    let queue = Queue::new(start, PoolRules::new(2, 3).unwrap(), 4).await;
    let store = queue.store();
    let id = queue.competition.id;
    let deadline = queue.competition.ticket_deadline();
    let reserve = |ticket: Uuid, player: &'static str| async move {
        store
            .reserve_queued_ticket(id, ticket, player, 4, 1, deadline)
            .await
            .unwrap()
    };

    let first = Uuid::now_v7();
    let QueuedReservation::Reserved(reserved) = reserve(first, "alice").await else {
        panic!("a ticket is made for the entry");
    };
    assert_eq!(reserved.ticket.id, first);
    assert_eq!(reserved.ticket.competition_id, id);
    assert_eq!(reserved.ticket.reserved_by.as_deref(), Some("alice"));
    assert!(reserved.superseded_payment_hash.is_none());
    let hash = reserved.ticket.hash.clone();
    let original = hash.clone();
    assert_eq!(
        hex::encode(dlctix::hashlock::sha256(
            &hex::decode(&reserved.ticket.encrypted_preimage).unwrap()
        )),
        hash
    );

    // Asking again for the same entry gets the same ticket.
    let QueuedReservation::Reserved(again) = reserve(first, "alice").await else {
        panic!("the same ticket again");
    };
    assert_eq!((again.ticket.id, again.ticket.hash), (first, hash));
    // Nobody else can take it.
    assert!(matches!(
        reserve(first, "bob").await,
        QueuedReservation::Taken
    ));

    // A player holds at most three unpaid tickets at a time.
    for _ in 0..2 {
        assert!(matches!(
            reserve(Uuid::now_v7(), "alice").await,
            QueuedReservation::Reserved(_)
        ));
    }
    assert!(matches!(
        reserve(Uuid::now_v7(), "alice").await,
        QueuedReservation::TooManyUnpaid
    ));
    // No seat count: anyone may enter until the cap of four live tickets.
    assert!(matches!(
        reserve(Uuid::now_v7(), "bob").await,
        QueuedReservation::Reserved(_)
    ));
    assert!(matches!(
        reserve(Uuid::now_v7(), "carol").await,
        QueuedReservation::Full
    ));

    // Tickets stop at the deadline.
    assert!(matches!(
        store
            .reserve_queued_ticket(
                id,
                Uuid::now_v7(),
                "dave",
                100,
                1,
                OffsetDateTime::now_utc() - Duration::seconds(1)
            )
            .await
            .unwrap(),
        QueuedReservation::Closed
    ));

    // An expired unpaid invoice gives the ticket a fresh hash; the old one is superseded.
    let first_id = first.to_string();
    queue
        .db
        .execute_write(move |pool| async move {
            sqlx::query(
                "UPDATE tickets SET payment_request = 'lnbc1', invoice_expires_at = datetime('now', '-1 minute')
                 WHERE id = ?",
            )
            .bind(first_id)
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();
    let QueuedReservation::Reserved(rotated) = reserve(first, "alice").await else {
        panic!("the expired ticket is reserved again");
    };
    assert_eq!(rotated.ticket.id, first);
    assert_ne!(rotated.ticket.hash, original);
    assert!(rotated.ticket.payment_request.is_none());
    assert!(rotated.superseded_payment_hash.is_some());

    // A hold that lapsed stops counting toward the cap. Once someone else takes the place, the
    // lapsed ticket is not held again: the queue is full.
    let first_id = first.to_string();
    queue
        .db
        .execute_write(move |pool| async move {
            sqlx::query(
                "UPDATE tickets SET reserved_at = datetime('now', '-20 minutes'),
                     payment_request = 'lnbc1', invoice_expires_at = datetime('now', '-1 minute')
                 WHERE id = ?",
            )
            .bind(first_id)
            .execute(&pool)
            .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        reserve(Uuid::now_v7(), "carol").await,
        QueuedReservation::Reserved(_)
    ));
    assert!(matches!(
        reserve(first, "alice").await,
        QueuedReservation::Full
    ));
}

/// The default competition takes 20 entries. Players asking at once get exactly 20 tickets: paid
/// and payable tickets count, and the writes are serialized, so a 21st cannot slip in.
#[tokio::test]
async fn concurrent_entries_stop_at_the_cap() {
    let start = OffsetDateTime::now_utc() + Duration::hours(3);
    let queue = Queue::new(start, PoolRules::new(2, 20).unwrap(), 20).await;
    let id = queue.competition.id;
    let deadline = queue.competition.ticket_deadline();
    // Five have paid already.
    for index in 0..5 {
        queue.ticket(&format!("paid{index}"), true).await;
    }
    let attempts = (0..30).map(|index| {
        let store = queue.store().clone();
        async move {
            store
                .reserve_queued_ticket(
                    id,
                    Uuid::now_v7(),
                    &format!("player{index}"),
                    20,
                    1,
                    deadline,
                )
                .await
                .unwrap()
        }
    });
    let results = futures::future::join_all(attempts).await;
    let reserved = results
        .iter()
        .filter(|result| matches!(result, QueuedReservation::Reserved(_)))
        .count();
    let full = results
        .iter()
        .filter(|result| matches!(result, QueuedReservation::Full))
        .count();
    assert_eq!((reserved, full), (15, 15));
}

/// A player who paid for as many entries as the queue allows one player gets no new ticket.
/// Unpaid tickets don't count, so abandoning a payment never locks a player out.
#[tokio::test]
async fn a_player_who_paid_for_their_entries_gets_no_more_tickets() {
    let start = OffsetDateTime::now_utc() + Duration::hours(3);
    let queue = Queue::new(start, PoolRules::new(2, 3).unwrap(), 100).await;
    let store = queue.store();
    let id = queue.competition.id;
    let deadline = queue.competition.ticket_deadline();
    let reserve = |player: &'static str, max_per_player| {
        let store = store.clone();
        async move {
            store
                .reserve_queued_ticket(id, Uuid::now_v7(), player, 100, max_per_player, deadline)
                .await
                .unwrap()
        }
    };

    assert!(matches!(
        reserve("alice", 1).await,
        QueuedReservation::Reserved(_)
    ));
    assert!(matches!(
        reserve("alice", 1).await,
        QueuedReservation::Reserved(_)
    ));
    queue.ticket("alice", true).await;
    assert!(matches!(
        reserve("alice", 1).await,
        QueuedReservation::EntryLimit
    ));
    assert!(matches!(
        reserve("alice", 2).await,
        QueuedReservation::Reserved(_)
    ));
    // Paid without an entry yet still counts: that ticket is the entry on its way.
    queue.ticket("bob", false).await;
    assert!(matches!(
        reserve("bob", 1).await,
        QueuedReservation::EntryLimit
    ));
    assert!(matches!(
        reserve("carol", 1).await,
        QueuedReservation::Reserved(_)
    ));
}

#[tokio::test]
async fn kickoff_forms_pools_from_the_seed_and_moves_their_tickets() {
    let start = OffsetDateTime::now_utc() - Duration::minutes(10);
    let rules = PoolRules::new(2, 3).unwrap();
    let queue = Queue::new(start, rules, 100).await;
    queue.chain_closing_at(20, 30);
    let mut complete = Vec::new();
    for player in ["a", "b", "c", "d", "e", "f", "g"] {
        complete.push(queue.ticket(player, true).await);
    }
    // Paid but never entered: no pool takes it, and it stays to be refunded.
    let leftover = queue.ticket("h", false).await;
    complete.sort_unstable();
    assert_eq!(
        queue
            .store()
            .complete_queued_tickets(queue.competition.id)
            .await
            .unwrap(),
        complete
    );

    assert_eq!(queue.advance().await, Step::Finished);

    let close = BlockHash::hash(&20u32.to_be_bytes());
    let Formation::Pools {
        pools: expected, ..
    } = pools::form(&rules, queue.competition.id, &complete, &close).unwrap()
    else {
        panic!("seven tickets form pools");
    };
    let records = queue
        .store()
        .competition_pools(queue.competition.id)
        .await
        .unwrap();
    assert_eq!(records.len(), 3, "seven players in pools of at most three");
    for (index, (record, mut members)) in records.iter().zip(expected).enumerate() {
        members.sort_unstable();
        assert_eq!(record.pool_index as usize, index);
        assert_eq!(record.members, members, "the pools follow from the seed");
        assert_eq!(record.tickets, complete);
        assert_eq!(record.block_hash, close);
        assert_eq!(record.close_height, 20);
        assert_eq!(record.parent_id, queue.competition.id);

        let pool = queue
            .coordinator
            .get_competition(record.competition_id)
            .await
            .unwrap();
        assert_eq!(pool.kind, CompetitionKind::Pool);
        assert_eq!(pool.parent_id, Some(queue.competition.id));
        assert_eq!(pool.pool_index, Some(record.pool_index));
        assert_eq!(pool.event_submission.id, pool.id);
        assert_eq!(pool.event_submission.total_allowed_entries, members.len());
        assert_eq!(
            pool.event_submission.total_competition_pool,
            members.len() * 5_000
        );
        assert_eq!(pool.total_entries as usize, members.len());
        assert!(pool.escrow_funds_confirmed_at.is_some());
        assert_eq!(
            pool.get_state(),
            CompetitionState::EscrowFundsConfirmed,
            "a pool starts where a filled competition's escrows are confirmed"
        );
        assert!(queue.store().is_ark_funded(pool.id).await.unwrap());
        assert!(queue.store().has_automatic_payouts(pool.id).await.unwrap());
        let json = serde_json::to_value(&pool).unwrap();
        assert_eq!(json["kind"], "pool");
        assert_eq!(json["parent_id"], serde_json::json!(queue.competition.id));
        for member in &members {
            assert_eq!(
                queue.event_of(*member).await,
                (pool.id, Some(pool.id)),
                "a member's ticket and entry move to its pool"
            );
        }
    }
    assert_eq!(
        queue.event_of(leftover).await,
        (queue.competition.id, None),
        "a ticket no pool took stays with the queue"
    );

    let parent = queue
        .coordinator
        .get_competition(queue.competition.id)
        .await
        .unwrap();
    assert!(parent.pools_formed_at.is_some());
    assert_eq!(parent.get_state(), CompetitionState::PoolsFormed);
    let summary = parent.queue.clone().unwrap();
    assert_eq!(summary.entries, 7);
    assert_eq!(
        summary
            .pools
            .iter()
            .map(|pool| pool.players)
            .collect::<Vec<_>>(),
        vec![3, 2, 2]
    );
    let active = queue.store().active_competition_ids().await.unwrap();
    assert!(!active.contains(&queue.competition.id), "the queue is done");
    for record in &records {
        assert!(active.contains(&record.competition_id), "its pools run");
    }
    assert!(
        queue
            .store()
            .get_competitions_pending_cleanup(false)
            .await
            .unwrap()
            .contains(&queue.competition.id),
        "the leftover ticket's escrow is refunded"
    );

    // Running the kickoff again forms nothing more.
    assert_eq!(queue.advance().await, Step::Finished);
    assert_eq!(
        queue
            .store()
            .competition_pools(queue.competition.id)
            .await
            .unwrap(),
        records
    );
    let pools: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM competitions WHERE kind = 'pool'")
        .fetch_one(queue.db.read())
        .await
        .unwrap();
    assert_eq!(pools, 3);

    // The queue finishes once every pool has, and its leftover ticket is still refunded.
    let store = queue.store();
    assert_eq!(
        store.finish_formed_queues().await.unwrap(),
        0,
        "its pools run"
    );
    let now = OffsetDateTime::now_utc();
    let mut pools = Vec::new();
    for record in &records {
        pools.push(store.get_competition(record.competition_id).await.unwrap());
    }
    pools[0].completed_at = Some(now);
    pools[1].completed_at = Some(now);
    // Cancelled after its contract was funded, a pool may still settle.
    pools[2].cancelled_at = Some(now);
    pools[2].funding_confirmed_at = Some(now);
    store.update_competitions(pools.clone()).await.unwrap();
    assert_eq!(store.finish_formed_queues().await.unwrap(), 0);
    pools[2].funding_confirmed_at = None;
    store
        .update_competitions(vec![pools[2].clone()])
        .await
        .unwrap();
    assert_eq!(store.finish_formed_queues().await.unwrap(), 1);
    assert_eq!(store.finish_formed_queues().await.unwrap(), 0, "once");
    let parent = store.get_competition(queue.competition.id).await.unwrap();
    assert!(parent.pools_finished_at.is_some());
    assert_eq!(parent.get_state(), CompetitionState::PoolsFinished);
    assert_eq!(parent.get_state().to_string(), "pools_finished");
    assert!(
        store
            .get_competitions_pending_cleanup(false)
            .await
            .unwrap()
            .contains(&queue.competition.id),
        "the leftover ticket's escrow is still refunded"
    );
    assert!(!store
        .active_competition_ids()
        .await
        .unwrap()
        .contains(&queue.competition.id));
}

#[tokio::test]
async fn kickoff_waits_for_the_close_and_its_block() {
    let later = OffsetDateTime::now_utc() + Duration::hours(1);
    let queue = Queue::new(later, PoolRules::new(2, 3).unwrap(), 100).await;
    queue.ticket("a", true).await;
    queue.ticket("b", true).await;
    assert_eq!(
        queue.advance().await,
        Step::Next(Wait::Until(later)),
        "registration is open until the start"
    );

    let start = OffsetDateTime::now_utc() - Duration::minutes(1);
    let queue = Queue::new(start, PoolRules::new(2, 3).unwrap(), 100).await;
    queue.ticket("a", true).await;
    queue.ticket("b", true).await;
    // Observations started, but nothing cancels a queued competition as unfilled.
    let lease = queue.lease().await;
    assert!(!queue
        .store()
        .cancel_unfilled_at_deadline(&queue.competition, &lease)
        .await
        .unwrap());
    // The chain's tip is still before the close.
    queue.chain_closing_at(40, 30);
    assert!(matches!(queue.advance().await, Step::Next(Wait::Until(_))));
    assert!(queue
        .store()
        .competition_pools(queue.competition.id)
        .await
        .unwrap()
        .is_empty());
    queue.chain_closing_at(20, 30);
    assert_eq!(queue.advance().await, Step::Finished);
    assert_eq!(
        queue
            .store()
            .competition_pools(queue.competition.id)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_queue_too_small_for_a_pool_is_cancelled_and_refunded() {
    let start = OffsetDateTime::now_utc() - Duration::minutes(10);
    let queue = Queue::new(start, PoolRules::new(3, 5).unwrap(), 100).await;
    queue.chain_closing_at(20, 30);
    let entered = queue.ticket("a", true).await;
    queue.ticket("b", true).await;
    queue.ticket("c", false).await;

    assert_eq!(queue.advance().await, Step::Finished);
    let parent = queue
        .coordinator
        .get_competition(queue.competition.id)
        .await
        .unwrap();
    assert!(parent.is_cancelled());
    assert!(parent.pools_formed_at.is_none());
    assert_eq!(parent.get_state(), CompetitionState::Cancelled);
    assert!(queue
        .store()
        .competition_pools(queue.competition.id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(queue.event_of(entered).await.0, queue.competition.id);
    assert!(queue
        .store()
        .get_competitions_pending_cleanup(false)
        .await
        .unwrap()
        .contains(&queue.competition.id));
    // Refunds are the only thing left.
    assert!(!queue
        .store()
        .active_competition_ids()
        .await
        .unwrap()
        .contains(&queue.competition.id));
}

#[tokio::test]
async fn a_formation_that_does_not_match_the_tickets_writes_nothing() {
    let start = OffsetDateTime::now_utc() - Duration::minutes(10);
    let queue = Queue::new(start, PoolRules::new(2, 3).unwrap(), 100).await;
    let a = queue.ticket("a", true).await;
    let b = queue.ticket("b", true).await;
    let stranger = Uuid::now_v7();
    let lease = queue.lease().await;
    let pool_id = Uuid::now_v7();
    let formation = super::queued_store::PoolFormation {
        parent_id: queue.competition.id,
        close_height: 1,
        block_hash: BlockHash::all_zeros(),
        tickets: vec![a, b, stranger],
        pools: vec![super::queued_store::NewPool {
            competition_id: pool_id,
            pool_index: 0,
            members: vec![a, b, stranger],
            event_submission: queued::pool_event(
                &queue.competition.event_submission,
                pool_id,
                3,
                5_000,
                1,
            )
            .unwrap(),
        }],
        formed_at: OffsetDateTime::now_utc(),
    };
    assert!(queue
        .store()
        .form_queued_pools(formation, &lease)
        .await
        .is_err());
    assert_eq!(queue.event_of(a).await.0, queue.competition.id);
    assert!(queue
        .store()
        .competition_pools(queue.competition.id)
        .await
        .unwrap()
        .is_empty());
    let parent = queue
        .store()
        .get_competition(queue.competition.id)
        .await
        .unwrap();
    assert!(parent.pools_formed_at.is_none());
    let pools: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM competitions WHERE kind = 'pool'")
        .fetch_one(queue.db.read())
        .await
        .unwrap();
    assert_eq!(pools, 0);
}

#[tokio::test]
async fn a_pool_that_cannot_get_its_session_retries_then_fails_to_be_refunded() {
    let start = OffsetDateTime::now_utc() - Duration::minutes(10);
    let queue = Queue::new(start, PoolRules::new(2, 3).unwrap(), 100).await;
    queue.chain_closing_at(20, 30);
    for player in ["a", "b"] {
        queue.ticket(player, true).await;
    }
    assert_eq!(queue.advance().await, Step::Finished);
    let pool_id = queue
        .store()
        .competition_pools(queue.competition.id)
        .await
        .unwrap()[0]
        .competition_id;
    let advance_pool = || async {
        let lease = queue
            .store()
            .acquire_lease(
                &Lease::competition_resource(pool_id),
                "queued-test",
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap()
            .unwrap();
        queue
            .coordinator
            .advance_competition(pool_id, &lease, &Pacing::default())
            .await
    };

    // Keymeld is unavailable here, so the pool has no session: it tries again later.
    assert!(advance_pool().await.is_err());
    let pool = queue.store().get_competition(pool_id).await.unwrap();
    assert!(pool.failed_at.is_none());
    assert!(
        pool.event_announcement.is_none(),
        "no event before its session"
    );

    // Past the deadline it fails, and cleanup refunds its escrows.
    let long_ago = (OffsetDateTime::now_utc() - Duration::hours(2))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let id = pool_id.to_string();
    queue
        .db
        .execute_write(move |pool| async move {
            sqlx::query("UPDATE competitions SET created_at = ? WHERE id = ?")
                .bind(long_ago)
                .bind(id)
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        advance_pool().await.unwrap(),
        Step::Next(Wait::Until(_))
    ));
    let pool = queue.store().get_competition(pool_id).await.unwrap();
    assert!(pool.failed_at.is_some());
    assert!(queue
        .store()
        .get_competitions_pending_cleanup(false)
        .await
        .unwrap()
        .contains(&pool_id));
}
