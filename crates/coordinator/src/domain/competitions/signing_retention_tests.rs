use super::*;
use crate::{
    config::KeymeldSettings,
    infra::{
        bitcoin_mock::MockBitcoinClient,
        db::{DBConnection, DatabasePoolConfig, DatabaseType},
        keymeld::create_keymeld_service,
        lightning_mock::MockLnClient,
        lnurl_mock::MockLnurlPay,
        oracle_mock::MockOracle,
    },
};
use bitcoin::Network;
use sha2::{Digest, Sha256};

const DAY: i64 = 24 * 60 * 60;

/// A competition with one paid entry, a stored Keymeld session, and that session's signing
/// state.
struct Seeded {
    id: Uuid,
    ticket: Uuid,
    entry: Uuid,
    session: String,
}

struct Fixture {
    coordinator: Coordinator,
    db: DBConnection,
    /// The clock every time below counts from, in Unix seconds.
    now: i64,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        Self::with(SigningStateSettings::default()).await
    }

    async fn with(settings: SigningStateSettings) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let keymeld = create_keymeld_service(
            KeymeldSettings::default(),
            Uuid::now_v7(),
            &[1; 32],
            db.clone(),
        )
        .unwrap();
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(db.clone()),
            Arc::new(MockBitcoinClient::new(Network::Regtest)),
            Arc::new(MockLnClient::new()),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            keymeld,
            None,
            72,
            1,
            "signing-retention-test".into(),
            false,
            1,
        )
        .await
        .unwrap()
        .with_signing_state(settings)
        .unwrap();
        Self {
            coordinator,
            db,
            now: OffsetDateTime::now_utc().unix_timestamp(),
            _directory: directory,
        }
    }

    fn store(&self) -> &CompetitionStore {
        &self.coordinator.competition_store
    }

    /// `now + offset` as the coordinator stores times: RFC 3339, quoted for SQL.
    fn at(&self, offset: i64) -> String {
        let time = OffsetDateTime::from_unix_timestamp(self.now + offset)
            .unwrap()
            .format(&Rfc3339)
            .unwrap();
        format!("'{time}'")
    }

    async fn sql(&self, sql: impl Into<String>, binds: Vec<String>) {
        let sql = sql.into();
        self.db
            .execute_write(move |pool| async move {
                let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
                for bind in binds {
                    query = query.bind(bind);
                }
                query.execute(&pool).await?;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// A competition whose stored session is its own id, as a single competition's and a
    /// pool's are, with a format-`format` checkpoint of `parts` parts and nodes.
    async fn competition(&self, format: i64, parts: i64) -> Seeded {
        let id = Uuid::now_v7();
        self.seed(id, &id.to_string(), format, parts).await
    }

    /// A competition whose stored session names `session`, as a refund session's does, and
    /// that session's signing state, unless another competition stored it already. Its ticket
    /// was paid and settled 60 days before `now`, before anything the tests do.
    async fn seed(&self, id: Uuid, session: &str, format: i64, parts: i64) -> Seeded {
        let (ticket, entry) = (Uuid::now_v7(), Uuid::now_v7());
        // The stored session names its session first; the rest is opaque to retention.
        let stored = serde_json::to_vec(&serde_json::json!({
            "session_id": session,
            "encrypted_session_secret": "sealed",
            "aggregate_key": "02aa",
        }))
        .unwrap();
        let (competition, session_id) = (id.to_string(), session.to_owned());
        let created = self.at(-10 * DAY);
        self.db
            .execute_write(move |pool| async move {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "INSERT INTO competitions (id, created_at, event_submission, keymeld_session)
                     VALUES (?, {created}, '{{}}', ?)"
                )))
                .bind(&competition)
                .bind(stored)
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO keymeld_protocol_state (session_id, version, encrypted_state, format)
                     VALUES (?, 12, 'sealed manifest or root', ?)
                     ON CONFLICT(session_id) DO NOTHING",
                )
                .bind(&session_id)
                .bind(format)
                .execute(&pool)
                .await?;
                sqlx::query(
                    "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
                     INSERT INTO keymeld_protocol_parts (session_id, digest, body)
                     SELECT ?2, randomblob(32), zeroblob(1500) FROM n WHERE i <= ?1",
                )
                .bind(parts)
                .bind(&session_id)
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO refund_invoice_authorizations
                         (keygen_session_id, user_id, invoice, authorization)
                     VALUES (?, ?, 'lnbcrt1refund', 'authenticated receipt')",
                )
                .bind(&session_id)
                .bind(Uuid::now_v7().to_string())
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
        let (preimage, hash) = ticket_secret(ticket);
        let paid = self.at(-60 * DAY);
        self.sql(
            format!(
                "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_by, paid_at,
                                      settled_at)
                 VALUES (?, ?, ?, ?, 'player', {paid}, {paid})"
            ),
            vec![ticket.to_string(), id.to_string(), preimage, hash],
        )
        .await;
        self.sql(
            "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey, payout_hash,
                                  entry_submission)
             VALUES (?, ?, ?, 'player', ?, ?, '{}')",
            vec![
                entry.to_string(),
                id.to_string(),
                ticket.to_string(),
                format!("entry-key-{entry}"),
                format!("payout-hash-{entry}"),
            ],
        )
        .await;
        Seeded {
            id,
            ticket,
            entry,
            session: session.to_owned(),
        }
    }

    /// Set the competition's lifecycle columns: `set` is the SET clause.
    async fn update(&self, competition: &Seeded, set: &str) {
        self.sql(
            format!("UPDATE competitions SET {set} WHERE id = ?"),
            vec![competition.id.to_string()],
        )
        .await;
    }

    /// The competition funded, settled and completed at `now + offset`.
    async fn completed(&self, competition: &Seeded, offset: i64) {
        let (funded, completed) = (self.at(offset - DAY), self.at(offset));
        self.update(
            competition,
            &format!(
                "funding_broadcasted_at = {funded}, funding_confirmed_at = {funded},
                 outcome_broadcasted_at = {funded}, completed_at = {completed}"
            ),
        )
        .await;
    }

    /// One retention pass at `now + offset`.
    async fn pass(&self, offset: i64) {
        self.coordinator
            .retire_signing_state_at(self.now + offset)
            .await
            .unwrap();
    }

    /// What a pass at `now + offset` would find.
    async fn survey(&self, offset: i64) -> RetentionSurvey {
        self.coordinator
            .survey_signing_state(self.now + offset - self.coordinator.signing_state.grace_secs())
            .await
            .unwrap()
    }

    /// The session's checkpoint rows, parts and nodes, and refund invoice authorizations.
    async fn stored(&self, session: &str) -> (i64, i64, i64) {
        let read = self.db.read();
        let rows =
            sqlx::query_scalar("SELECT COUNT(*) FROM keymeld_protocol_state WHERE session_id = ?")
                .bind(session)
                .fetch_one(read)
                .await
                .unwrap();
        let parts =
            sqlx::query_scalar("SELECT COUNT(*) FROM keymeld_protocol_parts WHERE session_id = ?")
                .bind(session)
                .fetch_one(read)
                .await
                .unwrap();
        let authorizations = sqlx::query_scalar(
            "SELECT COUNT(*) FROM refund_invoice_authorizations WHERE keygen_session_id = ?",
        )
        .bind(session)
        .fetch_one(read)
        .await
        .unwrap();
        (rows, parts, authorizations)
    }

    /// When the competition was last paid out, if it is fully paid out now.
    async fn paid_out_since(&self, competition: &Seeded) -> Option<i64> {
        self.store()
            .paid_out_since(&[competition.id])
            .await
            .unwrap()
            .get(&competition.id)
            .copied()
    }

    /// The paid-out rule never holds where the lifecycle, cleanup or a recovery record still
    /// sees money.
    async fn assert_agrees_with_lifecycle_cleanup_and_recovery(&self, competition: &Seeded) {
        if self.paid_out_since(competition).await.is_none() {
            return;
        }
        assert!(
            !self
                .store()
                .active_competition_ids()
                .await
                .unwrap()
                .contains(&competition.id),
            "a paid-out competition is still stepped"
        );
        assert!(
            !self
                .store()
                .get_competitions_pending_cleanup(true)
                .await
                .unwrap()
                .contains(&competition.id),
            "cleanup has work for a paid-out competition"
        );
        for row in self
            .store()
            .recovery_ticket_rows(competition.id)
            .await
            .unwrap()
        {
            assert!(
                !row.money_held,
                "a recovery record of a paid-out competition holds money"
            );
        }
    }
}

