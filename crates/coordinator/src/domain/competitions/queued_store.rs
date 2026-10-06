//! Storage for queued competitions: their terms, tickets made on demand, and the pools they form.
//! See `queued.rs`.

use coordinator_escrow::{pools::PoolRules, queued::QueuedTerms};
use dlctix::bitcoin::BlockHash;
use dlctix::hashlock;
use sqlx::Row;
use std::str::FromStr;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use super::{
    admission::before_deadline, queued::MAX_UNPAID_TICKETS_PER_PLAYER, Competition,
    CompetitionStore, CreateEvent, Lease, ReservedTicket, Ticket,
};
use crate::infra::{db::DatabaseWriteError, oracle::AddEventEntry};

/// A queued competition's settings and the terms its players consent to.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueSettings {
    pub competition_id: Uuid,
    pub pool_rules: PoolRules,
    pub stake_sats: u64,
    pub max_entries: u32,
    pub terms: QueuedTerms,
    /// The digest every key deposit is sealed under.
    pub terms_digest: [u8; 32],
}

/// One pool a queued competition formed, and the seed inputs it was formed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRecord {
    pub competition_id: Uuid,
    pub parent_id: Uuid,
    pub pool_index: u32,
    pub close_height: u32,
    pub block_hash: BlockHash,
    /// Every ticket the kickoff placed, sorted.
    pub tickets: Vec<Uuid>,
    /// This pool's tickets, sorted.
    pub members: Vec<Uuid>,
    /// UNIX seconds.
    pub formed_at: i64,
}

/// A pool to create at kickoff.
#[derive(Debug, Clone)]
pub struct NewPool {
    pub competition_id: Uuid,
    pub pool_index: u32,
    /// The pool's tickets, sorted.
    pub members: Vec<Uuid>,
    pub event_submission: CreateEvent,
}

/// Everything a kickoff writes at once.
#[derive(Debug, Clone)]
pub struct PoolFormation {
    pub parent_id: Uuid,
    pub close_height: u32,
    pub block_hash: BlockHash,
    /// Every ticket placed, sorted.
    pub tickets: Vec<Uuid>,
    pub pools: Vec<NewPool>,
    pub formed_at: OffsetDateTime,
}

/// What asking for a queued ticket did.
pub enum QueuedReservation {
    Reserved(Box<ReservedTicket>),
    /// Tickets are no longer sold.
    Closed,
    /// The queue holds its entry cap in paid and payable tickets.
    Full,
    /// The player already holds as many unpaid tickets as one may.
    TooManyUnpaid,
    /// The player already paid for as many entries as the competition allows one player.
    EntryLimit,
    /// The id names a ticket of another competition or player, or one already used.
    Taken,
}

const TICKET_COLUMNS: &str = "tickets.id as id,
    tickets.event_id as competition_id,
    entries.id as entry_id,
    tickets.ephemeral_pubkey as ephemeral_pubkey,
    encrypted_preimage,
    preimage_ciphertext,
    hash,
    payment_request,
    invoice_expires_at,
    datetime('now', '+10 minutes') as expiry,
    reserved_by,
    reserved_at,
    paid_at,
    settled_at,
    escrow_transaction";

/// A ticket that holds, or may soon hold, a buy-in: paid, reserved in the last ten minutes, or
/// with an invoice that can still be paid.
const LIVE_TICKET: &str = "(paid_at IS NOT NULL
    OR (reserved_by IS NOT NULL
        AND (reserved_at > datetime('now', '-10 minutes')
             OR (payment_request IS NOT NULL AND invoice_cancelled_at IS NULL
                 AND invoice_expires_at > datetime('now')))))";

fn uuid(text: &str) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(text).map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

fn uuids(json: &str) -> Result<Vec<Uuid>, sqlx::Error> {
    serde_json::from_str(json).map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

fn rfc3339(at: OffsetDateTime) -> Result<String, sqlx::Error> {
    at.format(&Rfc3339)
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))
}

