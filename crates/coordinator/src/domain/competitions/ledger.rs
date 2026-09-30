//! A player's money, entry by entry: what each entry cost, and what came back to them, as a
//! payout, a refund, or nothing yet. The entries page shows it, with its totals.
//!
//! Read in one query from the tables that already record it: the ticket (its price and when it
//! was paid, and whether its hold invoice settled or was cancelled), its Arkade escrow and that
//! escrow's refund, and the entry's latest payout.

use sqlx::{sqlite::SqliteRow, Row};
use time::OffsetDateTime;

use super::{
    ark_refund::refund_opens_at, ArkRefundState, CompetitionStore, Coordinator, CoordinatorFee,
};
use crate::domain::{leaderboard::Phase, Error};

/// What one entry cost, all in, and its parts. `total_sats` is the "Entry fee" players see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryPayment {
    /// The stake, which goes into the pot.
    pub entry_fee_sats: u64,
    pub service_fee_sats: u64,
    pub network_fee_sats: u64,
    pub total_sats: u64,
    pub paid_at: Option<OffsetDateTime>,
}

/// An entry's latest payout: the one that settled or is on its way, or else the last that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerPayout {
    pub id: String,
    pub sats: u64,
    pub initiated_at: Option<OffsetDateTime>,
    pub state: PayoutState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutState {
    /// Sent, and not settled yet.
    Pending,
    Settled(Option<OffsetDateTime>),
    /// Failed; it can be tried again.
    Failed,
}

/// A ticket's funded Arkade escrow, and its refund once one began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEscrow {
    pub sats: u64,
    /// When its refund opens: the escrow's refund locktime. Only read for an escrow not
    /// refunded yet.
    pub opens_at: Option<OffsetDateTime>,
    /// A batch spent it into its competition's funding, so there is nothing left to refund.
    pub spent_into_pool: bool,
    /// An operator wrote its refund off: it can never finish, and support settles it instead.
    pub written_off: bool,
    pub refund: Option<EscrowRefund>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscrowRefund {
    pub id: String,
    /// The hash of the player's invoice it pays.
    pub payment_hash: String,
    pub fee_sats: u64,
    pub state: ArkRefundState,
    pub updated_at: Option<OffsetDateTime>,
}

/// One of a player's entries, with its money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub entry_id: String,
    pub competition_id: String,
    /// The window's start, as stored, for a competition that can't be shown.
    pub start_time: String,
    /// The ticket's payment hash.
    pub payment_hash: String,
    /// None while the ticket isn't paid.
    pub payment: Option<EntryPayment>,
    /// Its hold invoice settled: the payment was taken.
    pub lightning_settled: bool,
    /// Its hold invoice was cancelled, which returns the payment.
    pub lightning_released_at: Option<OffsetDateTime>,
    pub escrow: Option<LedgerEscrow>,
    pub payout: Option<LedgerPayout>,
}