/// A ticket's preimage, hex, and its hash, as a paid ticket stores them.
fn ticket_secret(ticket: Uuid) -> (String, String) {
    let preimage = [ticket.as_bytes().as_slice(), ticket.as_bytes().as_slice()].concat();
    (
        hex::encode(&preimage),
        hex::encode(Sha256::digest(&preimage)),
    )
}

const KEPT: (i64, i64, i64) = (1, 40, 1);
const GONE: (i64, i64, i64) = (0, 0, 0);

/// Competitions whose contract or money can still move keep their state however long they
/// wait: running ones, a failed one not yet cancelled, a cancelled one whose contract holds the
/// pot on chain or whose funding was broadcast, a cancelled pool an Arkade batch funded (with
/// or without the funding recorded on the competition), an expired one whose outputs are not
/// settled, and a queued competition with a pool still running.
#[tokio::test]
async fn unfinished_competitions_keep_their_signing_state() {
    let fixture = Fixture::new().await;
    let (long_ago, then) = (fixture.at(-30 * DAY), fixture.at(-29 * DAY));
    let mut unfinished = Vec::new();
    for (set, batch) in [
        ("created_at = created_at".to_string(), false),
        (
            format!("funding_confirmed_at = {long_ago}, awaiting_attestation_at = {then}"),
            false,
        ),
        (format!("failed_at = {long_ago}"), false),
        (
            format!("funding_confirmed_at = {long_ago}, failed_at = {then}, cancelled_at = {then}"),
            false,
        ),
        (
            format!("funding_broadcasted_at = {long_ago}, cancelled_at = {then}"),
            false,
        ),
        (
            format!("funding_broadcasted_at = {long_ago}, cancelled_at = {then}"),
            true,
        ),
        // The batch funded the pool, and the kickoff failed before it recorded the funding:
        // only a kickoff that Keymeld signs again can resume it.
        (
            format!("failed_at = {long_ago}, cancelled_at = {then}"),
            true,
        ),
        (
            format!(
                "funding_confirmed_at = {long_ago}, expiry_broadcasted_at = {then},
                 outcome_broadcasted_at = {then}"
            ),
            false,
        ),
        (
            format!("kind = 'queued', pools_formed_at = {long_ago}"),
            false,
        ),
    ] {
        let competition = fixture.competition(3, 40).await;
        fixture.update(&competition, &set).await;
        if batch {
            fixture
                .sql(
                    "INSERT INTO ark_funded_competitions (event_id, commitment_tx)
                     VALUES (?, 'batch')",
                    vec![competition.id.to_string()],
                )
                .await;
        }
        unfinished.push((competition, set));
    }

    for offset in [0, 3 * DAY, 30 * DAY] {
        fixture.pass(offset).await;
    }
    let survey = fixture.survey(30 * DAY).await;
    for (competition, set) in &unfinished {
        assert_eq!(fixture.paid_out_since(competition).await, None, "{set}");
        assert!(!survey.waiting.contains(&competition.id), "{set}");
        assert!(
            !survey.due_competitions().contains(&competition.id),
            "{set}"
        );
        assert_eq!(fixture.stored(&competition.session).await, KEPT, "{set}");
    }
}