fn pool_row(row: &sqlx::sqlite::SqliteRow) -> Result<PoolRecord, sqlx::Error> {
    Ok(PoolRecord {
        competition_id: uuid(&row.try_get::<String, _>("competition_id")?)?,
        parent_id: uuid(&row.try_get::<String, _>("parent_id")?)?,
        pool_index: u32::try_from(row.try_get::<i64, _>("pool_index")?)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
        close_height: u32::try_from(row.try_get::<i64, _>("close_height")?)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
        block_hash: BlockHash::from_str(&row.try_get::<String, _>("block_hash")?)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
        tickets: uuids(&row.try_get::<String, _>("tickets_json")?)?,
        members: uuids(&row.try_get::<String, _>("members_json")?)?,
        formed_at: row.try_get("formed_at")?,
    })
}

impl CompetitionStore {
    /// Store a new queued competition with its settings, in one write. Its entries are held in
    /// Arkade escrows and paid out automatically, as its pools' will be.
    pub async fn add_queued_competition(
        &self,
        competition: &Competition,
        settings: &QueueSettings,
    ) -> Result<(), DatabaseWriteError> {
        let id = competition.id.to_string();
        let created_at = rfc3339(competition.created_at)?;
        let event_submission = serde_json::to_string(&competition.event_submission)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let event_announcement = competition
            .event_announcement
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let pool_rules = serde_json::to_string(&settings.pool_rules)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let terms_json = serde_json::to_string(&settings.terms)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let digest = hex::encode(settings.terms_digest);
        let stake = i64::try_from(settings.stake_sats)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let max_entries = i64::from(settings.max_entries);
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                sqlx::query(
                    "INSERT INTO competitions (id, created_at, event_submission, event_announcement, kind)
                     VALUES (?, ?, ?, ?, 'queued')",
                )
                .bind(&id)
                .bind(&created_at)
                .bind(&event_submission)
                .bind(&event_announcement)
                .execute(&mut *tx)
                .await?;
                sqlx::query("INSERT INTO automatic_payout_competitions(event_id) VALUES (?)")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO ark_funded_competitions(event_id) VALUES (?)")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "INSERT INTO queued_competitions
                        (competition_id, pool_rules, stake_sats, max_entries, terms_json, terms_digest)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(&pool_rules)
                .bind(stake)
                .bind(max_entries)
                .bind(&terms_json)
                .bind(&digest)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    /// A queued competition's settings, or `None` for any other competition.
    ///
    /// The stored digest must be the terms' own, and the settings must be the terms', or the row
    /// is refused: deposits are sealed under the digest players were given.
    pub async fn queue_settings(
        &self,
        competition_id: Uuid,
    ) -> Result<Option<QueueSettings>, sqlx::Error> {
        let Some(row) = sqlx::query(
            "SELECT pool_rules, stake_sats, max_entries, terms_json, terms_digest
             FROM queued_competitions WHERE competition_id = ?",
        )
        .bind(competition_id.to_string())
        .fetch_optional(self.db_connection.read())
        .await?
        else {
            return Ok(None);
        };
        let decode = |message: String| sqlx::Error::Decode(message.into());
        let pool_rules: PoolRules = serde_json::from_str(&row.try_get::<String, _>("pool_rules")?)
            .map_err(|error| decode(error.to_string()))?;
        let terms: QueuedTerms = serde_json::from_str(&row.try_get::<String, _>("terms_json")?)
            .map_err(|error| decode(error.to_string()))?;
        let digest = terms.digest().map_err(|error| decode(error.to_string()))?;
        let stored: String = row.try_get("terms_digest")?;
        let stake_sats = u64::try_from(row.try_get::<i64, _>("stake_sats")?)
            .map_err(|error| decode(error.to_string()))?;
        let max_entries = u32::try_from(row.try_get::<i64, _>("max_entries")?)
            .map_err(|error| decode(error.to_string()))?;
        if hex::encode(digest) != stored
            || terms.competition_id != competition_id
            || terms.pool_rules != pool_rules
            || terms.stake_sats != stake_sats
        {
            return Err(decode(format!(
                "queued competition {competition_id}'s stored terms do not match its digest or settings"
            )));
        }
        Ok(Some(QueueSettings {
            competition_id,
            pool_rules,
            stake_sats,
            max_entries,
            terms,
            terms_digest: digest,
        }))
    }

    /// Reserve a queued competition's ticket `ticket_id` for `player`, creating it if it does not
    /// exist yet.
    ///
    /// A queued competition has no seats: a ticket is made when a player asks for one, with the
    /// id of the entry it pays for. Asking again for the same id returns the same ticket; if its
    /// invoice expired unpaid, it gets a fresh preimage and hash first, as a ticket taken over
    /// does, and `superseded_payment_hash` names the old one. A new ticket is refused once paid
    /// and payable tickets reach `max_entries`, or when the player already holds
    /// `MAX_UNPAID_TICKETS_PER_PLAYER` unpaid ones.
    pub async fn reserve_queued_ticket(
        &self,
        competition_id: Uuid,
        ticket_id: Uuid,
        player: &str,
        max_entries: u32,
        max_per_player: u32,
        deadline: OffsetDateTime,
    ) -> Result<QueuedReservation, DatabaseWriteError> {
        let competition = competition_id.to_string();
        let ticket = ticket_id.to_string();
        let player = player.to_string();
        let preimage = hashlock::preimage_random(&mut rand::rng());
        let hash_hex = hex::encode(hashlock::sha256(&preimage));
        let ciphertext = self
            .seal_preimage(ticket_id, &hash_hex, &preimage)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let preimage_hex =
            super::ticket_preimage::plaintext_column(ciphertext.as_deref(), &preimage);
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                if !before_deadline(Some(deadline)) {
                    tx.rollback().await?;
                    return Ok(QueuedReservation::Closed);
                }
                let existing = sqlx::query(&format!(
                    "SELECT tickets.event_id, tickets.reserved_by, tickets.hash,
                            tickets.paid_at IS NOT NULL AS paid,
                            (tickets.payment_request IS NOT NULL
                             AND (tickets.invoice_expires_at IS NULL
                                  OR tickets.invoice_expires_at <= datetime('now'))) AS expired,
                            {LIVE_TICKET} AS live,
                            EXISTS (SELECT 1 FROM entries WHERE entries.ticket_id = tickets.id) AS used
                     FROM tickets WHERE tickets.id = ?"
                ))
                .bind(&ticket)
                .fetch_optional(&mut *tx)
                .await?;
                let mut superseded_payment_hash = None;
                // A ticket it already holds is its own to pay; a new or released one counts
                // against the entries it may make.
                let paid_by_player = || {
                    sqlx::query_scalar::<_, i64>(super::store::PAID_TICKETS_OF_PLAYER)
                        .bind(&competition)
                        .bind(&player)
                };
                let count_live = |extra: &'static str| {
                    format!("SELECT COUNT(*) FROM tickets WHERE event_id = ? {extra} AND {LIVE_TICKET}")
                };
                match existing {
                    Some(row) => {
                        let event_id: String = row.try_get("event_id")?;
                        let holder: Option<String> = row.try_get("reserved_by")?;
                        let used: bool = row.try_get("used")?;
                        if event_id != competition
                            || used
                            || holder.as_ref().is_some_and(|holder| holder != &player)
                        {
                            tx.rollback().await?;
                            return Ok(QueuedReservation::Taken);
                        }
                        let paid: bool = row.try_get("paid")?;
                        let expired: bool = row.try_get("expired")?;
                        if holder.is_none() {
                            if paid_by_player().fetch_one(&mut *tx).await? >= i64::from(max_per_player) {
                                tx.rollback().await?;
                                return Ok(QueuedReservation::EntryLimit);
                            }
                            // Released unpaid; it counts toward the cap again once reserved.
                            let live: i64 = sqlx::query_scalar(&count_live(""))
                                .bind(&competition)
                                .fetch_one(&mut *tx)
                                .await?;
                            if live >= i64::from(max_entries) {
                                tx.rollback().await?;
                                return Ok(QueuedReservation::Full);
                            }
                            sqlx::query(
                                "UPDATE tickets SET reserved_by = ?, reserved_at = datetime('now')
                                 WHERE id = ? AND reserved_by IS NULL AND paid_at IS NULL",
                            )
                            .bind(&player)
                            .bind(&ticket)
                            .execute(&mut *tx)
                            .await?;
                        } else if !paid && !row.try_get::<bool, _>("live")? {
                            // Its hold lapsed, so it stopped counting toward the cap and others
                            // may have filled the queue since: it is held again only if there is
                            // still room.
                            let live: i64 = sqlx::query_scalar(&count_live(""))
                                .bind(&competition)
                                .fetch_one(&mut *tx)
                                .await?;
                            if live >= i64::from(max_entries) {
                                tx.rollback().await?;
                                return Ok(QueuedReservation::Full);
                            }
                            sqlx::query(
                                "UPDATE tickets SET reserved_at = datetime('now')
                                 WHERE id = ? AND paid_at IS NULL",
                            )
                            .bind(&ticket)
                            .execute(&mut *tx)
                            .await?;
                        }
                        if holder.is_some() && !paid && expired {
                            // Its invoice can no longer be paid: a fresh preimage and hash, so
                            // nothing issued for the old one can pay for this ticket.
                            let old_hash: String = row.try_get("hash")?;
                            sqlx::query(
                                "UPDATE tickets
                                 SET encrypted_preimage = ?, preimage_ciphertext = ?, hash = ?,
                                     reserved_at = datetime('now'),
                                     payment_request = NULL, invoice_expires_at = NULL,
                                     escrow_transaction = NULL, ephemeral_pubkey = NULL
                                 WHERE id = ? AND paid_at IS NULL",
                            )
                            .bind(&preimage_hex)
                            .bind(&ciphertext)
                            .bind(&hash_hex)
                            .bind(&ticket)
                            .execute(&mut *tx)
                            .await?;
                            superseded_payment_hash = Some(old_hash);
                        }
                    }
                    None => {
                        if paid_by_player().fetch_one(&mut *tx).await? >= i64::from(max_per_player) {
                            tx.rollback().await?;
                            return Ok(QueuedReservation::EntryLimit);
                        }
                        let live: i64 = sqlx::query_scalar(&count_live(""))
                            .bind(&competition)
                            .fetch_one(&mut *tx)
                            .await?;
                        if live >= i64::from(max_entries) {
                            tx.rollback().await?;
                            return Ok(QueuedReservation::Full);
                        }
                        let unpaid: i64 = sqlx::query_scalar(&count_live(
                            "AND reserved_by = ? AND paid_at IS NULL",
                        ))
                        .bind(&competition)
                        .bind(&player)
                        .fetch_one(&mut *tx)
                        .await?;
                        if unpaid >= MAX_UNPAID_TICKETS_PER_PLAYER {
                            tx.rollback().await?;
                            return Ok(QueuedReservation::TooManyUnpaid);
                        }
                        sqlx::query(
                            "INSERT INTO tickets (id, event_id, encrypted_preimage, preimage_ciphertext, hash,
                                                  reserved_at, reserved_by)
                             VALUES (?, ?, ?, ?, ?, datetime('now'), ?)",
                        )
                        .bind(&ticket)
                        .bind(&competition)
                        .bind(&preimage_hex)
                        .bind(&ciphertext)
                        .bind(&hash_hex)
                        .bind(&player)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
                // A registration sent for an earlier hash of this ticket is never used.
                sqlx::query(
                    "DELETE FROM ticket_keymeld_registrations
                     WHERE ticket_id = ? AND ticket_hash != (SELECT hash FROM tickets WHERE id = ?)",
                )
                .bind(&ticket)
                .bind(&ticket)
                .execute(&mut *tx)
                .await?;
                let reserved = sqlx::query_as::<_, Ticket>(&format!(
                    "SELECT {TICKET_COLUMNS} FROM tickets
                     LEFT JOIN entries ON tickets.id = entries.ticket_id
                     WHERE tickets.id = ?"
                ))
                .bind(&ticket)
                .fetch_one(&mut *tx)
                .await?;
                if !before_deadline(Some(deadline)) {
                    tx.rollback().await?;
                    return Ok(QueuedReservation::Closed);
                }
                tx.commit().await?;
                Ok(QueuedReservation::Reserved(Box::new(ReservedTicket {
                    ticket: reserved,
                    superseded_payment_hash,
                })))
            })
            .await
    }

    /// Paid entries of a queued competition, in the queue and in the pools it formed.
    /// The tickets that count against a queued competition's entry cap: paid, or held for an
    /// unexpired invoice, as [`Self::reserve_queued_ticket`] counts them.
    pub async fn queued_held_count(&self, competition_id: Uuid) -> Result<u64, sqlx::Error> {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM tickets WHERE event_id = ? AND {LIVE_TICKET}"
        ))
        .bind(competition_id.to_string())
        .fetch_one(self.db_connection.read())
        .await?;
        u64::try_from(count).map_err(|error| sqlx::Error::Decode(Box::new(error)))
    }

    pub async fn queued_entry_count(&self, competition_id: Uuid) -> Result<u64, sqlx::Error> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM entries e JOIN tickets t ON t.id = e.ticket_id
             WHERE t.paid_at IS NOT NULL
               AND (e.event_id = ?1
                    OR e.event_id IN (SELECT competition_id FROM competition_pools WHERE parent_id = ?1))",
        )
        .bind(competition_id.to_string())
        .fetch_one(self.db_connection.read())
        .await?;
        u64::try_from(count).map_err(|error| sqlx::Error::Decode(Box::new(error)))
    }

    /// The tickets of a queued competition that can play, sorted: paid, their escrow VTXO funded,
    /// an entry submitted under the ticket's own id with its accepted payout policy, and the
    /// player's key deposit stored for the ticket's current hash.
    pub async fn complete_queued_tickets(
        &self,
        competition_id: Uuid,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT t.id FROM tickets t
             JOIN entries e ON e.ticket_id = t.id AND e.event_id = t.event_id AND e.id = t.id
             JOIN entry_payout_policies p ON p.entry_id = e.id
             JOIN ticket_ark_escrows x ON x.ticket_id = t.id AND x.ticket_hash = t.hash
             JOIN ticket_keymeld_registrations r ON r.ticket_id = t.id AND r.ticket_hash = t.hash
             WHERE t.event_id = ? AND t.paid_at IS NOT NULL
               AND x.funded_at IS NOT NULL AND x.vtxo_outpoint IS NOT NULL
             ORDER BY t.id",
        )
        .bind(competition_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?;
        ids.iter().map(|id| uuid(id)).collect()
    }

    /// Form a queued competition's pools in one write: create each pool's competition, already
    /// escrow-confirmed, Arkade-funded and paid out automatically; move each pool's tickets and
    /// entries, and each entry's oracle submission, to it; record the formation; and mark the
    /// queued competition as formed.
    ///
    /// Only while `lease` holds the queued competition and it has not formed pools, been
    /// cancelled or failed. Returns whether this call formed them; a repeat forms nothing. Any
    /// ticket or entry that is not where the formation expects it aborts the whole write.
    pub async fn form_queued_pools(
        &self,
        formation: PoolFormation,
        lease: &Lease,
    ) -> Result<bool, DatabaseWriteError> {
        let parent = formation.parent_id.to_string();
        let formed_at = rfc3339(formation.formed_at)?;
        let formed_unix = formation.formed_at.unix_timestamp();
        let tickets_json = serde_json::to_string(&formation.tickets)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let block_hash = formation.block_hash.to_string();
        let close_height = i64::from(formation.close_height);
        let mut pools = Vec::with_capacity(formation.pools.len());
        for pool in &formation.pools {
            pools.push((
                pool.competition_id.to_string(),
                i64::from(pool.pool_index),
                pool.members.clone(),
                serde_json::to_string(&pool.members)
                    .map_err(|error| sqlx::Error::Encode(Box::new(error)))?,
                serde_json::to_string(&pool.event_submission)
                    .map_err(|error| sqlx::Error::Encode(Box::new(error)))?,
            ));
        }
        let lease = lease.clone();
        self.db_connection
            .execute_write(move |db| async move {
                let mut tx = db.begin().await?;
                let eligible: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM competitions
                        WHERE id = ? AND kind = 'queued' AND pools_formed_at IS NULL
                          AND cancelled_at IS NULL AND failed_at IS NULL)
                     AND EXISTS (SELECT 1 FROM leases WHERE resource = ? AND holder = ? AND token = ?)",
                )
                .bind(&parent)
                .bind(&lease.resource)
                .bind(&lease.holder)
                .bind(lease.token)
                .fetch_one(&mut *tx)
                .await?;
                if !eligible {
                    tx.rollback().await?;
                    return Ok(false);
                }
                let misplaced = |what: &str, id: &Uuid| {
                    sqlx::Error::Protocol(format!(
                        "{what} {id} is not a paid ticket of queued competition {parent}"
                    ))
                };
                for (child, index, members, members_json, event_submission) in &pools {
                    sqlx::query(
                        "INSERT INTO competitions
                            (id, created_at, event_submission, kind, parent_id, pool_index,
                             escrow_funds_confirmed_at)
                         VALUES (?, ?, ?, 'pool', ?, ?, ?)",
                    )
                    .bind(child)
                    .bind(&formed_at)
                    .bind(event_submission)
                    .bind(&parent)
                    .bind(index)
                    .bind(&formed_at)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("INSERT INTO automatic_payout_competitions(event_id) VALUES (?)")
                        .bind(child)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("INSERT INTO ark_funded_competitions(event_id) VALUES (?)")
                        .bind(child)
                        .execute(&mut *tx)
                        .await?;
                    for member in members {
                        let moved = sqlx::query(
                            "UPDATE tickets SET event_id = ?
                             WHERE id = ? AND event_id = ? AND paid_at IS NOT NULL",
                        )
                        .bind(child)
                        .bind(member.to_string())
                        .bind(&parent)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                        if moved != 1 {
                            return Err(misplaced("ticket", member));
                        }
                        let submission: Option<Vec<u8>> = sqlx::query_scalar(
                            "SELECT entry_submission FROM entries WHERE ticket_id = ? AND event_id = ?",
                        )
                        .bind(member.to_string())
                        .bind(&parent)
                        .fetch_optional(&mut *tx)
                        .await?;
                        let submission = submission.ok_or_else(|| misplaced("entry of ticket", member))?;
                        let mut submission: AddEventEntry = serde_json::from_slice(&submission)
                            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                        submission.event_id = Uuid::parse_str(child)
                            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                        let submission = serde_json::to_string(&submission)
                            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
                        let moved = sqlx::query(
                            "UPDATE entries SET event_id = ?, entry_submission = ?
                             WHERE ticket_id = ? AND event_id = ?",
                        )
                        .bind(child)
                        .bind(submission)
                        .bind(member.to_string())
                        .bind(&parent)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                        if moved != 1 {
                            return Err(misplaced("entry of ticket", member));
                        }
                    }
                    sqlx::query(
                        "INSERT INTO competition_pools
                            (competition_id, parent_id, pool_index, close_height, block_hash,
                             tickets_json, members_json, formed_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(child)
                    .bind(&parent)
                    .bind(index)
                    .bind(close_height)
                    .bind(&block_hash)
                    .bind(&tickets_json)
                    .bind(members_json)
                    .bind(formed_unix)
                    .execute(&mut *tx)
                    .await?;
                }
                let marked = sqlx::query(
                    "UPDATE competitions SET pools_formed_at = ?
                     WHERE id = ? AND kind = 'queued' AND pools_formed_at IS NULL",
                )
                .bind(&formed_at)
                .bind(&parent)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if marked != 1 {
                    return Err(sqlx::Error::Protocol(format!(
                        "queued competition {parent} changed while its pools formed"
                    )));
                }
                tx.commit().await?;
                Ok(true)
            })
            .await
    }

    /// The pools a queued competition formed, by index.
    pub async fn competition_pools(&self, parent_id: Uuid) -> Result<Vec<PoolRecord>, sqlx::Error> {
        sqlx::query("SELECT * FROM competition_pools WHERE parent_id = ? ORDER BY pool_index")
            .bind(parent_id.to_string())
            .fetch_all(self.db_connection.read())
            .await?
            .iter()
            .map(pool_row)
            .collect()
    }

    /// How a pool was formed, or `None` for a competition that is not a pool.
    pub async fn pool_record(&self, pool_id: Uuid) -> Result<Option<PoolRecord>, sqlx::Error> {
        sqlx::query("SELECT * FROM competition_pools WHERE competition_id = ?")
            .bind(pool_id.to_string())
            .fetch_optional(self.db_connection.read())
            .await?
            .as_ref()
            .map(pool_row)
            .transpose()
    }

    /// Mark queued competitions finished once every pool they formed has finished: completed,
    /// or cancelled before its contract was funded. A pool cancelled after funding may still
    /// settle, so it is not finished. Nothing about money changes here: a queue's leftover
    /// tickets are refunded by cleanup, which keys on `pools_formed_at`, as before.
    /// Returns how many were marked.
    pub async fn finish_formed_queues(&self) -> Result<u64, DatabaseWriteError> {
        let now = rfc3339(OffsetDateTime::now_utc())?;
        self.db_connection
            .execute_write(move |pool| async move {
                Ok(sqlx::query(
                    "UPDATE competitions SET pools_finished_at = ?
                     WHERE kind = 'queued' AND pools_formed_at IS NOT NULL
                       AND pools_finished_at IS NULL
                       AND cancelled_at IS NULL AND failed_at IS NULL
                       AND EXISTS (SELECT 1 FROM competitions pools
                                   WHERE pools.parent_id = competitions.id)
                       AND NOT EXISTS (SELECT 1 FROM competitions pools
                                       WHERE pools.parent_id = competitions.id
                                         AND pools.completed_at IS NULL
                                         AND (pools.cancelled_at IS NULL
                                              OR pools.funding_confirmed_at IS NOT NULL))",
                )
                .bind(now)
                .execute(&pool)
                .await?
                .rows_affected())
            })
            .await
    }

    /// Cancel a queued competition that formed no pools, under its lease.
    pub async fn cancel_queued_competition(
        &self,
        competition_id: Uuid,
        lease: &Lease,
    ) -> Result<bool, DatabaseWriteError> {
        let id = competition_id.to_string();
        let now = rfc3339(OffsetDateTime::now_utc())?;
        let lease = lease.clone();
        self.db_connection
            .execute_write(move |pool| async move {
                let changed = sqlx::query(
                    "UPDATE competitions SET cancelled_at = ?
                     WHERE id = ? AND kind = 'queued' AND pools_formed_at IS NULL
                       AND cancelled_at IS NULL AND failed_at IS NULL
                       AND EXISTS (SELECT 1 FROM leases WHERE resource = ? AND holder = ? AND token = ?)",
                )
                .bind(now)
                .bind(id)
                .bind(lease.resource)
                .bind(lease.holder)
                .bind(lease.token)
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed == 1)
            })
            .await
    }
}