/// What came back for an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Returned {
    /// Nothing yet: the competition hasn't started, or is live.
    InPlay,
    AwaitingResult,
    /// It finished, and no payout is recorded for the entry.
    NoPayout,
    Payout(LedgerPayout),
    Refund(Refund),
    /// Its escrow's refund was written off: it can never finish on its own.
    RefundWrittenOff,
    /// It didn't run, and no refund is recorded or on its way.
    NoRefund,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refund {
    pub kind: RefundKind,
    /// What the player gets back: all they paid for a released hold invoice, the escrow less the
    /// refund's Lightning fee once that is known.
    pub sats: u64,
    pub state: RefundState,
    /// The refund's id and the hash of the invoice it pays, for an escrow refund under way.
    pub id: Option<String>,
    pub payment_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundKind {
    /// The hold invoice is cancelled, so the payment never leaves the player's wallet.
    Lightning,
    /// The Arkade escrow is paid back to the player's Lightning Address.
    Escrow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundState {
    /// Nothing can be refunded before the escrow's refund locktime.
    Locked(OffsetDateTime),
    Refunding,
    Refunded(Option<OffsetDateTime>),
}

/// The competition was called off, or never filled: entry fees are owed back.
fn did_not_run(phase: Phase) -> bool {
    matches!(phase, Phase::Unfilled | Phase::Cancelled | Phase::Failed)
}

impl LedgerEntry {
    /// What came back for the entry, in a competition in `phase` (none when it can't be read).
    pub fn returned(&self, phase: Option<Phase>, now: OffsetDateTime) -> Returned {
        if let Some(payout) = &self.payout {
            return Returned::Payout(payout.clone());
        }
        let dead = phase.is_some_and(did_not_run);
        if let Some(escrow) = self
            .escrow
            .as_ref()
            .filter(|escrow| !escrow.spent_into_pool)
        {
            let refunded = escrow.refund.as_ref().is_some_and(|refund| {
                matches!(refund.state, ArkRefundState::Paid | ArkRefundState::Settled)
            });
            if escrow.written_off && !refunded {
                return Returned::RefundWrittenOff;
            }
            if let Some(refund) = &escrow.refund {
                let state = match refund.state {
                    ArkRefundState::Paid | ArkRefundState::Settled => {
                        RefundState::Refunded(refund.updated_at)
                    }
                    _ => RefundState::Refunding,
                };
                return Returned::Refund(Refund {
                    kind: RefundKind::Escrow,
                    sats: escrow.sats.saturating_sub(refund.fee_sats),
                    state,
                    id: Some(refund.id.clone()),
                    payment_hash: Some(refund.payment_hash.clone()),
                });
            }
            if dead {
                let state = match escrow.opens_at {
                    Some(at) if at > now => RefundState::Locked(at),
                    _ => RefundState::Refunding,
                };
                return Returned::Refund(Refund {
                    kind: RefundKind::Escrow,
                    sats: escrow.sats,
                    state,
                    id: None,
                    payment_hash: None,
                });
            }
        }
        if let (Some(payment), Some(at)) = (&self.payment, self.lightning_released_at) {
            return Returned::Refund(
                self.lightning_refund(payment, RefundState::Refunded(Some(at))),
            );
        }
        match phase {
            Some(phase) if did_not_run(phase) => match &self.payment {
                // A held payment is released once the competition is cancelled.
                Some(payment) if !self.lightning_settled && self.escrow.is_none() => {
                    Returned::Refund(self.lightning_refund(payment, RefundState::Refunding))
                }
                _ => Returned::NoRefund,
            },
            Some(Phase::AwaitingResult) => Returned::AwaitingResult,
            Some(Phase::Scored | Phase::Expired) => Returned::NoPayout,
            _ => Returned::InPlay,
        }
    }

    fn lightning_refund(&self, payment: &EntryPayment, state: RefundState) -> Refund {
        Refund {
            kind: RefundKind::Lightning,
            sats: payment.total_sats,
            state,
            id: None,
            payment_hash: Some(self.payment_hash.clone()),
        }
    }
}

/// A player's money over all their entries. Only what settled counts as received; what is on
/// its way is counted apart, so the net isn't misread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerTotals {
    /// Paid entries.
    pub entries: usize,
    pub paid_sats: u64,
    /// Settled payouts.
    pub won_sats: u64,
    /// Settled refunds.
    pub refunded_sats: u64,
    /// Payouts sent and not settled yet.
    pub pending_payouts: usize,
    pub pending_payout_sats: u64,
    /// Refunds owed, locked or under way.
    pub pending_refunds: usize,
    pub pending_refund_sats: u64,
    /// When the first locked refund opens.
    pub locked_until: Option<OffsetDateTime>,
}

impl LedgerTotals {
    pub fn add(&mut self, entry: &LedgerEntry, returned: &Returned) {
        if let Some(payment) = &entry.payment {
            self.entries += 1;
            self.paid_sats += payment.total_sats;
        }
        match returned {
            Returned::Payout(payout) => match payout.state {
                PayoutState::Settled(_) => self.won_sats += payout.sats,
                PayoutState::Pending => {
                    self.pending_payouts += 1;
                    self.pending_payout_sats += payout.sats;
                }
                PayoutState::Failed => {}
            },
            Returned::Refund(refund) => match refund.state {
                RefundState::Refunded(_) => self.refunded_sats += refund.sats,
                RefundState::Locked(at) => {
                    self.pending_refunds += 1;
                    self.pending_refund_sats += refund.sats;
                    self.locked_until = Some(self.locked_until.map_or(at, |first| first.min(at)));
                }
                RefundState::Refunding => {
                    self.pending_refunds += 1;
                    self.pending_refund_sats += refund.sats;
                }
            },
            _ => {}
        }
    }

    pub fn received_sats(&self) -> u64 {
        self.won_sats + self.refunded_sats
    }

    /// Received less paid: above zero, the player is up.
    pub fn net_sats(&self) -> i64 {
        self.received_sats() as i64 - self.paid_sats as i64
    }

    pub fn pending_sats(&self) -> u64 {
        self.pending_payout_sats + self.pending_refund_sats
    }
}

impl CompetitionStore {
    /// Every entry of the player with `pubkey` (hex) with its money, newest competition first,
    /// in one query.
    pub async fn player_ledger(&self, pubkey: &str) -> Result<Vec<LedgerEntry>, sqlx::Error> {
        let rows = sqlx::query(
            "WITH latest_payouts AS (
                SELECT entry_id, id, payout_amount_sats, initiated_at, succeed_at, failed_at,
                       ROW_NUMBER() OVER (
                           PARTITION BY entry_id
                           ORDER BY failed_at IS NOT NULL, unixepoch(initiated_at) DESC
                       ) AS rn
                FROM payouts
                WHERE entry_id IN (SELECT id FROM entries WHERE pubkey = ?1)
            )
            SELECT e.id AS entry_id, e.event_id AS competition_id,
                   json_extract(c.event_submission, '$.start_observation_date') AS start_time,
                   json_extract(c.event_submission, '$.entry_fee') AS entry_fee,
                   CAST(json_extract(c.event_submission, '$.coordinator_fee_basis_points') AS INTEGER)
                       AS fee_basis_points,
                   CAST(json_extract(c.event_submission, '$.coordinator_fee_percentage') AS REAL)
                       AS fee_percentage,
                   t.hash AS payment_hash, t.network_fee_sats AS network_fee_sats,
                   t.paid_at IS NOT NULL AS paid, unixepoch(t.paid_at) AS paid_at,
                   t.settled_at IS NOT NULL AS settled,
                   unixepoch(t.invoice_cancelled_at) AS released_at,
                   a.vtxo_sats AS escrow_sats, a.escrow_tap_tree AS escrow_tap_tree,
                   f.commitment_tx IS NOT NULL AS spent_into_pool,
                   w.ticket_id IS NOT NULL AS written_off,
                   r.refund_id AS refund_id, r.payment_hash AS refund_payment_hash,
                   r.fee_sats AS refund_fee_sats, r.state AS refund_state,
                   r.updated_at AS refund_updated_at,
                   p.id AS payout_id, p.payout_amount_sats AS payout_sats,
                   unixepoch(p.initiated_at) AS payout_initiated_at,
                   p.succeed_at IS NOT NULL AS payout_settled,
                   unixepoch(p.succeed_at) AS payout_settled_at,
                   p.failed_at IS NOT NULL AS payout_failed
            FROM entries e
            JOIN competitions c ON c.id = e.event_id
            JOIN tickets t ON t.id = e.ticket_id
            LEFT JOIN ticket_ark_escrows a
                ON a.ticket_id = t.id AND a.ticket_hash = t.hash AND a.funded_at IS NOT NULL
            LEFT JOIN ticket_ark_refunds r ON r.ticket_id = t.id
            LEFT JOIN ark_funded_competitions f ON f.event_id = t.event_id
            LEFT JOIN ticket_ark_refund_write_offs w
                ON w.ticket_id = t.id AND w.ticket_hash = t.hash
            LEFT JOIN latest_payouts p ON p.entry_id = e.id AND p.rn = 1
            WHERE e.pubkey = ?1
            ORDER BY json_extract(c.event_submission, '$.start_observation_date') DESC, e.id DESC",
        )
        .bind(pubkey)
        .fetch_all(self.db_connection.read())
        .await?;
        rows.iter().map(ledger_entry).collect()
    }
}