/// A competition that just finished keeps its state through the retention period, counted
/// from when it was last paid out, and loses it, whole, at the end.
#[tokio::test]
async fn a_finished_competition_keeps_its_state_for_the_retention_period_then_loses_it() {
    let fixture = Fixture::new().await;
    let finished = fixture.competition(3, 40).await;
    let bystander = fixture.competition(3, 40).await;
    fixture.completed(&finished, 0).await;
    fixture
        .assert_agrees_with_lifecycle_cleanup_and_recovery(&finished)
        .await;
    assert_eq!(fixture.paid_out_since(&finished).await, Some(fixture.now));

    for offset in [0, DAY, 3 * DAY - 1] {
        let survey = fixture.survey(offset).await;
        assert!(survey.waiting.contains(&finished.id), "{offset}");
        assert!(survey.due.is_empty(), "{offset}");
        fixture.pass(offset).await;
        assert_eq!(fixture.stored(&finished.session).await, KEPT, "{offset}");
    }

    let survey = fixture.survey(3 * DAY).await;
    assert!(survey.waiting.is_empty());
    assert_eq!(
        survey.due,
        vec![(
            fixture.now,
            StoredSigningState {
                competition_id: finished.id,
                session: SessionId::parse(&finished.session).unwrap(),
            }
        )]
    );
    fixture.pass(3 * DAY).await;
    assert_eq!(fixture.stored(&finished.session).await, GONE);
    let survey = fixture.survey(3 * DAY).await;
    assert!(survey.waiting.is_empty() && survey.due.is_empty());
    // The stored session stays: the contract API reads it, and refunds find it.
    let kept: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT keymeld_session FROM competitions WHERE id = ?")
            .bind(finished.id.to_string())
            .fetch_one(fixture.db.read())
            .await
            .unwrap();
    assert!(kept.is_some());
    // Nothing of another competition goes.
    assert_eq!(fixture.stored(&bystander.session).await, KEPT);
    fixture.pass(10 * DAY).await;
    assert_eq!(fixture.stored(&bystander.session).await, KEPT);
}

/// A competition paid out longer ago than the period, as every finished one is when retention
/// first runs, loses its state on the first pass.
#[tokio::test]
async fn a_competition_paid_out_long_ago_loses_its_state_on_the_first_pass() {
    let fixture = Fixture::new().await;
    let finished = fixture.competition(2, 40).await;
    fixture.completed(&finished, -10 * DAY).await;
    fixture.pass(0).await;
    assert_eq!(fixture.stored(&finished.session).await, GONE);
}

/// A configured period replaces three days.
#[tokio::test]
async fn the_retention_period_is_configurable() {
    let fixture = Fixture::with(SigningStateSettings {
        delete_paid_out: true,
        paid_out_retention_days: 10,
    })
    .await;
    let finished = fixture.competition(3, 40).await;
    fixture.completed(&finished, 0).await;
    fixture.pass(3 * DAY).await;
    fixture.pass(10 * DAY - 1).await;
    assert_eq!(fixture.stored(&finished.session).await, KEPT);
    fixture.pass(10 * DAY).await;
    assert_eq!(fixture.stored(&finished.session).await, GONE);
}