impl Coordinator {
    /// The player's entries with their money; see [`CompetitionStore::player_ledger`].
    pub async fn player_ledger(&self, pubkey: &str) -> Result<Vec<LedgerEntry>, Error> {
        Ok(self.competition_store.player_ledger(pubkey).await?)
    }
}

fn unix(row: &SqliteRow, column: &str) -> Result<Option<OffsetDateTime>, sqlx::Error> {
    Ok(row
        .try_get::<Option<i64>, _>(column)?
        .and_then(|at| OffsetDateTime::from_unix_timestamp(at).ok()))
}

fn sats(row: &SqliteRow, column: &str) -> Result<Option<u64>, sqlx::Error> {
    Ok(row
        .try_get::<Option<i64>, _>(column)?
        .map(|value| value.max(0) as u64))
}

/// The coordinator fee as the event stores it: in basis points, or as a percentage in older
/// events.
fn coordinator_fee(row: &SqliteRow) -> Result<CoordinatorFee, sqlx::Error> {
    let basis_points: Option<i64> = row.try_get("fee_basis_points")?;
    let percentage: Option<f64> = row.try_get("fee_percentage")?;
    let fields = serde_json::json!({
        "coordinator_fee_basis_points": basis_points,
        "coordinator_fee_percentage": percentage,
    });
    serde_json::from_value(fields).map_err(|e| sqlx::Error::ColumnDecode {
        index: "coordinator_fee".into(),
        source: Box::new(e),
    })
}

fn ledger_entry(row: &SqliteRow) -> Result<LedgerEntry, sqlx::Error> {
    let payment = if row.try_get::<bool, _>("paid")? {
        let entry_fee_sats = sats(row, "entry_fee")?.unwrap_or_default();
        let service_fee_sats = coordinator_fee(row)?.fee_for(entry_fee_sats);
        let network_fee_sats = sats(row, "network_fee_sats")?.unwrap_or_default();
        Some(EntryPayment {
            entry_fee_sats,
            service_fee_sats,
            network_fee_sats,
            total_sats: entry_fee_sats + service_fee_sats + network_fee_sats,
            paid_at: unix(row, "paid_at")?,
        })
    } else {
        None
    };
    let refund = match row.try_get::<Option<String>, _>("refund_id")? {
        Some(id) => Some(EscrowRefund {
            id,
            payment_hash: row.try_get("refund_payment_hash")?,
            fee_sats: sats(row, "refund_fee_sats")?.unwrap_or_default(),
            state: row.try_get::<String, _>("refund_state")?.parse()?,
            updated_at: unix(row, "refund_updated_at")?,
        }),
        None => None,
    };
    let escrow = match sats(row, "escrow_sats")? {
        Some(sats) => {
            let spent_into_pool: bool = row.try_get("spent_into_pool")?;
            let written_off: bool = row.try_get("written_off")?;
            // Only an escrow still owed back needs its locktime, which takes its script.
            let opens_at = if spent_into_pool || written_off || refund.is_some() {
                None
            } else {
                row.try_get::<Option<String>, _>("escrow_tap_tree")?
                    .as_deref()
                    .and_then(refund_opens_at)
            };
            Some(LedgerEscrow {
                sats,
                opens_at,
                spent_into_pool,
                written_off,
                refund,
            })
        }
        None => None,
    };
    let payout = match row.try_get::<Option<String>, _>("payout_id")? {
        Some(id) => Some(LedgerPayout {
            id,
            sats: sats(row, "payout_sats")?.unwrap_or_default(),
            initiated_at: unix(row, "payout_initiated_at")?,
            state: if row.try_get::<bool, _>("payout_settled")? {
                PayoutState::Settled(unix(row, "payout_settled_at")?)
            } else if row.try_get::<bool, _>("payout_failed")? {
                PayoutState::Failed
            } else {
                PayoutState::Pending
            },
        }),
        None => None,
    };
    Ok(LedgerEntry {
        entry_id: row.try_get("entry_id")?,
        competition_id: row.try_get("competition_id")?,
        start_time: row
            .try_get::<Option<String>, _>("start_time")?
            .unwrap_or_default(),
        payment_hash: row.try_get("payment_hash")?,
        payment,
        lightning_settled: row.try_get("settled")?,
        lightning_released_at: unix(row, "released_at")?,
        escrow,
        payout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Competition, CreateEvent};
    use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
    use coordinator_ark::testing::{keypair, xonly};
    use coordinator_ark_escrow::{EntryEscrow, RelativeTimelock, ServerRules};
    use time::Duration;
    use uuid::Uuid;

    const PLAYER: &str = "player";

    fn at(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    /// An escrow's tap tree, hex, whose refund opens at `refund_at`.
    fn escrow_tap_tree(refund_at: OffsetDateTime) -> String {
        let rules = ServerRules {
            signer: xonly(&keypair(3)),
            min_exit_delay: RelativeTimelock::Seconds(512),
            block_timelocks_allowed: false,
        };
        let refund_at = refund_at.unix_timestamp() as u32;
        let terms = coordinator_ark::escrow_terms(
            &rules,
            xonly(&keypair(1)),
            xonly(&keypair(2)),
            refund_at,
            refund_at - 3600,
        )
        .unwrap();
        hex::encode(
            EntryEscrow::new(terms)
                .unwrap()
                .vtxo_script()
                .encode_tap_tree(),
        )
    }

    async fn store() -> (CompetitionStore, DBConnection, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        (CompetitionStore::new(database.clone()), database, directory)
    }

    /// A competition with a 5,000 sat entry fee and a 5% service fee, whose window opened
    /// `days_ago` days ago.
    async fn competition(store: &CompetitionStore, days_ago: i64) -> Uuid {
        let start = OffsetDateTime::now_utc() - Duration::days(days_ago);
        let competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + Duration::days(2),
            start_observation_date: start,
            end_observation_date: start + Duration::hours(1),
            locations: vec!["KORD".into()],
            number_of_values_per_entry: 1,
            number_of_places_win: 1,
            total_allowed_entries: 3,
            entry_fee: 5_000,
            coordinator_fee: CoordinatorFee::whole_percent(5),
            total_competition_pool: 15_000,
            relative_locktime_block_delta: None,
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
        });
        let id = competition.id;
        store
            .add_competition_with_tickets(competition, vec![])
            .await
            .unwrap();
        id
    }

    /// A paid entry in `event` by `pubkey`, whose ticket carries a network fee: the entry's id and
    /// its ticket's.
    async fn entry(database: &DBConnection, event: Uuid, pubkey: &str) -> (String, String) {
        let (ticket, entry) = (Uuid::now_v7().to_string(), Uuid::now_v7().to_string());
        let (event, pubkey) = (event.to_string(), pubkey.to_owned());
        let ids = (entry.clone(), ticket.clone());
        database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_at,
                         paid_at, network_fee_sats)
                     VALUES (?, ?, 'encrypted', ?, datetime('now'), '2026-09-01 10:00:00', 50)",
                )
                .bind(&ticket)
                .bind(&event)
                .bind(format!("hash-{ticket}"))
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey,
                         payout_hash, entry_submission)
                     VALUES (?, ?, ?, ?, ?, ?, '{}')",
                )
                .bind(&entry)
                .bind(&event)
                .bind(&ticket)
                .bind(&pubkey)
                .bind(format!("pubkey-{entry}"))
                .bind(format!("hash-{entry}"))
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
        ids
    }

    async fn run(database: &DBConnection, sql: &'static str, binds: Vec<String>) {
        database
            .execute_write(move |pool| async move {
                let mut query = sqlx::query(sql);
                for bind in binds {
                    query = query.bind(bind);
                }
                query.execute(&pool).await?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn payout(database: &DBConnection, entry: &str, sats: i64, state: &str) {
        let settled = (state == "settled").then(|| "2026-09-03T12:00:00.5Z".to_owned());
        let failed = (state == "failed").then(|| "2026-09-03T11:00:00Z".to_owned());
        let (id, entry) = (Uuid::now_v7().to_string(), entry.to_owned());
        database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO payouts (id, entry_id, payout_payment_request,
                         payout_amount_sats, initiated_at, succeed_at, failed_at)
                     VALUES (?, ?, 'lnbc1', ?, '2026-09-03T10:00:00.123456789Z', ?, ?)",
                )
                .bind(id)
                .bind(entry)
                .bind(sats)
                .bind(settled)
                .bind(failed)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// Funds the ticket's escrow with its price, refundable from `refund_at`.
    async fn escrow(database: &DBConnection, ticket: &str, refund_at: OffsetDateTime) {
        run(
            database,
            "INSERT INTO ticket_ark_escrows (ticket_id, ticket_hash, escrow_tap_tree,
                 escrow_address, vtxo_sats, funded_at)
             VALUES (?1, 'hash-' || ?1, ?2, 'tark1escrow', 5300, 1756720800)",
            vec![ticket.to_owned(), escrow_tap_tree(refund_at)],
        )
        .await;
        run(
            database,
            "UPDATE tickets SET settled_at = paid_at WHERE id = ?",
            vec![ticket.to_owned()],
        )
        .await;
    }

    #[tokio::test]
    async fn one_query_reads_what_each_entry_paid_and_got_back() {
        let (store, database, _directory) = store().await;
        let now = OffsetDateTime::now_utc();

        // Won, and the payout settled; before it, a payout that failed.
        let won_in = competition(&store, 5).await;
        let (won, won_ticket) = entry(&database, won_in, PLAYER).await;
        payout(&database, &won, 9_000, "failed").await;
        payout(&database, &won, 12_000, "settled").await;
        run(
            &database,
            "UPDATE tickets SET settled_at = paid_at WHERE id = ?",
            vec![won_ticket],
        )
        .await;
        // Won, with the payout still on its way.
        let pending_in = competition(&store, 4).await;
        let (pending, _) = entry(&database, pending_in, PLAYER).await;
        payout(&database, &pending, 7_000, "pending").await;
        // Its escrow refunded: the refund's Lightning fee is kept back.
        let refunded_in = competition(&store, 3).await;
        let (refunded, refunded_ticket) = entry(&database, refunded_in, PLAYER).await;
        escrow(&database, &refunded_ticket, now - Duration::days(1)).await;
        run(
            &database,
            "INSERT INTO ticket_ark_refunds (ticket_id, refund_id, invoice, payment_hash,
                 fee_sats, state, created_at, updated_at)
             VALUES (?, 'refund-1', 'lnbc1refund', 'refund-hash', 20, 'settled',
                 1788436000, 1788436800)",
            vec![refunded_ticket],
        )
        .await;
        // Its escrow not refunded yet, and locked for another day.
        let locked_in = competition(&store, 2).await;
        let (locked, locked_ticket) = entry(&database, locked_in, PLAYER).await;
        let opens = OffsetDateTime::from_unix_timestamp((now + Duration::days(1)).unix_timestamp())
            .unwrap();
        escrow(&database, &locked_ticket, opens).await;
        // Its hold invoice cancelled, which releases the payment.
        let released_in = competition(&store, 1).await;
        let (released, released_ticket) = entry(&database, released_in, PLAYER).await;
        run(
            &database,
            "UPDATE tickets SET invoice_cancelled_at = '2026-09-02 09:30:00' WHERE id = ?",
            vec![released_ticket],
        )
        .await;
        // Its escrow's refund written off by an operator.
        let written_off_in = competition(&store, 6).await;
        let (written_off, written_off_ticket) = entry(&database, written_off_in, PLAYER).await;
        escrow(&database, &written_off_ticket, now - Duration::days(1)).await;
        run(
            &database,
            "INSERT INTO ticket_ark_refund_write_offs (ticket_id, ticket_hash, reason,
                 written_off_at)
             VALUES (?1, 'hash-' || ?1, 'no registration', 1788436800)",
            vec![written_off_ticket],
        )
        .await;
        // Another player's entry is not the player's.
        entry(&database, won_in, "someone-else").await;

        let ledger = store.player_ledger(PLAYER).await.unwrap();
        let ids: Vec<&str> = ledger.iter().map(|row| row.entry_id.as_str()).collect();
        assert_eq!(
            ids,
            [&released, &locked, &refunded, &pending, &won, &written_off].map(String::as_str),
            "newest competition first, and only the player's"
        );
        let by_id = |id: &str| ledger.iter().find(|row| row.entry_id == id).unwrap();

        // Every entry cost the same, all in: 5,000 + 5% + the ticket's network fee.
        for row in &ledger {
            assert_eq!(
                row.payment,
                Some(EntryPayment {
                    entry_fee_sats: 5_000,
                    service_fee_sats: 250,
                    network_fee_sats: 50,
                    total_sats: 5_300,
                    paid_at: Some(at(1_788_256_800)),
                })
            );
            assert!(row.payment_hash.starts_with("hash-"));
        }

        let won = by_id(&won);
        assert!(won.lightning_settled);
        let payout = won.payout.as_ref().unwrap();
        assert_eq!(
            payout.sats, 12_000,
            "the settled payout, not the failed one"
        );
        assert_eq!(payout.state, PayoutState::Settled(Some(at(1_788_436_800))));
        assert_eq!(payout.initiated_at, Some(at(1_788_429_600)));
        assert_eq!(
            by_id(&pending).payout.as_ref().unwrap().state,
            PayoutState::Pending
        );

        let refund = by_id(&refunded).escrow.as_ref().unwrap();
        assert_eq!(refund.sats, 5_300);
        assert_eq!(
            refund.opens_at, None,
            "refunded, so its locktime isn't read"
        );
        let refund = refund.refund.as_ref().unwrap();
        assert_eq!(
            (refund.id.as_str(), refund.fee_sats, refund.state),
            ("refund-1", 20, ArkRefundState::Settled)
        );
        assert_eq!(refund.updated_at, Some(at(1_788_436_800)));

        let locked = by_id(&locked).escrow.as_ref().unwrap();
        assert_eq!(locked.opens_at, Some(opens));
        assert!(!locked.spent_into_pool && !locked.written_off && locked.refund.is_none());

        let released = by_id(&released);
        assert_eq!(released.lightning_released_at, Some(at(1_788_341_400)));
        assert!(!released.lightning_settled && released.escrow.is_none());

        let written_off = by_id(&written_off).escrow.as_ref().unwrap();
        assert!(written_off.written_off && written_off.opens_at.is_none());

        // What each shows once the competitions are read: these two won, and the others were
        // called off.
        let mut totals = LedgerTotals::default();
        for row in &ledger {
            let phase = if row.payout.is_some() {
                Phase::Scored
            } else {
                Phase::Cancelled
            };
            totals.add(row, &row.returned(Some(phase), now));
        }
        assert_eq!(
            totals,
            LedgerTotals {
                entries: 6,
                paid_sats: 31_800,
                won_sats: 12_000,
                refunded_sats: 5_280 + 5_300,
                pending_payouts: 1,
                pending_payout_sats: 7_000,
                pending_refunds: 1,
                pending_refund_sats: 5_300,
                locked_until: Some(opens),
            }
        );
        assert_eq!(totals.net_sats(), 22_580 - 31_800);
        assert!(store.player_ledger("nobody").await.unwrap().is_empty());
    }

    fn paid() -> LedgerEntry {
        LedgerEntry {
            entry_id: "e".into(),
            competition_id: "c".into(),
            start_time: String::new(),
            payment_hash: "hash".into(),
            payment: Some(EntryPayment {
                entry_fee_sats: 5_000,
                service_fee_sats: 250,
                network_fee_sats: 50,
                total_sats: 5_300,
                paid_at: None,
            }),
            lightning_settled: false,
            lightning_released_at: None,
            escrow: None,
            payout: None,
        }
    }

    #[test]
    fn what_came_back_follows_the_competition_until_money_moves() {
        let now = at(1_800_000_000);
        let entry = paid();
        assert_eq!(entry.returned(Some(Phase::Live), now), Returned::InPlay);
        assert_eq!(entry.returned(None, now), Returned::InPlay);
        assert_eq!(
            entry.returned(Some(Phase::AwaitingResult), now),
            Returned::AwaitingResult
        );
        assert_eq!(entry.returned(Some(Phase::Scored), now), Returned::NoPayout);

        // A held payment of a competition that didn't run is released, all of it.
        let Returned::Refund(refund) = entry.returned(Some(Phase::Unfilled), now) else {
            panic!("a held payment is refunded");
        };
        assert_eq!(
            (refund.kind, refund.sats, refund.state),
            (RefundKind::Lightning, 5_300, RefundState::Refunding)
        );
        // Once taken, nothing is owed back through the invoice.
        let settled = LedgerEntry {
            lightning_settled: true,
            ..paid()
        };
        assert_eq!(
            settled.returned(Some(Phase::Cancelled), now),
            Returned::NoRefund
        );

        // An escrow opens for its refund at its locktime.
        let escrowed = LedgerEntry {
            lightning_settled: true,
            escrow: Some(LedgerEscrow {
                sats: 5_300,
                opens_at: Some(now + Duration::hours(3)),
                spent_into_pool: false,
                written_off: false,
                refund: None,
            }),
            ..paid()
        };
        let state = |entry: &LedgerEntry, now| match entry.returned(Some(Phase::Cancelled), now) {
            Returned::Refund(refund) => refund.state,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            state(&escrowed, now),
            RefundState::Locked(now + Duration::hours(3))
        );
        assert_eq!(
            state(&escrowed, now + Duration::hours(4)),
            RefundState::Refunding
        );
        // An escrow a batch spent into its pool is the contract's to pay out.
        let spent = LedgerEntry {
            escrow: Some(LedgerEscrow {
                spent_into_pool: true,
                ..escrowed.escrow.clone().unwrap()
            }),
            ..escrowed.clone()
        };
        assert_eq!(spent.returned(Some(Phase::Scored), now), Returned::NoPayout);
        // A refund written off is no longer on its way; one paid before stays refunded.
        let written_off = LedgerEntry {
            escrow: Some(LedgerEscrow {
                written_off: true,
                ..escrowed.escrow.clone().unwrap()
            }),
            ..escrowed.clone()
        };
        assert_eq!(
            written_off.returned(Some(Phase::Cancelled), now),
            Returned::RefundWrittenOff
        );
        // A refund under way counts once the player's invoice is paid.
        let mut refunding = escrowed.clone();
        refunding.escrow.as_mut().unwrap().refund = Some(EscrowRefund {
            id: "r".into(),
            payment_hash: "h".into(),
            fee_sats: 20,
            state: ArkRefundState::Submitted,
            updated_at: Some(now),
        });
        assert_eq!(state(&refunding, now), RefundState::Refunding);
        refunding
            .escrow
            .as_mut()
            .unwrap()
            .refund
            .as_mut()
            .unwrap()
            .state = ArkRefundState::Paid;
        assert_eq!(state(&refunding, now), RefundState::Refunded(Some(now)));
    }
}