/// With deletion off nothing is deleted, however long a competition is paid out; the pass
/// still finds what is due, for the gauges.
#[tokio::test]
async fn nothing_is_deleted_while_deletion_is_off() {
    let fixture = Fixture::with(SigningStateSettings {
        delete_paid_out: false,
        ..SigningStateSettings::default()
    })
    .await;
    let finished = fixture.competition(3, 40).await;
    fixture.completed(&finished, 0).await;
    for offset in [0, 3 * DAY, 365 * DAY] {
        fixture.pass(offset).await;
    }
    assert_eq!(fixture.stored(&finished.session).await, KEPT);
    assert_eq!(
        fixture.survey(365 * DAY).await.due_competitions(),
        BTreeSet::from([finished.id])
    );
}

/// A competition cancelled before its contract held the pot keeps its state while any escrow
/// refund is open, since a refund registers every paid player again; a written-off refund
/// counts as open. Once every refund settled, the period starts from the last settlement.
#[tokio::test]
async fn a_refunded_competition_keeps_its_state_until_every_refund_settled() {
    let fixture = Fixture::new().await;
    let refunded = fixture.competition(3, 40).await;
    let other_ticket = Uuid::now_v7();
    fixture
        .update(&refunded, &format!("cancelled_at = {}", fixture.at(0)))
        .await;
    let (preimage, hash) = ticket_secret(other_ticket);
    fixture
        .sql(
            format!(
                "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, paid_at, settled_at)
                 VALUES (?, ?, ?, ?, {paid}, {paid})",
                paid = fixture.at(-DAY)
            ),
            vec![
                other_ticket.to_string(),
                refunded.id.to_string(),
                preimage,
                hash,
            ],
        )
        .await;
    for ticket in [refunded.ticket, other_ticket] {
        fixture
            .sql(
                "INSERT INTO ticket_ark_escrows (ticket_id, ticket_hash, escrow_tap_tree,
                                                 escrow_address, vtxo_outpoint, vtxo_sats, funded_at)
                 SELECT id, hash, '00', 'tark1escrow', 'aa:0', 5000, 1 FROM tickets WHERE id = ?",
                vec![ticket.to_string()],
            )
            .await;
    }
    let refund = |ticket: Uuid, state: &'static str, updated: i64| {
        let fixture = &fixture;
        async move {
            fixture
                .sql(
                    "INSERT INTO ticket_ark_refunds (ticket_id, refund_id, invoice, payment_hash,
                                                     fee_sats, state, created_at, updated_at)
                     VALUES (?, 'refund', 'lnbc', 'hash', 0, ?, 1, CAST(? AS INTEGER))
                     ON CONFLICT(ticket_id) DO UPDATE
                     SET state = excluded.state, updated_at = excluded.updated_at",
                    vec![
                        ticket.to_string(),
                        state.to_owned(),
                        (fixture.now + updated).to_string(),
                    ],
                )
                .await;
        }
    };

    // Neither escrow is refunded yet.
    fixture.pass(0).await;
    assert_eq!(fixture.paid_out_since(&refunded).await, None);
    // One refund settled, the other is under way.
    refund(refunded.ticket, "settled", DAY).await;
    refund(other_ticket, "submitted", DAY).await;
    fixture.pass(DAY).await;
    assert_eq!(fixture.paid_out_since(&refunded).await, None);
    // The other is written off: the player recovers it alone, and it still counts as open.
    fixture
        .sql(
            "DELETE FROM ticket_ark_refunds WHERE ticket_id = ?",
            vec![other_ticket.to_string()],
        )
        .await;
    fixture
        .sql(
            "INSERT INTO ticket_ark_refund_write_offs (ticket_id, ticket_hash, reason, written_off_at)
             SELECT id, hash, 'stuck', 1 FROM tickets WHERE id = ?",
            vec![other_ticket.to_string()],
        )
        .await;
    fixture.pass(2 * DAY).await;
    assert_eq!(fixture.paid_out_since(&refunded).await, None);
    fixture.pass(30 * DAY).await;
    assert_eq!(fixture.stored(&refunded.session).await, KEPT);

    // Its refund settles after all.
    refund(other_ticket, "settled", 31 * DAY).await;
    fixture
        .assert_agrees_with_lifecycle_cleanup_and_recovery(&refunded)
        .await;
    assert_eq!(
        fixture.paid_out_since(&refunded).await,
        Some(fixture.now + 31 * DAY)
    );
    fixture.pass(34 * DAY - 1).await;
    assert_eq!(fixture.stored(&refunded.session).await, KEPT);
    fixture.pass(34 * DAY).await;
    assert_eq!(fixture.stored(&refunded.session).await, GONE);
}

/// A contract that expired settles its refund outcome before it completes: until then its
/// state stays, and the period starts once it completed.
#[tokio::test]
async fn an_expired_contract_keeps_its_state_until_its_refund_outputs_settle() {
    let fixture = Fixture::new().await;
    let expired = fixture.competition(2, 40).await;
    let funded = fixture.at(-DAY);
    fixture
        .update(
            &expired,
            &format!(
                "funding_broadcasted_at = {funded}, funding_confirmed_at = {funded},
                 expiry_broadcasted_at = {funded}, outcome_broadcasted_at = {funded}"
            ),
        )
        .await;
    fixture.pass(0).await;
    fixture.pass(5 * DAY).await;
    assert_eq!(fixture.paid_out_since(&expired).await, None);
    assert_eq!(fixture.stored(&expired.session).await, KEPT);

    fixture
        .update(&expired, &format!("completed_at = {}", fixture.at(6 * DAY)))
        .await;
    fixture
        .assert_agrees_with_lifecycle_cleanup_and_recovery(&expired)
        .await;
    fixture.pass(9 * DAY - 1).await;
    assert_eq!(fixture.stored(&expired.session).await, KEPT);
    fixture.pass(9 * DAY).await;
    assert_eq!(fixture.stored(&expired.session).await, GONE);
}

/// Every way money can still move keeps a completed competition's state, and each one alone
/// is enough. Closing it starts the period from when it closed.
#[tokio::test]
async fn every_open_payment_keeps_a_finished_competitions_state() {
    let fixture = Fixture::new().await;
    // What opens it, what closes it at ?2 (an RFC 3339 time, or Unix seconds where the column
    // keeps those), and whether the close is recorded with a time.
    let cases: [(&str, &str, &str, bool); 8] = [
        (
            "a payout job under way",
            "INSERT INTO payout_jobs (id, entry_id, request_json, created_at, retry_at)
             VALUES ('job-' || ?1, ?1, '{}', 1, 1)",
            "UPDATE payout_jobs SET completed_at = CAST(?2 AS INTEGER) WHERE entry_id = ?1",
            true,
        ),
        (
            "a Lightning payout in flight",
            "INSERT INTO payouts (id, entry_id, payout_payment_request, payout_amount_sats,
                                  initiated_at)
             VALUES ('payout-' || ?1, ?1, 'lnbc', 5000, '2026-01-01T00:00:00Z')",
            "UPDATE payouts SET succeed_at = ?2 WHERE entry_id = ?1",
            true,
        ),
        (
            "a payout an operator holds",
            "INSERT INTO payout_holds (entry_id, payment_hash, amount_sats, reason, held_at)
             VALUES (?1, 'hash', 5000, 'restore', '2026-01-01T00:00:00Z')",
            "UPDATE payout_holds SET released_at = ?2 WHERE entry_id = ?1",
            true,
        ),
        (
            "a winner owed, then paid",
            "INSERT INTO owed_winners (entry_id, competition_id, amount_sats, owed_since)
             SELECT id, event_id, 5000, '2026-01-01T00:00:00Z' FROM entries WHERE id = ?1",
            "UPDATE owed_winners SET settled_at = ?2 WHERE entry_id = ?1",
            true,
        ),
        (
            "a winner owed, then claiming on chain",
            "INSERT INTO owed_winners (entry_id, competition_id, amount_sats, owed_since)
             SELECT id, event_id, 5000, '2026-01-01T00:00:00Z' FROM entries WHERE id = ?1",
            "UPDATE owed_winners SET claimed_on_chain_at = ?2 WHERE entry_id = ?1",
            true,
        ),
        (
            "a held Lightning payment",
            "UPDATE tickets SET settled_at = NULL
             WHERE id = (SELECT ticket_id FROM entries WHERE id = ?1)",
            "UPDATE tickets SET invoice_cancelled_at = ?2
             WHERE id = (SELECT ticket_id FROM entries WHERE id = ?1)",
            true,
        ),
        (
            "an Arkade refund not settled",
            "INSERT INTO ticket_ark_refunds (ticket_id, refund_id, invoice, payment_hash,
                                             fee_sats, state, created_at, updated_at)
             SELECT ticket_id, 'refund', 'lnbc', 'hash', 0, 'paid', 1, 1 FROM entries
             WHERE id = ?1",
            "UPDATE ticket_ark_refunds SET state = 'settled', updated_at = CAST(?2 AS INTEGER)
             WHERE ticket_id = (SELECT ticket_id FROM entries WHERE id = ?1)",
            true,
        ),
        (
            "a paid escrow of a pool no batch funded",
            "INSERT INTO ticket_ark_escrows (ticket_id, ticket_hash, escrow_tap_tree,
                                             escrow_address, vtxo_outpoint, vtxo_sats, funded_at)
             SELECT t.id, t.hash, '00', 'tark1escrow', 'aa:0', 5000, 1
             FROM tickets t JOIN entries e ON e.ticket_id = t.id WHERE e.id = ?1",
            "INSERT INTO ark_funded_competitions (event_id, commitment_tx)
             SELECT event_id, 'batch' || ?2 FROM entries WHERE id = ?1",
            false,
        ),
    ];
    for (index, (name, open, close, timed)) in cases.into_iter().enumerate() {
        let start = 100 * DAY * index as i64;
        let competition = fixture.competition(3, 40).await;
        fixture.completed(&competition, start).await;
        fixture.sql(open, vec![competition.entry.to_string()]).await;
        assert_eq!(fixture.paid_out_since(&competition).await, None, "{name}");
        fixture.pass(start).await;
        fixture.pass(start + 30 * DAY).await;
        assert_eq!(fixture.stored(&competition.session).await, KEPT, "{name}");

        let closed = start + 31 * DAY;
        let closed_at = if close.contains("CAST(?2 AS INTEGER)") {
            (fixture.now + closed).to_string()
        } else {
            fixture.at(closed).trim_matches('\'').to_owned()
        };
        fixture
            .sql(close, vec![competition.entry.to_string(), closed_at])
            .await;
        fixture
            .assert_agrees_with_lifecycle_cleanup_and_recovery(&competition)
            .await;
        let since = if timed { closed } else { start };
        assert_eq!(
            fixture.paid_out_since(&competition).await,
            Some(fixture.now + since),
            "{name} closed"
        );
        if timed {
            fixture.pass(closed + 3 * DAY - 1).await;
            assert_eq!(fixture.stored(&competition.session).await, KEPT, "{name}");
        }
        fixture.pass(closed + 3 * DAY).await;
        assert_eq!(fixture.stored(&competition.session).await, GONE, "{name}");
    }
}

/// Money that moves again during the period makes the competition not paid out, and the
/// period starts over once it is paid out again.
#[tokio::test]
async fn money_that_moves_again_starts_the_period_over() {
    let fixture = Fixture::new().await;
    let finished = fixture.competition(3, 40).await;
    fixture.completed(&finished, 0).await;
    fixture.pass(0).await;
    assert!(fixture.survey(0).await.waiting.contains(&finished.id));

    // A restore finds a payment the database had no record of: an operator holds the payout.
    fixture
        .sql(
            format!(
                "INSERT INTO payout_holds (entry_id, payment_hash, amount_sats, reason, held_at)
                 VALUES (?, 'hash', 5000, 'restore', {})",
                fixture.at(2 * DAY)
            ),
            vec![finished.entry.to_string()],
        )
        .await;
    assert_eq!(fixture.paid_out_since(&finished).await, None);
    fixture.pass(3 * DAY).await;
    fixture.pass(10 * DAY).await;
    assert_eq!(fixture.stored(&finished.session).await, KEPT);

    fixture
        .sql(
            format!(
                "UPDATE payout_holds SET released_at = {} WHERE entry_id = ?",
                fixture.at(11 * DAY)
            ),
            vec![finished.entry.to_string()],
        )
        .await;
    assert_eq!(
        fixture.paid_out_since(&finished).await,
        Some(fixture.now + 11 * DAY)
    );
    fixture.pass(14 * DAY - 1).await;
    assert_eq!(fixture.stored(&finished.session).await, KEPT);
    fixture.pass(14 * DAY).await;
    assert_eq!(fixture.stored(&finished.session).await, GONE);
}

/// A competition that is final, whose session an operation still holds, keeps its state until
/// a pass finds the session free. The pass neither waits for it nor deletes any of it.
#[tokio::test]
async fn a_session_in_use_is_deleted_on_a_later_pass() {
    let fixture = Fixture::new().await;
    let finished = fixture.competition(3, 40).await;
    fixture.completed(&finished, -10 * DAY).await;

    let session = SessionId::parse(&finished.session).unwrap();
    let operation = fixture
        .coordinator
        .keymeld
        .try_hold_session(&session)
        .await
        .expect("free");
    tokio::time::timeout(std::time::Duration::from_secs(5), fixture.pass(0))
        .await
        .expect("the pass does not wait for the session");
    assert_eq!(fixture.stored(&finished.session).await, KEPT);

    drop(operation);
    fixture.pass(60).await;
    assert_eq!(fixture.stored(&finished.session).await, GONE);
}

/// Every format loses its row, and formats 2 and 3 every part and node, more than one batch of
/// them. Two competitions go a pass, the longest paid out first.
#[tokio::test]
async fn every_checkpoint_format_is_deleted_whole() {
    let fixture = Fixture::new().await;
    let mut finished = Vec::new();
    for (format, parts, paid_out) in [(3, 700, -5 * DAY), (1, 0, -7 * DAY), (2, 700, -6 * DAY)] {
        let competition = fixture.competition(format, parts).await;
        fixture.completed(&competition, paid_out).await;
        finished.push(competition);
    }
    let running = fixture.competition(3, 700).await;

    fixture.pass(0).await;
    assert_eq!(fixture.stored(&finished[1].session).await, GONE);
    assert_eq!(fixture.stored(&finished[2].session).await, GONE);
    assert_eq!(
        fixture.stored(&finished[0].session).await,
        (1, 700, 1),
        "the third waits for the next pass"
    );
    fixture.pass(60).await;
    for competition in &finished {
        assert_eq!(fixture.stored(&competition.session).await, GONE);
    }
    assert_eq!(fixture.stored(&running.session).await, (1, 700, 1));
}

/// A deletion that stopped part way, here after its first batch, leaves a checkpoint that
/// no longer loads; the next pass finishes it while the competition is still paid out. If its
/// money moved again meanwhile, what is left stays until it is paid out again.
#[tokio::test]
async fn a_deletion_that_stopped_part_way_finishes_on_the_next_pass() {
    let fixture = Fixture::new().await;
    let stop_part_way = |session: String| {
        let fixture = &fixture;
        async move {
            fixture
                .sql(
                    "DELETE FROM keymeld_protocol_parts WHERE session_id = ?1 AND digest IN (
                         SELECT digest FROM keymeld_protocol_parts WHERE session_id = ?1
                         LIMIT 256)",
                    vec![session],
                )
                .await;
        }
    };

    let finished = fixture.competition(3, 700).await;
    fixture.completed(&finished, -10 * DAY).await;
    stop_part_way(finished.session.clone()).await;
    fixture.pass(0).await;
    assert_eq!(fixture.stored(&finished.session).await, GONE);

    let reopened = fixture.competition(2, 700).await;
    fixture.completed(&reopened, -10 * DAY).await;
    stop_part_way(reopened.session.clone()).await;
    fixture
        .sql(
            "INSERT INTO owed_winners (entry_id, competition_id, amount_sats, owed_since)
             VALUES (?, ?, 5000, '2026-01-01T00:00:00Z')",
            vec![reopened.entry.to_string(), reopened.id.to_string()],
        )
        .await;
    fixture.pass(60).await;
    assert_eq!(fixture.stored(&reopened.session).await, (1, 444, 1));
    fixture
        .sql(
            format!(
                "UPDATE owed_winners SET settled_at = {} WHERE entry_id = ?",
                fixture.at(DAY)
            ),
            vec![reopened.entry.to_string()],
        )
        .await;
    fixture.pass(4 * DAY).await;
    assert_eq!(fixture.stored(&reopened.session).await, GONE);
}

/// A queued competition's refund session, and that of a pool that never got its own, has an id
/// of its own. Each goes once its competition is paid out: the queue once its pools all
/// finished and its leftover tickets were refunded, the pool once it was cancelled.
#[tokio::test]
async fn refund_sessions_with_ids_of_their_own_go_once_their_competition_is_paid_out() {
    let fixture = Fixture::new().await;
    let queue = fixture
        .seed(Uuid::now_v7(), &Uuid::now_v7().to_string(), 3, 40)
        .await;
    fixture
        .update(
            &queue,
            &format!("kind = 'queued', pools_formed_at = {}", fixture.at(-DAY)),
        )
        .await;
    let pool = fixture
        .seed(Uuid::now_v7(), &Uuid::now_v7().to_string(), 3, 40)
        .await;
    fixture
        .update(
            &pool,
            &format!(
                "kind = 'pool', parent_id = '{}', failed_at = {}, cancelled_at = {}",
                queue.id,
                fixture.at(-DAY),
                fixture.at(0)
            ),
        )
        .await;
    assert!(fixture.survey(0).await.unowned.is_empty());
    fixture.pass(10 * DAY).await;
    assert_eq!(
        fixture.stored(&queue.session).await,
        KEPT,
        "a pool still runs"
    );
    assert_eq!(fixture.stored(&pool.session).await, GONE);

    fixture
        .update(
            &queue,
            &format!("pools_finished_at = {}", fixture.at(11 * DAY)),
        )
        .await;
    fixture
        .assert_agrees_with_lifecycle_cleanup_and_recovery(&queue)
        .await;
    fixture.pass(14 * DAY - 1).await;
    assert_eq!(fixture.stored(&queue.session).await, KEPT);
    fixture.pass(14 * DAY).await;
    assert_eq!(fixture.stored(&queue.session).await, GONE);
}

/// A session is deleted only through the competition that owns it: one named by another
/// competition's id belongs to that competition, one two competitions name belongs to
/// neither, and a stored session that is not a session id names none.
#[tokio::test]
async fn a_session_this_competition_does_not_own_is_kept() {
    let fixture = Fixture::new().await;
    let owner = fixture.competition(3, 40).await;
    let borrower = fixture.seed(Uuid::now_v7(), &owner.session, 3, 0).await;
    fixture.completed(&borrower, -10 * DAY).await;

    let shared = Uuid::now_v7().to_string();
    let mut sharing = Vec::new();
    for _ in 0..2 {
        let competition = fixture.seed(Uuid::now_v7(), &shared, 3, 40).await;
        fixture
            .update(
                &competition,
                &format!("kind = 'queued', cancelled_at = {}", fixture.at(-10 * DAY)),
            )
            .await;
        sharing.push(competition.id);
    }
    let unreadable = fixture
        .seed(Uuid::now_v7(), "not a session id", 3, 40)
        .await;
    fixture
        .update(
            &unreadable,
            &format!("kind = 'queued', cancelled_at = {}", fixture.at(-10 * DAY)),
        )
        .await;

    let survey = fixture.survey(0).await;
    assert!(survey.due.is_empty() && survey.waiting.is_empty());
    let mut unowned = survey.unowned.clone();
    unowned.sort();
    let mut expected = vec![sharing[0], sharing[1], unreadable.id];
    expected.sort();
    assert_eq!(unowned, expected);

    fixture.pass(0).await;
    fixture.pass(30 * DAY).await;
    // The owner still runs, and keeps its parts and nodes.
    assert_eq!(fixture.stored(&owner.session).await.1, 40);
    assert_eq!(fixture.stored(&shared).await, (1, 80, 2));
    assert_eq!(fixture.stored("not a session id").await, (1, 40, 1));
}

/// Times are read however they are stored: RFC 3339 with or without fractions, SQLite's own
/// format, an offset, or Unix seconds. One that is not a time keeps the state.
#[tokio::test]
async fn times_are_read_in_every_stored_format_and_an_unreadable_one_keeps_the_state() {
    let fixture = Fixture::new().await;
    let base = OffsetDateTime::from_unix_timestamp(fixture.now).unwrap();
    let formats = [
        base.format(&Rfc3339).unwrap(),
        (base + time::Duration::nanoseconds(123_456_789))
            .format(&Rfc3339)
            .unwrap(),
        base.format(time::macros::format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second]"
        ))
        .unwrap(),
        base.format(time::macros::format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond][offset_hour sign:mandatory]:[offset_minute]"
        ))
        .unwrap(),
    ];
    for completed in formats {
        let competition = fixture.competition(3, 1).await;
        fixture
            .update(&competition, &format!("completed_at = '{completed}'"))
            .await;
        assert_eq!(
            fixture.paid_out_since(&competition).await,
            Some(fixture.now),
            "{completed}"
        );
    }

    let unreadable = fixture.competition(3, 40).await;
    fixture
        .update(&unreadable, "completed_at = 'last tuesday'")
        .await;
    assert_eq!(
        fixture.paid_out_since(&unreadable).await,
        Some(UNREADABLE_TIME)
    );
    fixture.pass(365 * DAY).await;
    assert_eq!(fixture.stored(&unreadable.session).await, KEPT);
    assert!(fixture
        .survey(365 * DAY)
        .await
        .waiting
        .contains(&unreadable.id));
}

/// The rule agrees with the lifecycle, cleanup and the recovery records in every state these
/// tests use.
#[tokio::test]
async fn paid_out_never_holds_where_the_lifecycle_cleanup_or_recovery_sees_money() {
    let fixture = Fixture::new().await;
    let then = fixture.at(-DAY);
    for set in [
        format!("completed_at = {then}"),
        format!("cancelled_at = {then}"),
        format!("failed_at = {then}"),
        format!("funding_confirmed_at = {then}, cancelled_at = {then}"),
        format!("funding_broadcasted_at = {then}, cancelled_at = {then}"),
        format!("kind = 'queued', pools_formed_at = {then}"),
        format!("kind = 'queued', pools_formed_at = {then}, pools_finished_at = {then}"),
    ] {
        for open in [
            None,
            Some("UPDATE tickets SET settled_at = NULL WHERE event_id = ?"),
            Some("UPDATE tickets SET escrow_transaction = '00' WHERE event_id = ?"),
            Some(
                "INSERT INTO ticket_ark_escrows (ticket_id, ticket_hash, escrow_tap_tree,
                                                 escrow_address, vtxo_outpoint, vtxo_sats, funded_at)
                 SELECT id, hash, '00', 'tark1escrow', 'aa:0', 5000, 1 FROM tickets
                 WHERE event_id = ?",
            ),
            Some(
                "INSERT INTO owed_winners (entry_id, competition_id, amount_sats, owed_since)
                 SELECT id, event_id, 5000, '2026-01-01T00:00:00Z' FROM entries
                 WHERE event_id = ?",
            ),
            Some(
                "INSERT INTO ark_funded_competitions (event_id, commitment_tx) VALUES (?, 'batch')",
            ),
        ] {
            let competition = fixture.competition(3, 1).await;
            fixture.update(&competition, &set).await;
            if let Some(open) = open {
                fixture.sql(open, vec![competition.id.to_string()]).await;
            }
            fixture
                .assert_agrees_with_lifecycle_cleanup_and_recovery(&competition)
                .await;
            if open.is_some_and(|open| !open.contains("ark_funded_competitions")) {
                assert_eq!(
                    fixture.paid_out_since(&competition).await,
                    None,
                    "{set}: {open:?}"
                );
            }
        }
    }
}
