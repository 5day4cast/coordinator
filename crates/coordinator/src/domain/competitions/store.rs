use dlctix::{bitcoin::XOnlyPublicKey, hashlock, musig2::PubNonce, SigMap};
use log::debug;
use sqlx::{Execute, Sqlite};
use std::collections::HashMap;
use std::sync::Arc;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::{
    api::routes::FinalSignatures,
    domain::{EntryPayout, PayoutError, PayoutStatus},
    infra::{
        db::{DBConnection, DatabaseWriteError},
        keymeld::StoredDlcKeygenSession,
    },
};

use super::{
    admission::{before_deadline, within_entry_window},
    ticket_preimage::stored_preimage,
    ticket_registration::lapsed_ticket_ids,
    Competition, EntryStatus, SearchBy, Ticket, TicketCipher, UserEntry,
};

/// A ticket reserved for a caller. When a stale reservation was taken over,
/// the ticket has already been given a fresh preimage and hash, and
/// `superseded_payment_hash` is the previous holder's invoice hash for the
/// caller to cancel.
pub struct ReservedTicket {
    pub ticket: Ticket,
    pub superseded_payment_hash: Option<String>,
}

/// What saving an entry before its deadline did.
#[derive(Debug)]
pub enum EntryAdmission {
    Added(Box<UserEntry>),
    /// The deadline passed.
    Closed,
    /// The player already has as many entries as the competition allows one player.
    EntryLimit,
    /// The hour its entry id allows passed: its ticket has lapsed (see [`super::LapsedTicket`]).
    Lapsed,
}

impl EntryAdmission {
    /// The entry, if it was saved.
    pub fn added(self) -> Option<UserEntry> {
        match self {
            EntryAdmission::Added(entry) => Some(*entry),
            EntryAdmission::Closed | EntryAdmission::EntryLimit | EntryAdmission::Lapsed => None,
        }
    }
}

/// What asking for a competition's ticket before its deadline did.
pub enum TicketReservation {
    Reserved(Box<ReservedTicket>),
    /// The deadline passed: tickets are no longer sold.
    Closed,
    /// The player already paid for as many entries as the competition allows one player.
    EntryLimit,
    /// As [`Self::EntryLimit`], with one of those tickets lapsed (see [`super::LapsedTicket`]): a
    /// single competition's lapsed ticket keeps its seat, so it still counts as the player's
    /// entry, though it can no longer be entered.
    Lapsed,
}

impl TicketReservation {
    /// The ticket, if one was reserved.
    pub fn reserved(self) -> Option<ReservedTicket> {
        match self {
            TicketReservation::Reserved(reserved) => Some(*reserved),
            TicketReservation::Closed
            | TicketReservation::EntryLimit
            | TicketReservation::Lapsed => None,
        }
    }
}

/// How many tickets `reserved_by` has paid for in `event_id`, entered or not, lapsed or not: what a
/// single competition's entries-per-player limit counts. A queued competition's leaves out the
/// lapsed ones (see [`paid_entries_of_player`]).
const PAID_TICKETS_OF_PLAYER: &str =
    "SELECT COUNT(*) FROM tickets WHERE event_id = ? AND reserved_by = ? AND paid_at IS NOT NULL";

/// How many of the entries queued competition `event_id` allows one player `player` has taken at
/// `now`: a paid ticket takes one, entered or with its entry on its way, unless it lapsed (see
/// [`super::LapsedTicket`]). Read inside the write that issues a ticket.
pub(super) async fn paid_entries_of_player(
    connection: &mut sqlx::SqliteConnection,
    event_id: &str,
    player: &str,
    now: OffsetDateTime,
) -> Result<i64, sqlx::Error> {
    let paid: i64 = sqlx::query_scalar(PAID_TICKETS_OF_PLAYER)
        .bind(event_id)
        .bind(player)
        .fetch_one(&mut *connection)
        .await?;
    let lapsed = lapsed_ticket_ids(&mut *connection, event_id, Some(player), now).await?;
    Ok(paid - lapsed.len() as i64)
}

#[derive(Debug, Clone)]
pub struct CompetitionStore {
    pub(super) db_connection: DBConnection,
    /// Seals ticket preimages. Without it (tests and tools) tickets keep only the plaintext
    /// column; see `ticket_preimage.rs`.
    pub(super) ticket_cipher: Option<Arc<TicketCipher>>,
}

impl CompetitionStore {
    pub fn new(db_connection: DBConnection) -> Self {
        Self {
            db_connection,
            ticket_cipher: None,
        }
    }

    pub fn with_ticket_cipher(mut self, cipher: Arc<TicketCipher>) -> Self {
        self.ticket_cipher = Some(cipher);
        self
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        self.db_connection.ping().await
    }

    pub async fn quick_check(&self) -> Result<(), sqlx::Error> {
        self.db_connection.quick_check().await
    }

    pub async fn get_stored_public_key(&self) -> Result<XOnlyPublicKey, sqlx::Error> {
        let key_bytes: Vec<u8> = sqlx::query_scalar("SELECT pubkey FROM coordinator_metadata")
            .fetch_one(self.db_connection.read())
            .await?;

        let converted_key =
            XOnlyPublicKey::from_slice(&key_bytes).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

        Ok(converted_key)
    }

    pub async fn add_coordinator_metadata(
        &self,
        name: String,
        pubkey: XOnlyPublicKey,
    ) -> Result<(), DatabaseWriteError> {
        let pubkey_raw = pubkey.serialize().to_vec();

        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query("INSERT INTO coordinator_metadata (pubkey, name) VALUES (?, ?)")
                    .bind(&pubkey_raw[..])
                    .bind(&name)
                    .execute(&pool)
                    .await?;
                Ok(())
            })
            .await
    }

    pub async fn add_entry(
        &self,
        entry: UserEntry,
        ticket_id: Uuid,
    ) -> Result<UserEntry, DatabaseWriteError> {
        self.add_entry_with_policy(entry, ticket_id, None).await
    }

    pub async fn add_entry_with_policy(
        &self,
        entry: UserEntry,
        ticket_id: Uuid,
        payout_policy: Option<String>,
    ) -> Result<UserEntry, DatabaseWriteError> {
        // Unbounded callers are internal storage operations; HTTP admission uses the
        // deadline-bearing variant so queueing cannot admit a late entry.
        match self
            .insert_entry(entry, ticket_id, payout_policy, None, None, u32::MAX)
            .await?
        {
            EntryAdmission::Added(entry) => Ok(*entry),
            _ => unreachable!("an unbounded entry write has no deadline or limit"),
        }
    }

    /// Saves the entry unless the deadline passed, the hour its entry id allows ended at
    /// `finish_by` (when it has one: a competition with automatic payouts), or its player already
    /// has `max_per_player` entries in the competition.
    pub(super) async fn add_entry_with_policy_before(
        &self,
        entry: UserEntry,
        ticket_id: Uuid,
        payout_policy: Option<String>,
        deadline: OffsetDateTime,
        finish_by: Option<OffsetDateTime>,
        max_per_player: u32,
    ) -> Result<EntryAdmission, DatabaseWriteError> {
        self.insert_entry(
            entry,
            ticket_id,
            payout_policy,
            Some(deadline),
            finish_by,
            max_per_player,
        )
        .await
    }

    async fn insert_entry(
        &self,
        entry: UserEntry,
        ticket_id: Uuid,
        payout_policy: Option<String>,
        deadline: Option<OffsetDateTime>,
        finish_by: Option<OffsetDateTime>,
        max_per_player: u32,
    ) -> Result<EntryAdmission, DatabaseWriteError> {
        debug!("adding entry {} for ticket {}", entry.id, ticket_id);

        let entry_submission = serde_json::to_string(&entry.entry_submission)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let entry_id = entry.id.to_string();
        let ticket_id_str = ticket_id.to_string();
        let event_id = entry.event_id.to_string();
        let pubkey = entry.pubkey.clone();
        let ephemeral_pubkey = entry.ephemeral_pubkey.clone();
        let payout_hash = entry.payout_hash.clone();
        let encrypted_keymeld_private_key = entry.encrypted_keymeld_private_key.clone();
        let keymeld_auth_pubkey = entry.keymeld_auth_pubkey.clone();
        let keymeld_registration_context = entry
            .keymeld_registration_context
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;

        let keymeld_escrow_policy = entry
            .keymeld_escrow_policy
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;

        // The refusal, if the write refused the entry.
        let refused = self
            .db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                if !before_deadline(deadline) {
                    tx.rollback().await?;
                    return Ok(Some(EntryAdmission::Closed));
                }
                // Once its hour has passed the ticket has lapsed: it no longer counts against
                // the competition's places or its player's entries, so another may have taken
                // its place. Checked here, where writes are serialized, as well as before.
                if !within_entry_window(finish_by) {
                    tx.rollback().await?;
                    return Ok(Some(EntryAdmission::Lapsed));
                }
                // Tickets bound the entries a player gets; this catches tickets paid at once.
                // Another entry for this same ticket is left to the unique index, which the
                // caller reads as a retry.
                let entered: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM entries WHERE event_id = ? AND pubkey = ? AND ticket_id != ?",
                )
                .bind(&event_id)
                .bind(&pubkey)
                .bind(&ticket_id_str)
                .fetch_one(&mut *tx)
                .await?;
                if entered >= i64::from(max_per_player) {
                    tx.rollback().await?;
                    return Ok(Some(EntryAdmission::EntryLimit));
                }
                sqlx::query(
                    "INSERT INTO entries (
                        id,
                        ticket_id,
                        event_id,
                        pubkey,
                        ephemeral_pubkey,
                        payout_hash,
                        entry_submission,
                        encrypted_keymeld_private_key,
                        keymeld_auth_pubkey,
                        keymeld_registration_context,
                        keymeld_escrow_policy
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&entry_id)
                .bind(&ticket_id_str)
                .bind(&event_id)
                .bind(&pubkey)
                .bind(ephemeral_pubkey)
                .bind(payout_hash)
                .bind(entry_submission)
                .bind(encrypted_keymeld_private_key)
                .bind(keymeld_auth_pubkey)
                .bind(keymeld_registration_context)
                .bind(keymeld_escrow_policy)
                .execute(&mut *tx)
                .await?;
                if let Some(policy) = payout_policy {
                    sqlx::query(
                        "INSERT INTO entry_payout_policies(entry_id, policy_json) VALUES (?, ?)",
                    )
                    .bind(entry_id)
                    .bind(policy)
                    .execute(&mut *tx)
                    .await?;
                }
                if !before_deadline(deadline) {
                    tx.rollback().await?;
                    return Ok(Some(EntryAdmission::Closed));
                }
                if !within_entry_window(finish_by) {
                    tx.rollback().await?;
                    return Ok(Some(EntryAdmission::Lapsed));
                }
                tx.commit().await?;
                Ok(None)
            })
            .await?;

        Ok(match refused {
            Some(refusal) => refusal,
            None => EntryAdmission::Added(Box::new(entry)),
        })
    }

    pub async fn add_final_signatures(
        &self,
        entry_id: Uuid,
        final_signatures: FinalSignatures,
    ) -> Result<bool, DatabaseWriteError> {
        let sigs_json = serde_json::to_string(&final_signatures.partial_signatures)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let entry_id_str = entry_id.to_string();
        let funding_psbt = final_signatures.funding_psbt_base64.clone();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE entries
                    SET partial_signatures = ?,
                        funding_psbt_base64 = ?,
                        signed_at = datetime('now')
                    WHERE id = ?
                      AND partial_signatures IS NULL",
                )
                .bind(sigs_json)
                .bind(funding_psbt)
                .bind(entry_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    pub async fn add_public_nonces(
        &self,
        entry_id: Uuid,
        public_nonces: SigMap<PubNonce>,
    ) -> Result<bool, DatabaseWriteError> {
        let nonces_json =
            serde_json::to_string(&public_nonces).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let entry_id_str = entry_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE entries
                    SET public_nonces = ?
                    WHERE id = ?
                      AND public_nonces IS NULL",
                )
                .bind(nonces_json)
                .bind(entry_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    pub async fn mark_entry_sellback_broadcast(
        &self,
        entry_id: Uuid,
        broadcast_time: OffsetDateTime,
    ) -> Result<bool, DatabaseWriteError> {
        let broadcast_time_str = broadcast_time
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let entry_id_str = entry_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE entries
                    SET sellback_broadcasted_at = ?
                    WHERE id = ?",
                )
                .bind(broadcast_time_str)
                .bind(entry_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    pub async fn mark_entry_reclaim_broadcast(
        &self,
        entry_id: Uuid,
        broadcast_time: OffsetDateTime,
    ) -> Result<bool, DatabaseWriteError> {
        let broadcast_time_str = broadcast_time
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let entry_id_str = entry_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE entries
                    SET reclaimed_broadcasted_at = ?
                    WHERE id = ?",
                )
                .bind(broadcast_time_str)
                .bind(entry_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// Record that the entry's split output was left on chain because sweeping it would leave
    /// less than the dust limit. The first time is kept.
    pub async fn mark_entry_sweep_uneconomic(
        &self,
        entry_id: Uuid,
        skipped_at: OffsetDateTime,
    ) -> Result<bool, DatabaseWriteError> {
        let skipped_at_str = skipped_at
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let entry_id_str = entry_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE entries
                    SET sweep_uneconomic_at = ?
                    WHERE id = ? AND sweep_uneconomic_at IS NULL",
                )
                .bind(skipped_at_str)
                .bind(entry_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// Update the keymeld_auth_pubkey for an entry.
    /// This is called after the keygen session is created and the user has derived their auth pubkey.
    pub async fn update_keymeld_auth_pubkey(
        &self,
        entry_id: Uuid,
        keymeld_auth_pubkey: String,
    ) -> Result<bool, DatabaseWriteError> {
        let entry_id_str = entry_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE entries
                    SET keymeld_auth_pubkey = ?
                    WHERE id = ?",
                )
                .bind(keymeld_auth_pubkey)
                .bind(entry_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    pub async fn store_payout_info_pending(
        &self,
        entry_id: Uuid,
        payout_preimage: String,
        ephemeral_private_key: String,
        ln_invoice: String,
        payout_amount_sats: u64,
    ) -> Result<Uuid, DatabaseWriteError> {
        let payout_id = Uuid::now_v7();
        let initiated_at = OffsetDateTime::now_utc();
        let entry_id_str = entry_id.to_string();
        let payout_id_str = payout_id.to_string();
        let initiated_at_str = initiated_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();

        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;

                sqlx::query(
                    "INSERT INTO payouts (
                        id,
                        entry_id,
                        payout_payment_request,
                        payout_amount_sats,
                        initiated_at,
                        succeed_at,
                        failed_at,
                        error
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&payout_id_str)
                .bind(&entry_id_str)
                .bind(&ln_invoice)
                .bind(payout_amount_sats as i64)
                .bind(&initiated_at_str)
                .bind(None::<String>) // succeed_at
                .bind(None::<String>) // failed_at
                .bind(None::<String>)
                .execute(&mut *tx)
                .await?;

                if let Ok(hash) =
                    crate::infra::lightning::extract_payment_hash_from_invoice(&ln_invoice)
                {
                    sqlx::query(
                        "INSERT INTO payout_payment_hashes(payment_hash, payout_id) VALUES (?, ?)",
                    )
                    .bind(hash)
                    .bind(&payout_id_str)
                    .execute(&mut *tx)
                    .await?;
                }
                sqlx::query(
                    "UPDATE entries
                    SET payout_preimage = ?,
                        ephemeral_privatekey = ?
                    WHERE id = ?",
                )
                .bind(&payout_preimage)
                .bind(&ephemeral_private_key)
                .bind(&entry_id_str)
                .execute(&mut *tx)
                .await?;

                tx.commit().await?;
                Ok(payout_id)
            })
            .await
    }

    /// `payment_preimage` is the proof of payment LND reports on settlement;
    /// a later report never erases one already stored.
    pub async fn mark_payout_succeeded(
        &self,
        payout_id: Uuid,
        succeed_at: OffsetDateTime,
        payment_preimage: Option<String>,
    ) -> Result<(), DatabaseWriteError> {
        let succeed_at_str = succeed_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let payout_id_str = payout_id.to_string();

        let newly_succeeded = self
            .db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                let was_open: Option<bool> = sqlx::query_scalar(
                    "SELECT succeed_at IS NULL AND failed_at IS NULL FROM payouts WHERE id = ?",
                )
                .bind(&payout_id_str)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(proof) = payment_preimage.as_ref() {
                    let invoice: String = sqlx::query_scalar(
                        "SELECT payout_payment_request FROM payouts WHERE id = ?",
                    )
                    .bind(&payout_id_str)
                    .fetch_one(&mut *tx)
                    .await?;
                    let hash = crate::infra::lightning::extract_payment_hash_from_invoice(&invoice)
                        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
                    super::verify_payout_preimage(proof, &hash)
                        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
                }
                sqlx::query(
                    "UPDATE payouts
                    SET succeed_at = COALESCE(succeed_at, ?),
                        payment_preimage = COALESCE(payment_preimage, ?)
                    WHERE id = ? AND failed_at IS NULL",
                )
                .bind(succeed_at_str)
                .bind(payment_preimage)
                .bind(payout_id_str)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(was_open.unwrap_or(false))
            })
            .await?;
        if newly_succeeded {
            crate::metrics::record_payout_result(true);
        }
        Ok(())
    }

    /// Returns whether this call failed the payout; one already failed or succeeded is kept.
    pub async fn mark_payout_failed(
        &self,
        payout_id: Uuid,
        failed_at: OffsetDateTime,
        error: PayoutError,
    ) -> Result<bool, DatabaseWriteError> {
        let error_blob =
            serde_json::to_string(&error).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let failed_at_str = failed_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let payout_id_str = payout_id.to_string();

        let newly_failed = self
            .db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE payouts
                    SET failed_at = ?, error = ?
                    WHERE id = ? AND succeed_at IS NULL AND failed_at IS NULL",
                )
                .bind(failed_at_str)
                .bind(error_blob)
                .bind(payout_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await?;
        if newly_failed {
            crate::metrics::record_payout_result(false);
        }
        Ok(newly_failed)
    }

    pub async fn get_payout(&self, payout_id: Uuid) -> Result<Option<EntryPayout>, sqlx::Error> {
        sqlx::query_as::<_, EntryPayout>(
            "SELECT
                id,
                entry_id,
                payout_payment_request,
                payout_amount_sats,
                initiated_at,
                succeed_at,
                failed_at,
                error,
                payment_preimage
            FROM payouts
            WHERE id = ?",
        )
        .bind(payout_id.to_string())
        .fetch_optional(self.db_connection.read())
        .await
    }

    pub async fn get_all_pending_payouts(&self) -> Result<Vec<EntryPayout>, sqlx::Error> {
        let entry_payouts = sqlx::query_as::<_, EntryPayout>(
            "SELECT
                id,
                entry_id,
                payout_payment_request,
                payout_amount_sats,
                initiated_at,
                succeed_at,
                failed_at,
                error,
                payment_preimage
            FROM payouts
            WHERE succeed_at IS NULL AND failed_at IS NULL
            ORDER BY initiated_at ASC",
        )
        .fetch_all(self.db_connection.read())
        .await?;

        Ok(entry_payouts)
    }

    pub async fn get_payout_by_payment_hash(
        &self,
        payment_hash: &str,
    ) -> Result<Option<EntryPayout>, sqlx::Error> {
        use crate::infra::lightning::extract_payment_hash_from_invoice;

        let pending_payouts = self.get_all_pending_payouts().await?;

        for payout in pending_payouts {
            if let Ok(hash) = extract_payment_hash_from_invoice(&payout.payout_payment_request) {
                if hash == payment_hash {
                    return Ok(Some(payout));
                }
            }
        }

        Ok(None)
    }

    pub async fn get_entry_payouts(
        &self,
        entry_id: Uuid,
        status_filter: Option<PayoutStatus>,
    ) -> Result<Vec<EntryPayout>, sqlx::Error> {
        let mut query_builder = sqlx::QueryBuilder::<Sqlite>::new(
            "SELECT
                id,
                entry_id,
                payout_payment_request,
                payout_amount_sats,
                initiated_at,
                succeed_at,
                failed_at,
                error,
                payment_preimage
            FROM payouts
            WHERE entry_id = ",
        );

        query_builder.push_bind(entry_id.to_string());

        match status_filter {
            Some(PayoutStatus::Pending) => {
                query_builder.push(" AND succeed_at IS NULL AND failed_at IS NULL");
            }
            Some(PayoutStatus::Succeeded) => {
                query_builder.push(" AND succeed_at IS NOT NULL");
            }
            Some(PayoutStatus::Failed) => {
                query_builder.push(" AND failed_at IS NOT NULL");
            }
            None => {
                // No additional conditions
            }
        }

        query_builder.push(" ORDER BY initiated_at DESC");

        let query = query_builder.build();

        let entry_payouts = sqlx::query_as::<_, EntryPayout>(query.sql())
            .fetch_all(self.db_connection.read())
            .await?;

        Ok(entry_payouts)
    }

    pub async fn get_competition_entries(
        &self,
        event_id: Uuid,
        statuses: Vec<EntryStatus>,
    ) -> Result<Vec<UserEntry>, sqlx::Error> {
        let mut base_query = String::from(
            "WITH latest_payouts AS (
                  SELECT
                      entry_id,
                      payout_payment_request,
                      ROW_NUMBER() OVER (
                          PARTITION BY entry_id
                          ORDER BY COALESCE(succeed_at, initiated_at) DESC
                      ) as rn,
                      COALESCE(succeed_at, initiated_at) as latest_payout_time
                  FROM payouts
                  WHERE failed_at IS NULL
              )
            SELECT
                entries.id as id,
                ticket_id,
                entries.event_id as event_id,
                pubkey,
                entries.ephemeral_pubkey as ephemeral_pubkey,
                ephemeral_privatekey,
                encrypted_keymeld_private_key,
                keymeld_auth_pubkey,
                keymeld_registration_context,
                keymeld_escrow_policy,
                public_nonces,
                partial_signatures,
                funding_psbt_base64,
                entry_submission,
                payout_hash,
                payout_preimage,
                signed_at,
                tickets.settled_at AS paid_at,
                sellback_broadcasted_at,
                reclaimed_broadcasted_at,
                sweep_uneconomic_at,
                split_output_spent_at,
                latest_payouts.latest_payout_time as paid_out_at,
                latest_payouts.payout_payment_request as payout_ln_invoice
            FROM entries
            LEFT JOIN tickets ON entries.ticket_id = tickets.id
            LEFT JOIN latest_payouts ON entries.id = latest_payouts.entry_id AND latest_payouts.rn = 1
            WHERE entries.event_id = ?",
        );

        // Add status filtering
        if !statuses.is_empty() {
            for status in statuses {
                match status {
                    EntryStatus::Paid => {
                        base_query.push_str(" AND tickets.paid_at IS NOT NULL");
                    }
                    EntryStatus::Signed => {
                        base_query.push_str(" AND signed_at IS NOT NULL");
                    }
                }
            }
        }

        let user_entries = sqlx::query_as::<_, UserEntry>(&base_query)
            .bind(event_id.to_string())
            .fetch_all(self.db_connection.read())
            .await?;

        Ok(user_entries)
    }

    /// Submitted entries per competition, without loading picks, keys or payment records.
    pub async fn player_entry_counts(
        &self,
        pubkey: &str,
    ) -> Result<HashMap<String, i64>, sqlx::Error> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT event_id, COUNT(*) FROM entries WHERE pubkey = ? GROUP BY event_id",
        )
        .bind(pubkey)
        .fetch_all(self.db_connection.read())
        .await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn get_user_entries(
        &self,
        pubkey: String,
        filter: SearchBy,
    ) -> Result<Vec<UserEntry>, sqlx::Error> {
        self.get_user_entries_selected(pubkey, filter, None).await
    }

    pub async fn get_user_entries_selected(
        &self,
        pubkey: String,
        filter: SearchBy,
        ids: Option<&[Uuid]>,
    ) -> Result<Vec<UserEntry>, sqlx::Error> {
        if ids.is_some_and(|ids| ids.is_empty()) {
            return Ok(Vec::new());
        }
        let base_query = "WITH latest_payouts AS (
              SELECT
                  entry_id,
                  payout_payment_request,
                  ROW_NUMBER() OVER (
                      PARTITION BY entry_id
                      ORDER BY COALESCE(succeed_at, initiated_at) DESC
                  ) as rn,
                  COALESCE(succeed_at, initiated_at) as latest_payout_time
              FROM payouts
              WHERE failed_at IS NULL
          )
          SELECT
              entries.id as id,
              ticket_id,
              entries.event_id as event_id,
              pubkey,
              entries.ephemeral_pubkey as ephemeral_pubkey,
              ephemeral_privatekey,
              encrypted_keymeld_private_key,
              keymeld_auth_pubkey,
              keymeld_registration_context,
              keymeld_escrow_policy,
              public_nonces,
              partial_signatures,
              funding_psbt_base64,
              entry_submission,
              payout_hash,
              payout_preimage,
              signed_at,
              tickets.paid_at AS paid_at,
              sellback_broadcasted_at,
              reclaimed_broadcasted_at,
              sweep_uneconomic_at,
              split_output_spent_at,
              latest_payouts.latest_payout_time as paid_out_at,
              latest_payouts.payout_payment_request as payout_ln_invoice
          FROM entries
          LEFT JOIN tickets ON entries.ticket_id = tickets.id
          LEFT JOIN latest_payouts ON entries.id = latest_payouts.entry_id AND latest_payouts.rn = 1
          WHERE pubkey = ?";

        let (mut final_query, mut params) = if let Some(event_ids) = filter.event_ids {
            if !event_ids.is_empty() {
                let placeholders = event_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let query = format!("{} AND entries.event_id IN ({})", base_query, placeholders);
                let mut all_params = vec![pubkey];
                all_params.extend(event_ids.into_iter().map(|id| id.to_string()));
                (query, all_params)
            } else {
                (base_query.to_string(), vec![pubkey])
            }
        } else {
            (base_query.to_string(), vec![pubkey])
        };

        if let Some(ids) = ids {
            final_query.push_str(&format!(
                " AND entries.id IN ({})",
                vec!["?"; ids.len()].join(",")
            ));
            params.extend(ids.iter().map(Uuid::to_string));
        }
        final_query.push_str(" ORDER BY entries.id DESC");
        let mut query_builder = sqlx::query_as::<_, UserEntry>(&final_query);

        for param in params {
            query_builder = query_builder.bind(param);
        }

        let user_entries = query_builder.fetch_all(self.db_connection.read()).await?;

        Ok(user_entries)
    }

    /// Lightweight query for the entries list page.
    /// Joins entries with competitions to get observation dates and payout status
    /// in a single query, avoiding N+1 competition fetches.
    pub async fn get_user_entry_views(
        &self,
        pubkey: String,
    ) -> Result<Vec<super::UserEntryView>, sqlx::Error> {
        let query = "
            WITH latest_payouts AS (
                SELECT
                    entry_id,
                    ROW_NUMBER() OVER (
                        PARTITION BY entry_id
                        ORDER BY COALESCE(succeed_at, initiated_at) DESC
                    ) as rn,
                    COALESCE(succeed_at, initiated_at) as latest_payout_time
                FROM payouts
                WHERE failed_at IS NULL
            )
            SELECT
                entries.id as entry_id,
                entries.event_id as competition_id,
                json_extract(competitions.event_submission, '$.start_observation_date') as start_time,
                json_extract(competitions.event_submission, '$.end_observation_date') as end_time,
                entries.signed_at as signed_at,
                tickets.paid_at as paid_at,
                latest_payouts.latest_payout_time as paid_out_at
            FROM entries
            JOIN competitions ON entries.event_id = competitions.id
            LEFT JOIN tickets ON entries.ticket_id = tickets.id
            LEFT JOIN latest_payouts ON entries.id = latest_payouts.entry_id AND latest_payouts.rn = 1
            WHERE entries.pubkey = ?
            ORDER BY json_extract(competitions.event_submission, '$.start_observation_date') DESC";

        let views = sqlx::query_as::<_, super::UserEntryView>(query)
            .bind(pubkey)
            .fetch_all(self.db_connection.read())
            .await?;

        Ok(views)
    }

    pub async fn add_competition_with_tickets(
        &self,
        competition: Competition,
        tickets: Vec<Ticket>,
    ) -> Result<Competition, DatabaseWriteError> {
        self.add_competition_with_tickets_mode(competition, tickets, false)
            .await
    }

    pub async fn add_competition_with_tickets_mode(
        &self,
        competition: Competition,
        tickets: Vec<Ticket>,
        automatic: bool,
    ) -> Result<Competition, DatabaseWriteError> {
        let created_at = competition
            .created_at
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let event_submission = serde_json::to_string(&competition.event_submission)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let event_announcement = competition
            .event_announcement
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let competition_id_str = competition.id.to_string();

        // Prepare ticket data for the closure. Older releases read the plaintext column, so it is
        // written beside the ciphertext.
        let mut ticket_data = Vec::with_capacity(tickets.len());
        for t in &tickets {
            let ciphertext = match &t.preimage_ciphertext {
                Some(sealed) => Some(sealed.clone()),
                None if self.ticket_cipher.is_some() => {
                    let preimage =
                        stored_preimage(None, t.id, &t.hash, None, &t.legacy_preimage_hex)
                            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
                    self.seal_preimage(t.id, &t.hash, &preimage)
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?
                }
                None => None,
            };
            ticket_data.push((
                t.id.to_string(),
                t.competition_id.to_string(),
                t.legacy_preimage_hex.clone(),
                ciphertext,
                t.hash.clone(),
                t.payment_request.clone(),
            ));
        }

        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;

                sqlx::query(
                    "INSERT INTO competitions (
                        id,
                        created_at,
                        event_submission,
                        event_announcement
                    ) VALUES (?, ?, ?, ?)",
                )
                .bind(&competition_id_str)
                .bind(&created_at)
                .bind(&event_submission)
                .bind(event_announcement)
                .execute(&mut *tx)
                .await?;

                if automatic {
                    sqlx::query("INSERT INTO automatic_payout_competitions(event_id) VALUES (?)")
                        .bind(&competition_id_str)
                        .execute(&mut *tx)
                        .await?;
                }

                for (id, event_id, legacy_preimage_hex, ciphertext, hash, payment_request) in
                    &ticket_data
                {
                    sqlx::query(
                        "INSERT INTO tickets (
                            id,
                            event_id,
                            encrypted_preimage,
                            preimage_ciphertext,
                            hash,
                            payment_request
                        ) VALUES (?, ?, ?, ?, ?, ?)",
                    )
                    .bind(id)
                    .bind(event_id)
                    .bind(legacy_preimage_hex)
                    .bind(ciphertext)
                    .bind(hash)
                    .bind(payment_request)
                    .execute(&mut *tx)
                    .await?;
                }

                tx.commit().await?;
                Ok(())
            })
            .await?;

        Ok(competition)
    }

    /// The values `write_competitions` stores for `competition`, in its UPDATE's column order.
    /// Equal values mean saving the competition again would change nothing.
    pub(crate) fn update_columns(
        competition: &Competition,
    ) -> Result<Vec<Option<String>>, sqlx::Error> {
        let event_announcement = competition
            .event_announcement
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let outcome_transaction = competition
            .outcome_transaction
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let funding_psbt_base64 = competition.funding_psbt_base64.clone();
        let funding_transaction = competition
            .funding_transaction
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let funding_outpoint = competition
            .funding_outpoint
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let contract_parameters = competition
            .contract_parameters
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let public_nonces = competition
            .public_nonces
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let aggregated_nonces = competition
            .aggregated_nonces
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let partial_signatures = competition
            .partial_signatures
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let signed_contract = competition
            .signed_contract
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let attestation = competition
            .attestation
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let cancelled_at = competition
            .cancelled_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let contracted_at = competition
            .contracted_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let signed_at = competition
            .signed_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let escrow_funds_confirmed_at = competition
            .escrow_funds_confirmed_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let event_created_at = competition
            .event_created_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let entries_submitted_at = competition
            .entries_submitted_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let funding_broadcasted_at = competition
            .funding_broadcasted_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let funding_confirmed_at = competition
            .funding_confirmed_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let funding_settled_at = competition
            .funding_settled_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let awaiting_attestation_at = competition
            .awaiting_attestation_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let expiry_broadcasted_at = competition
            .expiry_broadcasted_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let outcome_broadcasted_at = competition
            .outcome_broadcasted_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let delta_broadcasted_at = competition
            .delta_broadcasted_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let completed_at = competition
            .completed_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let failed_at = competition
            .failed_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let keymeld_keygen_completed_at = competition
            .keymeld_keygen_completed_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let invoices_settled_at = competition
            .invoices_settled_at
            .map(|ts| ts.format(&Rfc3339))
            .transpose()
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let errors = if !competition.errors.is_empty() {
            Some(
                serde_json::to_string(&competition.errors)
                    .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
            )
        } else {
            None
        };
        Ok(vec![
            event_announcement,
            outcome_transaction,
            funding_psbt_base64,
            funding_transaction,
            funding_outpoint,
            contract_parameters,
            public_nonces,
            aggregated_nonces,
            partial_signatures,
            signed_contract,
            attestation,
            cancelled_at,
            contracted_at,
            signed_at,
            escrow_funds_confirmed_at,
            event_created_at,
            entries_submitted_at,
            funding_broadcasted_at,
            funding_confirmed_at,
            funding_settled_at,
            awaiting_attestation_at,
            expiry_broadcasted_at,
            outcome_broadcasted_at,
            delta_broadcasted_at,
            completed_at,
            failed_at,
            keymeld_keygen_completed_at,
            invoices_settled_at,
            errors,
        ])
    }

    pub async fn update_competitions(
        &self,
        competitions: Vec<Competition>,
    ) -> Result<(), DatabaseWriteError> {
        self.write_competitions(competitions, None)
            .await
            .map(|_| ())
    }

    /// Save a competition only while `lease` still holds it. False if another process took it.
    pub async fn update_competition_fenced(
        &self,
        competition: Competition,
        lease: &super::Lease,
    ) -> Result<bool, DatabaseWriteError> {
        let written = self
            .write_competitions(vec![competition], Some(lease.clone()))
            .await?;
        Ok(written == 1)
    }

    /// Returns how many competitions were written. With `fence`, a competition is written
    /// only while that lease is current.
    async fn write_competitions(
        &self,
        competitions: Vec<Competition>,
        fence: Option<super::Lease>,
    ) -> Result<u64, DatabaseWriteError> {
        // Prepare all competition data before moving into closure
        let prepared_updates = competitions
            .iter()
            .map(|competition| {
                Ok((
                    Self::update_columns(competition)?,
                    competition.id.to_string(),
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        self.db_connection
            .execute_write(move |pool| async move {
                let query = "UPDATE competitions SET
                    event_announcement = ?,
                    outcome_transaction = ?,
                    funding_psbt_base64 = ?,
                    funding_transaction = ?,
                    funding_outpoint = ?,
                    contract_parameters = ?,
                    public_nonces = ?,
                    aggregated_nonces = ?,
                    partial_signatures = ?,
                    signed_contract = ?,
                    attestation = ?,
                    cancelled_at = ?,
                    contracted_at = ?,
                    signed_at = ?,
                    escrow_funds_confirmed_at = ?,
                    event_created_at = ?,
                    entries_submitted_at = ?,
                    funding_broadcasted_at = ?,
                    funding_confirmed_at = ?,
                    funding_settled_at = ?,
                    awaiting_attestation_at = ?,
                    expiry_broadcasted_at = ?,
                    outcome_broadcasted_at = ?,
                    delta_broadcasted_at = ?,
                    completed_at = ?,
                    failed_at = ?,
                    keymeld_keygen_completed_at = ?,
                    invoices_settled_at = ?,
                    errors = ?
                    WHERE id = ?";
                let fenced_query = format!(
                    "{query} AND EXISTS (SELECT 1 FROM leases
                        WHERE resource = ? AND holder = ? AND token = ?)"
                );
                let mut written = 0;

                for (columns, competition_id) in prepared_updates {
                    let mut update = sqlx::query(if fence.is_some() {
                        fenced_query.as_str()
                    } else {
                        query
                    });
                    for column in columns {
                        update = update.bind(column);
                    }
                    update = update.bind(competition_id);
                    if let Some(lease) = &fence {
                        update = update
                            .bind(&lease.resource)
                            .bind(&lease.holder)
                            .bind(lease.token);
                    }
                    written += update.execute(&pool).await?.rows_affected();
                }
                Ok(written)
            })
            .await
    }

    /// Competitions with lifecycle work left, as the runners' sweep sees them. A queued
    /// competition that formed its pools has none: its pools run instead. A cancelled one whose
    /// contract holds the pot has: its funding confirmed, or an Arkade batch funded it.
    pub async fn active_competition_ids(&self) -> Result<Vec<Uuid>, sqlx::Error> {
        let ids = sqlx::query_scalar::<_, String>(
            "SELECT id FROM competitions
             WHERE completed_at IS NULL
               AND (cancelled_at IS NULL OR funding_confirmed_at IS NOT NULL
                    OR (funding_broadcasted_at IS NOT NULL
                        AND EXISTS (SELECT 1 FROM ark_funded_competitions a
                                    WHERE a.event_id = competitions.id
                                      AND a.commitment_tx IS NOT NULL)))
               AND NOT (kind = 'queued' AND pools_formed_at IS NOT NULL)",
        )
        .fetch_all(self.db_connection.read())
        .await?;
        ids.iter()
            .map(|id| Uuid::parse_str(id).map_err(|e| sqlx::Error::Decode(Box::new(e))))
            .collect()
    }

    /// Failed and cancelled competitions retain cleanup work after their active
    /// lifecycle ends. Successful cancellation/reclaim/refund markers drain this queue.
    ///
    /// The work is a held invoice to cancel, an on-chain escrow to reclaim, or a funded Arkade
    /// escrow to refund that an operator has not written off. An Arkade ticket is paid and
    /// settled at once, since the swap service settles its invoice, so only its escrow says it
    /// still holds the player's buy-in. The escrows of a pool that a batch funded were spent into
    /// it, so they are not refunded.
    ///
    /// A queued competition that formed its pools keeps only the tickets no pool took, so their
    /// escrows are refunded the same way.
    pub async fn get_competitions_pending_cleanup(
        &self,
        include_escrows: bool,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        let ids = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT competitions.id
             FROM competitions JOIN tickets ON tickets.event_id = competitions.id
             WHERE (competitions.failed_at IS NOT NULL OR competitions.cancelled_at IS NOT NULL
                    OR (competitions.kind = 'queued' AND competitions.pools_formed_at IS NOT NULL))
               AND ((tickets.paid_at IS NOT NULL AND tickets.settled_at IS NULL
                     AND tickets.invoice_cancelled_at IS NULL)
                    OR (? AND tickets.escrow_transaction IS NOT NULL
                        AND tickets.escrow_reclaimed_at IS NULL)
                    OR (EXISTS (SELECT 1 FROM ticket_ark_escrows e
                                LEFT JOIN ticket_ark_refunds r ON r.ticket_id = e.ticket_id
                                WHERE e.ticket_id = tickets.id AND e.ticket_hash = tickets.hash
                                  AND e.funded_at IS NOT NULL
                                  AND (r.state IS NULL OR r.state != 'settled')
                                  AND NOT EXISTS (
                                      SELECT 1 FROM ticket_ark_refund_write_offs w
                                      WHERE w.ticket_id = e.ticket_id
                                        AND w.ticket_hash = e.ticket_hash))
                        AND NOT EXISTS (SELECT 1 FROM ark_funded_competitions a
                                        WHERE a.event_id = competitions.id
                                          AND a.commitment_tx IS NOT NULL)))",
        )
        .bind(include_escrows)
        .fetch_all(self.db_connection.read())
        .await?;
        ids.into_iter()
            .map(|id| Uuid::parse_str(&id).map_err(|error| sqlx::Error::Decode(Box::new(error))))
            .collect()
    }

    pub async fn get_competitions(
        &self,
        active_only: bool,
    ) -> Result<Vec<Competition>, sqlx::Error> {
        self.get_competitions_selected(active_only, None).await
    }

    pub async fn get_competitions_selected(
        &self,
        active_only: bool,
        ids: Option<&[Uuid]>,
    ) -> Result<Vec<Competition>, sqlx::Error> {
        if ids.is_some_and(|ids| ids.is_empty()) {
            return Ok(Vec::new());
        }
        let base_query = r#"
            WITH payout_stats AS (
                SELECT
                    entries.event_id,
                    COUNT(DISTINCT CASE WHEN payouts.succeed_at IS NOT NULL THEN payouts.entry_id END) as total_paid_out_entries
                FROM entries
                LEFT JOIN payouts ON entries.id = payouts.entry_id
                GROUP BY entries.event_id
            )
            SELECT
                competitions.id as id,
                created_at as created_at,
                event_submission,
                event_announcement,
                COUNT(entries.id) as total_entries,
                COUNT(CASE WHEN entries.public_nonces IS NOT NULL THEN entries.id END) as total_entry_nonces,
                COUNT(CASE WHEN entries.signed_at IS NOT NULL THEN entries.id END) as total_signed_entries,
                COUNT(tickets.paid_at) as total_paid_entries,
                COALESCE(payout_stats.total_paid_out_entries, 0) as total_paid_out_entries,
                outcome_transaction,
                competitions.funding_psbt_base64 as funding_psbt_base64,
                funding_outpoint,
                funding_transaction,
                contract_parameters,
                competitions.public_nonces as public_nonces,
                aggregated_nonces,
                competitions.partial_signatures as partial_signatures,
                signed_contract,
                attestation,
                cancelled_at as cancelled_at,
                contracted_at as contracted_at,
                competitions.signed_at as signed_at,
                escrow_funds_confirmed_at as escrow_funds_confirmed_at,
                event_created_at as event_created_at,
                entries_submitted_at as entries_submitted_at,
                funding_broadcasted_at as funding_broadcasted_at,
                funding_confirmed_at as funding_confirmed_at,
                funding_settled_at as funding_settled_at,
                awaiting_attestation_at as awaiting_attestation_at,
                invoices_settled_at as invoices_settled_at,
                expiry_broadcasted_at as expiry_broadcasted_at,
                outcome_broadcasted_at as outcome_broadcasted_at,
                delta_broadcasted_at as delta_broadcasted_at,
                completed_at as completed_at,
                failed_at as failed_at,
                keymeld_keygen_completed_at as keymeld_keygen_completed_at,
                errors,
                competitions.kind as kind,
                competitions.parent_id as parent_id,
                competitions.pool_index as pool_index,
                competitions.pools_formed_at as pools_formed_at,
                competitions.pools_finished_at as pools_finished_at
            FROM competitions
            LEFT JOIN payout_stats ON competitions.id = payout_stats.event_id
            LEFT JOIN entries ON entries.event_id = competitions.id
            LEFT JOIN tickets ON entries.ticket_id = tickets.id"#;

        let base_query = match ids {
            Some(ids) => base_query.replace(
                "FROM competitions",
                &format!(
                    "FROM (SELECT * FROM competitions WHERE id IN ({})) AS competitions",
                    vec!["?"; ids.len()].join(",")
                ),
            ),
            None => base_query.to_owned(),
        };
        let final_query = if active_only {
            format!(
                "{} WHERE expiry_broadcasted_at IS NULL AND completed_at IS NULL AND cancelled_at IS NULL
                GROUP BY
                    competitions.id,
                    created_at,
                    event_submission,
                    event_announcement,
                    outcome_transaction,
                    competitions.funding_psbt_base64,
                    funding_outpoint,
                    funding_transaction,
                    contract_parameters,
                    competitions.public_nonces,
                    aggregated_nonces,
                    competitions.partial_signatures,
                    signed_contract,
                    attestation,
                    cancelled_at,
                    contracted_at,
                    competitions.signed_at,
                    escrow_funds_confirmed_at,
                    event_created_at,
                    entries_submitted_at,
                    funding_broadcasted_at,
                    funding_confirmed_at,
                    funding_settled_at,
                    awaiting_attestation_at,
                    invoices_settled_at,
                    expiry_broadcasted_at,
                    outcome_broadcasted_at,
                    delta_broadcasted_at,
                    completed_at,
                    failed_at,
                    keymeld_keygen_completed_at,
                    errors,
                    competitions.kind,
                    competitions.parent_id,
                    competitions.pool_index,
                    competitions.pools_formed_at,
                    competitions.pools_finished_at,
                    payout_stats.total_paid_out_entries",
                base_query
            )
        } else {
            format!(
                "{}
                GROUP BY
                    competitions.id,
                    created_at,
                    event_submission,
                    event_announcement,
                    outcome_transaction,
                    competitions.funding_psbt_base64,
                    funding_outpoint,
                    funding_transaction,
                    contract_parameters,
                    competitions.public_nonces,
                    aggregated_nonces,
                    competitions.partial_signatures,
                    signed_contract,
                    attestation,
                    cancelled_at,
                    contracted_at,
                    competitions.signed_at,
                    escrow_funds_confirmed_at,
                    event_created_at,
                    entries_submitted_at,
                    funding_broadcasted_at,
                    funding_confirmed_at,
                    funding_settled_at,
                    awaiting_attestation_at,
                    invoices_settled_at,
                    expiry_broadcasted_at,
                    outcome_broadcasted_at,
                    delta_broadcasted_at,
                    completed_at,
                    failed_at,
                    keymeld_keygen_completed_at,
                    errors,
                    competitions.kind,
                    competitions.parent_id,
                    competitions.pool_index,
                    competitions.pools_formed_at,
                    competitions.pools_finished_at,
                    payout_stats.total_paid_out_entries",
                base_query
            )
        };

        let mut query = sqlx::query_as::<_, Competition>(&final_query);
        if let Some(ids) = ids {
            for id in ids {
                query = query.bind(id.to_string());
            }
        }
        let competitions = query.fetch_all(self.db_connection.read()).await?;

        Ok(competitions)
    }

    /// Every competition as the public lists show it, oldest first: its terms, entry counts and
    /// lifecycle times, but not its contract, nonces, signatures or transactions, which are most
    /// of a row and which a list never shows. Those fields are `None`, so read anything derived
    /// from them (a pot return, an outcome) from [`Self::get_competition`].
    ///
    /// Without its contract, a competition whose contract exists but that recorded nothing
    /// else would read as just created; those few are read in full, so
    /// [`Competition::get_state`] reads as it does for [`Self::get_competitions`].
    pub async fn list_competitions(&self) -> Result<Vec<Competition>, sqlx::Error> {
        self.list_summaries(false).await
    }

    /// The same lean list, retaining errors for operator investigations without loading
    /// every competition's contract, transaction, nonce and signature blobs.
    pub async fn list_operator_competitions(&self) -> Result<Vec<Competition>, sqlx::Error> {
        self.list_summaries(true).await
    }

    async fn list_summaries(&self, include_errors: bool) -> Result<Vec<Competition>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            WITH entry_counts AS (
                SELECT
                    entries.event_id,
                    COUNT(entries.id) AS total_entries,
                    COUNT(entries.public_nonces) AS total_entry_nonces,
                    COUNT(entries.signed_at) AS total_signed_entries,
                    COUNT(tickets.paid_at) AS total_paid_entries
                FROM entries
                LEFT JOIN tickets ON entries.ticket_id = tickets.id
                GROUP BY entries.event_id
            ),
            payout_stats AS (
                SELECT
                    entries.event_id,
                    COUNT(DISTINCT CASE WHEN payouts.succeed_at IS NOT NULL THEN payouts.entry_id END) AS total_paid_out_entries
                FROM entries
                JOIN payouts ON entries.id = payouts.entry_id
                GROUP BY entries.event_id
            )
            SELECT
                competitions.id AS id,
                created_at,
                event_submission,
                NULL AS event_announcement,
                COALESCE(entry_counts.total_entries, 0) AS total_entries,
                COALESCE(entry_counts.total_entry_nonces, 0) AS total_entry_nonces,
                COALESCE(entry_counts.total_signed_entries, 0) AS total_signed_entries,
                COALESCE(entry_counts.total_paid_entries, 0) AS total_paid_entries,
                COALESCE(payout_stats.total_paid_out_entries, 0) AS total_paid_out_entries,
                NULL AS outcome_transaction,
                NULL AS funding_psbt_base64,
                NULL AS funding_outpoint,
                NULL AS funding_transaction,
                NULL AS contract_parameters,
                NULL AS public_nonces,
                NULL AS aggregated_nonces,
                NULL AS partial_signatures,
                NULL AS signed_contract,
                attestation,
                cancelled_at,
                contracted_at,
                competitions.signed_at AS signed_at,
                escrow_funds_confirmed_at,
                event_created_at,
                entries_submitted_at,
                funding_broadcasted_at,
                funding_confirmed_at,
                funding_settled_at,
                awaiting_attestation_at,
                invoices_settled_at,
                expiry_broadcasted_at,
                outcome_broadcasted_at,
                delta_broadcasted_at,
                completed_at,
                failed_at,
                keymeld_keygen_completed_at,
                CASE WHEN ? THEN errors ELSE NULL END AS errors,
                competitions.kind AS kind,
                competitions.parent_id AS parent_id,
                competitions.pool_index AS pool_index,
                competitions.pools_formed_at AS pools_formed_at,
                competitions.pools_finished_at AS pools_finished_at,
                competitions.contract_parameters IS NOT NULL AS has_contract
            FROM competitions
            LEFT JOIN entry_counts ON entry_counts.event_id = competitions.id
            LEFT JOIN payout_stats ON payout_stats.event_id = competitions.id
            ORDER BY competitions.id"#,
        )
        .bind(include_errors)
        .fetch_all(self.db_connection.read())
        .await?;

        let mut competitions = Vec::with_capacity(rows.len());
        for row in &rows {
            let competition = <Competition as sqlx::FromRow<_>>::from_row(row)?;
            let has_contract = sqlx::Row::try_get::<i64, _>(row, "has_contract")? != 0;
            if has_contract && competition.get_state() == super::CompetitionState::Created {
                competitions.push(self.get_competition(competition.id).await?);
            } else {
                competitions.push(competition);
            }
        }
        Ok(competitions)
    }

    pub async fn get_competition(&self, competition_id: Uuid) -> Result<Competition, sqlx::Error> {
        let query_str = r#"
            WITH payout_stats AS (
                        SELECT
                            entries.event_id,
                            COUNT(DISTINCT CASE WHEN payouts.succeed_at IS NOT NULL THEN payouts.entry_id END) as total_paid_out_entries
                        FROM entries
                        LEFT JOIN payouts ON entries.id = payouts.entry_id
                        WHERE entries.event_id = ?
                        GROUP BY entries.event_id
                    )
            SELECT
                competitions.id as id,
                created_at as created_at,
                event_submission,
                event_announcement,
                COUNT(entries.id) as total_entries,
                COUNT(CASE WHEN entries.public_nonces IS NOT NULL THEN entries.id END) as total_entry_nonces,
                COUNT(CASE WHEN entries.signed_at IS NOT NULL THEN entries.id END) as total_signed_entries,
                COUNT(tickets.paid_at) as total_paid_entries,
                COALESCE(payout_stats.total_paid_out_entries, 0) as total_paid_out_entries,
                outcome_transaction,
                competitions.funding_psbt_base64 as funding_psbt_base64,
                funding_outpoint,
                funding_transaction,
                contract_parameters,
                competitions.public_nonces as public_nonces,
                aggregated_nonces,
                competitions.partial_signatures as partial_signatures,
                signed_contract,
                attestation,
                cancelled_at as cancelled_at,
                contracted_at as contracted_at,
                competitions.signed_at as signed_at,
                escrow_funds_confirmed_at as escrow_funds_confirmed_at,
                event_created_at as event_created_at,
                entries_submitted_at as entries_submitted_at,
                funding_broadcasted_at as funding_broadcasted_at,
                funding_confirmed_at as funding_confirmed_at,
                funding_settled_at as funding_settled_at,
                awaiting_attestation_at as awaiting_attestation_at,
                invoices_settled_at as invoices_settled_at,
                expiry_broadcasted_at as expiry_broadcasted_at,
                outcome_broadcasted_at as outcome_broadcasted_at,
                delta_broadcasted_at as delta_broadcasted_at,
                completed_at as completed_at,
                failed_at as failed_at,
                keymeld_keygen_completed_at as keymeld_keygen_completed_at,
                errors,
                competitions.kind as kind,
                competitions.parent_id as parent_id,
                competitions.pool_index as pool_index,
                competitions.pools_formed_at as pools_formed_at,
                competitions.pools_finished_at as pools_finished_at
            FROM competitions
            LEFT JOIN payout_stats ON competitions.id = payout_stats.event_id
            LEFT JOIN entries ON entries.event_id = competitions.id
            LEFT JOIN tickets ON entries.ticket_id = tickets.id
            WHERE competitions.id = ?
            GROUP BY
                competitions.id,
                created_at,
                event_submission,
                event_announcement,
                outcome_transaction,
                competitions.funding_psbt_base64,
                funding_outpoint,
                funding_transaction,
                contract_parameters,
                competitions.public_nonces,
                aggregated_nonces,
                competitions.partial_signatures,
                signed_contract,
                attestation,
                cancelled_at,
                contracted_at,
                competitions.signed_at,
                escrow_funds_confirmed_at,
                event_created_at,
                entries_submitted_at,
                funding_broadcasted_at,
                funding_confirmed_at,
                funding_settled_at,
                awaiting_attestation_at,
                invoices_settled_at,
                expiry_broadcasted_at,
                outcome_broadcasted_at,
                delta_broadcasted_at,
                completed_at,
                failed_at,
                keymeld_keygen_completed_at,
                errors,
                competitions.kind,
                competitions.parent_id,
                competitions.pool_index,
                competitions.pools_formed_at,
                competitions.pools_finished_at"#;

        let competition = sqlx::query_as::<_, Competition>(query_str)
            .bind(competition_id.to_string())
            .bind(competition_id.to_string())
            .fetch_one(self.db_connection.read())
            .await?;

        Ok(competition)
    }

    /// How many tickets `player` paid for in `competition_id`, entered or not, lapsed or not: what
    /// a single competition's entries-per-player limit counts.
    pub async fn paid_ticket_count(
        &self,
        competition_id: Uuid,
        player: &str,
    ) -> Result<u64, sqlx::Error> {
        let paid: i64 = sqlx::query_scalar(PAID_TICKETS_OF_PLAYER)
            .bind(competition_id.to_string())
            .bind(player)
            .fetch_one(self.db_connection.read())
            .await?;
        u64::try_from(paid).map_err(|error| sqlx::Error::Decode(Box::new(error)))
    }

    pub async fn get_and_reserve_ticket(
        &self,
        competition_id: Uuid,
        pubkey: &str,
    ) -> Result<ReservedTicket, DatabaseWriteError> {
        match self
            .reserve_ticket(competition_id, pubkey, None, u32::MAX)
            .await?
        {
            TicketReservation::Reserved(reserved) => Ok(*reserved),
            _ => unreachable!("an unbounded reservation has no deadline or limit"),
        }
    }

    /// A ticket for `pubkey`: the one it already holds and hasn't entered with, if any, unless
    /// that ticket lapsed (see [`super::LapsedTicket`]); otherwise a free one, unless it has paid
    /// for `max_per_player` tickets already, lapsed ones included.
    pub(super) async fn get_and_reserve_ticket_before(
        &self,
        competition_id: Uuid,
        pubkey: &str,
        deadline: OffsetDateTime,
        max_per_player: u32,
    ) -> Result<TicketReservation, DatabaseWriteError> {
        self.reserve_ticket(competition_id, pubkey, Some(deadline), max_per_player)
            .await
    }

    async fn reserve_ticket(
        &self,
        competition_id: Uuid,
        pubkey: &str,
        deadline: Option<OffsetDateTime>,
        max_per_player: u32,
    ) -> Result<TicketReservation, DatabaseWriteError> {
        let competition_id_str = competition_id.to_string();
        let pubkey_owned = pubkey.to_string();
        // Used only if a stale reservation is taken over: the ticket then gets
        // a fresh preimage and hash, so the previous holder's invoice can never
        // pay for the new holder's slot.
        let rotated_preimage = hashlock::preimage_random(&mut rand::rng());
        let rotated_preimage_hex = hex::encode(rotated_preimage);
        let rotated_hash_hex = hex::encode(hashlock::sha256(&rotated_preimage));
        let cipher = self.ticket_cipher.clone();

        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                if !before_deadline(deadline) {
                    tx.rollback().await?;
                    return Ok(TicketReservation::Closed);
                }
                // A paid ticket that lapsed, its entry no longer possible, is not handed back: it
                // is refunded. It keeps its seat, as the competition's payout terms and Keymeld
                // session name every seat's ticket, so it still counts as the player's entry.
                let lapsed = lapsed_ticket_ids(
                    &mut *tx,
                    &competition_id_str,
                    Some(pubkey_owned.as_str()),
                    OffsetDateTime::now_utc(),
                )
                .await?;

                // First, check if this user already has a reserved ticket for this competition
                // (that hasn't been used for an entry yet)
                let existing_ticket: Option<Ticket> = sqlx::query_as::<_, Ticket>(
                    r#"SELECT tickets.id as id,
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
                              escrow_transaction
                       FROM tickets
                       LEFT JOIN entries ON tickets.id = entries.ticket_id
                       WHERE tickets.event_id = ?
                         AND tickets.reserved_by = ?
                         AND (paid_at IS NOT NULL OR payment_request IS NULL
                              OR invoice_expires_at > datetime('now'))
                         AND entries.id IS NULL"#,
                )
                .bind(&competition_id_str)
                .bind(&pubkey_owned)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .find(|ticket| !lapsed.contains(&ticket.id));

                if let Some(ticket) = existing_ticket {
                    debug!("Found existing reserved ticket {} for user", ticket.id);
                    if !before_deadline(deadline) {
                        tx.rollback().await?;
                        return Ok(TicketReservation::Closed);
                    }
                    tx.commit().await?;
                    return Ok(TicketReservation::Reserved(Box::new(ReservedTicket {
                        ticket,
                        superseded_payment_hash: None,
                    })));
                }

                // Every ticket it paid for has an entry or lapsed: a new one only while under the
                // limit.
                let paid: i64 = sqlx::query_scalar(PAID_TICKETS_OF_PLAYER)
                    .bind(&competition_id_str)
                    .bind(&pubkey_owned)
                    .fetch_one(&mut *tx)
                    .await?;
                if paid >= i64::from(max_per_player) {
                    tx.rollback().await?;
                    return Ok(if lapsed.is_empty() {
                        TicketReservation::EntryLimit
                    } else {
                        TicketReservation::Lapsed
                    });
                }

                // No existing ticket, find an available one
                let ticket_id: Option<String> = sqlx::query_scalar(
                    r#"SELECT tickets.id
                       FROM tickets
                       LEFT JOIN entries ON tickets.id = entries.ticket_id
                       WHERE tickets.event_id = ?
                         AND entries.id IS NULL
                         AND (
                             reserved_at IS NULL
                             OR (
                                 reserved_at < datetime('now', '-10 minutes')
                                 AND paid_at IS NULL
                             )
                             OR (reserved_by = ? AND paid_at IS NULL AND payment_request IS NOT NULL
                                 AND (invoice_expires_at IS NULL OR invoice_expires_at <= datetime('now')))
                         )
                       ORDER BY
                           reserved_at IS NULL DESC,
                           reserved_at,
                           tickets.id
                       LIMIT 1"#,
                )
                .bind(&competition_id_str)
                .bind(&pubkey_owned)
                .fetch_optional(&mut *tx)
                .await?;

                let ticket_id = match ticket_id {
                    Some(id) => {
                        debug!("Found available ticket: {}", id);
                        id
                    }
                    None => {
                        debug!("No available tickets found");
                        tx.rollback().await?;
                        return Err(sqlx::Error::RowNotFound);
                    }
                };

                // A stale reservation being taken over may still have a live
                // invoice that the previous holder can pay.
                let superseded: Option<(Option<String>, String)> = sqlx::query_as(
                    "SELECT payment_request, hash FROM tickets WHERE id = ? AND reserved_by IS NOT NULL",
                )
                .bind(&ticket_id)
                .fetch_optional(&mut *tx)
                .await?;
                let superseded_payment_hash =
                    superseded.and_then(|(payment_request, hash)| payment_request.map(|_| hash));

                let rotated_ciphertext = match &cipher {
                    Some(cipher) => Some(
                        Uuid::parse_str(&ticket_id)
                            .map_err(|e| sqlx::Error::Decode(Box::new(e)))
                            .and_then(|id| {
                                cipher
                                    .seal(id, &rotated_hash_hex, &rotated_preimage)
                                    .map_err(|e| sqlx::Error::Encode(Box::new(e)))
                            })?,
                    ),
                    None => None,
                };

                // Reserve the ticket. A takeover (the row still names a
                // previous holder) also rotates its preimage and hash and
                // drops the invoice, escrow and pubkey that belonged to them.
                let rows_affected = sqlx::query(
                    r#"UPDATE tickets
                       SET reserved_at = datetime('now'),
                           reserved_by = ?,
                           encrypted_preimage = CASE WHEN reserved_by IS NULL THEN encrypted_preimage ELSE ? END,
                           preimage_ciphertext = CASE WHEN reserved_by IS NULL THEN preimage_ciphertext ELSE ? END,
                           hash = CASE WHEN reserved_by IS NULL THEN hash ELSE ? END,
                           payment_request = CASE WHEN reserved_by IS NULL THEN payment_request ELSE NULL END,
                           invoice_expires_at = CASE WHEN reserved_by IS NULL THEN invoice_expires_at ELSE NULL END,
                           escrow_transaction = CASE WHEN reserved_by IS NULL THEN escrow_transaction ELSE NULL END,
                           ephemeral_pubkey = CASE WHEN reserved_by IS NULL THEN ephemeral_pubkey ELSE NULL END
                       WHERE id = ?
                         AND event_id = ?"#,
                )
                .bind(&pubkey_owned)
                .bind(&rotated_preimage_hex)
                .bind(rotated_ciphertext)
                .bind(&rotated_hash_hex)
                .bind(&ticket_id)
                .bind(&competition_id_str)
                .execute(&mut *tx)
                .await?
                .rows_affected();

                if rows_affected == 0 {
                    debug!("Failed to reserve ticket {}", ticket_id);
                    tx.rollback().await?;
                    return Err(sqlx::Error::RowNotFound);
                }
                // A registration sent for a released reservation is never used.
                sqlx::query(
                    "DELETE FROM ticket_keymeld_registrations
                     WHERE ticket_id = ? AND ticket_hash != (SELECT hash FROM tickets WHERE id = ?)",
                )
                .bind(&ticket_id)
                .bind(&ticket_id)
                .execute(&mut *tx)
                .await?;

                // Get the updated ticket
                let ticket = sqlx::query_as::<_, Ticket>(
                    r#"SELECT tickets.id as id,
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
                              escrow_transaction
                       FROM tickets
                       LEFT JOIN entries ON tickets.id = entries.ticket_id
                       WHERE tickets.id = ?"#,
                )
                .bind(&ticket_id)
                .fetch_one(&mut *tx)
                .await?;
                if !before_deadline(deadline) {
                    tx.rollback().await?;
                    return Ok(TicketReservation::Closed);
                }
                tx.commit().await?;

                debug!("Successfully reserved ticket {}", ticket_id);

                Ok(TicketReservation::Reserved(Box::new(ReservedTicket {
                    ticket,
                    superseded_payment_hash,
                })))
            })
            .await
    }

    /// Reserved tickets whose invoice may still change: not settled, and not recorded as
    /// cancelled. A cancelled invoice is final; the ticket's escrow, if any, is cleanup's job.
    pub async fn get_pending_tickets(&self) -> Result<Vec<Ticket>, sqlx::Error> {
        let tickets = sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
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
                      escrow_transaction
               FROM tickets
               LEFT JOIN entries ON tickets.id = entries.ticket_id
               WHERE reserved_at IS NOT NULL
                 AND settled_at IS NULL
                 AND invoice_cancelled_at IS NULL
                 AND payment_request IS NOT NULL
                 AND tickets.id NOT IN (SELECT ticket_id FROM ticket_ark_escrows)"#,
        )
        .fetch_all(self.db_connection.read())
        .await?;

        Ok(tickets)
    }

    pub async fn get_paid_tickets(&self) -> Result<Vec<Ticket>, sqlx::Error> {
        let tickets = sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
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
                      escrow_transaction
               FROM tickets
               LEFT JOIN entries ON tickets.id = entries.ticket_id
               WHERE paid_at IS NOT NULL
                 AND settled_at IS NOT NULL
                 AND reserved_at IS NOT NULL"#,
        )
        .fetch_all(self.db_connection.read())
        .await?;

        Ok(tickets)
    }

    pub async fn get_paid_tickets_for_competition(
        &self,
        competition_id: Uuid,
    ) -> Result<Vec<Ticket>, sqlx::Error> {
        let tickets = sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
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
                      escrow_transaction
               FROM tickets
               LEFT JOIN entries ON tickets.id = entries.ticket_id
               WHERE paid_at IS NOT NULL
                 AND settled_at IS NOT NULL
                 AND reserved_at IS NOT NULL
                 AND tickets.event_id = ?"#,
        )
        .bind(competition_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?;

        Ok(tickets)
    }

    /// Tickets whose HODL invoice was accepted but never settled, and whose
    /// invoice has not been cancelled yet: the payer's funds are still held.
    pub async fn get_held_tickets_for_competition(
        &self,
        competition_id: Uuid,
    ) -> Result<Vec<Ticket>, sqlx::Error> {
        sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
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
                      escrow_transaction
               FROM tickets
               LEFT JOIN entries ON tickets.id = entries.ticket_id
               WHERE paid_at IS NOT NULL
                 AND settled_at IS NULL
                 AND invoice_cancelled_at IS NULL
                 AND tickets.event_id = ?"#,
        )
        .bind(competition_id.to_string())
        .fetch_all(self.db_connection.read())
        .await
    }

    pub async fn mark_ticket_invoice_cancelled(
        &self,
        ticket_id: Uuid,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_id = ticket_id.to_string();
        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets SET invoice_cancelled_at = datetime('now')
                    WHERE id = ? AND invoice_cancelled_at IS NULL AND settled_at IS NULL",
                )
                .bind(ticket_id)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    pub async fn get_ticket(&self, ticket_id: Uuid) -> Result<Ticket, sqlx::Error> {
        let ticket = sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
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
                      escrow_transaction
               FROM tickets
               LEFT JOIN entries ON tickets.id = entries.ticket_id
               WHERE tickets.id = ?"#,
        )
        .bind(ticket_id.to_string())
        .fetch_one(self.db_connection.read())
        .await?;

        Ok(ticket)
    }

    pub async fn get_ticket_by_hash(&self, hash: &str) -> Result<Option<Ticket>, sqlx::Error> {
        let ticket = sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
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
                      escrow_transaction
               FROM tickets
               LEFT JOIN entries ON tickets.id = entries.ticket_id
               WHERE tickets.hash = ?
               AND tickets.paid_at IS NULL
               AND tickets.id NOT IN (SELECT ticket_id FROM ticket_ark_escrows)"#,
        )
        .bind(hash)
        .fetch_optional(self.db_connection.read())
        .await?;

        Ok(ticket)
    }

    pub async fn get_tickets(
        &self,
        competition_id: Uuid,
    ) -> Result<HashMap<Uuid, Ticket>, sqlx::Error> {
        let tickets = sqlx::query_as::<_, Ticket>(
            r#"SELECT
                t.id,
                t.event_id as competition_id,
                e.id as entry_id,
                t.ephemeral_pubkey,
                t.encrypted_preimage,
                t.preimage_ciphertext,
                t.hash,
                t.payment_request,
                t.invoice_expires_at,
                datetime('now', '+10 minutes') as expiry,
                t.reserved_by,
                t.reserved_at,
                t.paid_at,
                t.settled_at,
                t.escrow_transaction
               FROM tickets t
               LEFT JOIN entries e ON e.ticket_id = t.id
               WHERE t.event_id = ?"#,
        )
        .bind(competition_id.to_string())
        .fetch_all(self.db_connection.read())
        .await?;

        let mut ticket_map = HashMap::new();
        for ticket in tickets {
            if let Some(entry_id) = ticket.entry_id {
                ticket_map.insert(entry_id, ticket);
            }
        }

        Ok(ticket_map)
    }

    pub async fn mark_ticket_paid(
        &self,
        ticket_hash: &str,
        competition_id: Uuid,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_hash_owned = ticket_hash.to_string();
        let competition_id_str = competition_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets
                    SET paid_at = datetime('now')
                    WHERE hash = ?
                    AND event_id = ?
                    AND paid_at IS NULL
                    AND settled_at IS NULL
                    AND reserved_at IS NOT NULL",
                )
                .bind(ticket_hash_owned)
                .bind(competition_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    pub async fn mark_ticket_settled(&self, ticket_id: Uuid) -> Result<bool, DatabaseWriteError> {
        let ticket_id_str = ticket_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets SET settled_at = datetime('now') WHERE id = ?
                    AND settled_at IS NULL
                    AND paid_at IS NOT NULL
                    AND reserved_at IS NOT NULL",
                )
                .bind(ticket_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// Test-only: Mark a ticket as both paid and settled, bypassing Lightning.
    /// Used by the synthetic testing tool to simulate invoice payment.
    pub async fn test_settle_ticket(&self, ticket_id: Uuid) -> Result<bool, DatabaseWriteError> {
        let ticket_id_str = ticket_id.to_string();

        let result = self
            .db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets
                    SET paid_at = COALESCE(paid_at, datetime('now')),
                        settled_at = COALESCE(settled_at, datetime('now'))
                    WHERE id = ?
                    AND reserved_at IS NOT NULL",
                )
                .bind(ticket_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await?;

        Ok(result)
    }

    pub async fn update_ticket_escrow(
        &self,
        ticket: &Ticket,
        ephemeral_pubkey: String,
        escrow_tx: String,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_id_str = ticket.id.to_string();
        let expected_hash = ticket.hash.clone();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets
                    SET escrow_transaction = ?, ephemeral_pubkey = ?
                    WHERE id = ? AND hash = ? AND reserved_by IS NOT NULL",
                )
                .bind(escrow_tx)
                .bind(ephemeral_pubkey)
                .bind(ticket_id_str)
                .bind(expected_hash)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// The key the ticket's escrow will be locked to, recorded at reservation;
    /// the escrow transaction itself is built once the invoice is accepted.
    pub async fn update_ticket_pubkey(
        &self,
        ticket: &Ticket,
        ephemeral_pubkey: String,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_id = ticket.id.to_string();
        let expected_hash = ticket.hash.clone();
        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets SET ephemeral_pubkey = ?
                     WHERE id = ? AND hash = ? AND reserved_by IS NOT NULL",
                )
                .bind(ephemeral_pubkey)
                .bind(ticket_id)
                .bind(expected_hash)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// Tickets of a competition whose escrow transaction exists and has not
    /// been reclaimed, entries or not.
    pub async fn get_escrowed_tickets_for_competition(
        &self,
        competition_id: Uuid,
    ) -> Result<Vec<Ticket>, sqlx::Error> {
        sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
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
                      escrow_transaction
               FROM tickets
               LEFT JOIN entries ON tickets.id = entries.ticket_id
               WHERE escrow_transaction IS NOT NULL
                 AND escrow_reclaimed_at IS NULL
                 AND tickets.event_id = ?"#,
        )
        .bind(competition_id.to_string())
        .fetch_all(self.db_connection.read())
        .await
    }

    pub async fn mark_ticket_escrow_reclaimed(
        &self,
        ticket_id: Uuid,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_id = ticket_id.to_string();
        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets SET escrow_reclaimed_at = datetime('now')
                    WHERE id = ? AND escrow_reclaimed_at IS NULL",
                )
                .bind(ticket_id)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    pub async fn update_ticket_payment_request(
        &self,
        ticket: &Ticket,
        payment_request: &str,
        invoice_expires_at: time::OffsetDateTime,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_id_str = ticket.id.to_string();
        let expected_hash = ticket.hash.clone();
        let payment_request_owned = payment_request.to_string();
        // Use SQLite datetime format: YYYY-MM-DD HH:MM:SS
        let format = time::format_description::parse_borrowed::<1>(
            "[year]-[month]-[day] [hour]:[minute]:[second]",
        )
        .expect("valid format");
        let expires_at_str = invoice_expires_at.format(&format).unwrap_or_default();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets SET payment_request = ?, invoice_expires_at = ? WHERE id = ? AND hash = ? AND reserved_by IS NOT NULL",
                )
                .bind(payment_request_owned)
                .bind(expires_at_str)
                .bind(ticket_id_str)
                .bind(expected_hash)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// The network fee fixed for the ticket's `hash`, if one is.
    pub async fn fixed_ticket_network_fee(
        &self,
        ticket_id: Uuid,
        hash: &str,
    ) -> Result<Option<u64>, sqlx::Error> {
        let fee: Option<i64> = sqlx::query_scalar(
            "SELECT network_fee_sats FROM tickets
             WHERE id = ? AND hash = ? AND network_fee_hash = hash",
        )
        .bind(ticket_id.to_string())
        .bind(hash)
        .fetch_optional(self.db_connection.read())
        .await?;
        fee.map(|fee| u64::try_from(fee).map_err(|e| sqlx::Error::Decode(Box::new(e))))
            .transpose()
    }

    /// Fix `network_fee_sats` for the ticket's current hash, unless a fee is already fixed for
    /// it, and return the fee that is. `None` if the ticket's hash or invoice changed.
    pub async fn fix_ticket_network_fee(
        &self,
        ticket: &Ticket,
        network_fee_sats: u64,
    ) -> Result<Option<u64>, DatabaseWriteError> {
        let ticket_id = ticket.id.to_string();
        let hash = ticket.hash.clone();
        let fee = i64::try_from(network_fee_sats)
            .map_err(|e| DatabaseWriteError::Sqlx(sqlx::Error::Encode(Box::new(e))))?;
        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                sqlx::query(
                    "UPDATE tickets SET network_fee_sats = ?, network_fee_hash = hash
                     WHERE id = ? AND hash = ? AND network_fee_hash IS NOT hash
                       AND payment_request IS NULL AND paid_at IS NULL
                       AND reserved_by IS NOT NULL",
                )
                .bind(fee)
                .bind(&ticket_id)
                .bind(&hash)
                .execute(&mut *tx)
                .await?;
                let fixed: Option<i64> = sqlx::query_scalar(
                    "SELECT network_fee_sats FROM tickets
                     WHERE id = ? AND hash = ? AND network_fee_hash = hash",
                )
                .bind(&ticket_id)
                .bind(&hash)
                .fetch_optional(&mut *tx)
                .await?;
                tx.commit().await?;
                fixed
                    .map(|fee| u64::try_from(fee).map_err(|e| sqlx::Error::Decode(Box::new(e))))
                    .transpose()
            })
            .await
    }

    /// Keep a competition's latest kickoff check, replacing any earlier one.
    pub async fn store_kickoff_check(
        &self,
        competition_id: Uuid,
        check: &super::KickoffCheck,
    ) -> Result<(), DatabaseWriteError> {
        let competition_id = competition_id.to_string();
        let check_json = serde_json::to_string(check)
            .map_err(|e| DatabaseWriteError::Sqlx(sqlx::Error::Encode(Box::new(e))))?;
        let checked_at = check
            .checked_at
            .format(&Rfc3339)
            .map_err(|e| DatabaseWriteError::Sqlx(sqlx::Error::Encode(Box::new(e))))?;
        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO competition_kickoff_checks(competition_id, check_json, checked_at)
                     VALUES (?, ?, ?)
                     ON CONFLICT(competition_id) DO UPDATE
                     SET check_json = excluded.check_json, checked_at = excluded.checked_at",
                )
                .bind(&competition_id)
                .bind(&check_json)
                .bind(&checked_at)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    /// A competition's latest kickoff check, if it had one.
    pub async fn kickoff_check(
        &self,
        competition_id: Uuid,
    ) -> Result<Option<super::KickoffCheck>, sqlx::Error> {
        let check_json: Option<String> = sqlx::query_scalar(
            "SELECT check_json FROM competition_kickoff_checks WHERE competition_id = ?",
        )
        .bind(competition_id.to_string())
        .fetch_optional(self.db_connection.read())
        .await?;
        check_json
            .map(|json| serde_json::from_str(&json).map_err(|e| sqlx::Error::Decode(Box::new(e))))
            .transpose()
    }

    /// Every competition's latest kickoff check.
    pub async fn kickoff_checks(&self) -> Result<HashMap<Uuid, super::KickoffCheck>, sqlx::Error> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT competition_id, check_json FROM competition_kickoff_checks")
                .fetch_all(self.db_connection.read())
                .await?;
        rows.into_iter()
            .map(|(id, json)| {
                let id = Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                let check =
                    serde_json::from_str(&json).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                Ok((id, check))
            })
            .collect()
    }

    pub async fn clear_ticket_reservation(
        &self,
        ticket: &Ticket,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_id_str = ticket.id.to_string();
        let expected_hash = ticket.hash.clone();
        let expected_invoice = ticket.payment_request.clone();
        let preimage = hashlock::preimage_random(&mut rand::rng());
        let new_preimage = hex::encode(preimage);
        let new_hash = hex::encode(hashlock::sha256(&preimage));
        let new_ciphertext = self
            .seal_preimage(ticket.id, &new_hash, &preimage)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                let result = sqlx::query(
                    "UPDATE tickets
                    SET encrypted_preimage = ?,
                        preimage_ciphertext = ?,
                        hash = ?,
                        ephemeral_pubkey = NULL,
                        reserved_at = NULL,
                        reserved_by = NULL,
                        escrow_transaction = NULL,
                        payment_request = NULL,
                        invoice_expires_at = NULL
                    WHERE id = ? AND hash = ? AND payment_request IS ?
                    AND paid_at IS NULL
                    AND settled_at IS NULL",
                )
                .bind(new_preimage)
                .bind(new_ciphertext)
                .bind(new_hash)
                .bind(&ticket_id_str)
                .bind(expected_hash)
                .bind(expected_invoice)
                .execute(&mut *tx)
                .await?;
                let released = result.rows_affected() > 0;
                if released {
                    // The released reservation's Keymeld registration goes with it.
                    sqlx::query("DELETE FROM ticket_keymeld_registrations WHERE ticket_id = ?")
                        .bind(&ticket_id_str)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                Ok(released)
            })
            .await
    }

    pub async fn update_ticket_escrow_transaction(
        &self,
        ticket: &Ticket,
        escrow_transaction: &str,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_id_str = ticket.id.to_string();
        let expected_hash = ticket.hash.clone();
        let escrow_transaction_owned = escrow_transaction.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                // Once bytes can be published, a retry must not replace them.
                let result = sqlx::query("UPDATE tickets SET escrow_transaction = ? WHERE id = ? AND hash = ? AND reserved_at IS NOT NULL AND paid_at IS NOT NULL AND settled_at IS NULL AND escrow_transaction IS NULL")
                    .bind(escrow_transaction_owned)
                    .bind(ticket_id_str)
                    .bind(expected_hash)
                    .execute(&pool)
                    .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    pub async fn reset_ticket_after_failed_escrow(
        &self,
        ticket_id: uuid::Uuid,
        new_preimage_hex: &str,
        new_hash: &str,
    ) -> Result<bool, DatabaseWriteError> {
        let ticket_id_str = ticket_id.to_string();
        let new_preimage_hex_owned = new_preimage_hex.to_string();
        let new_hash_owned = new_hash.to_string();
        let new_ciphertext = match self.ticket_cipher {
            Some(_) => {
                let preimage = stored_preimage(None, ticket_id, new_hash, None, new_preimage_hex)
                    .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
                self.seal_preimage(ticket_id, new_hash, &preimage)
                    .map_err(|e| sqlx::Error::Encode(Box::new(e)))?
            }
            None => None,
        };

        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;
                let result = sqlx::query(
                    "UPDATE tickets
                    SET
                        encrypted_preimage = ?,
                        preimage_ciphertext = ?,
                        hash = ?,
                        payment_request = NULL,
                        paid_at = NULL,
                        settled_at = NULL,
                        escrow_transaction = NULL,
                        ephemeral_pubkey = NULL,
                        reserved_by = NULL,
                        reserved_at = NULL
                        WHERE id = ?",
                )
                .bind(new_preimage_hex_owned)
                .bind(new_ciphertext)
                .bind(new_hash_owned)
                .bind(&ticket_id_str)
                .execute(&mut *tx)
                .await?;
                // The released reservation's Keymeld registration goes with it.
                sqlx::query("DELETE FROM ticket_keymeld_registrations WHERE ticket_id = ?")
                    .bind(&ticket_id_str)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// Store a Keymeld session for a competition
    pub async fn store_keymeld_session(
        &self,
        competition_id: Uuid,
        session: &StoredDlcKeygenSession,
    ) -> Result<bool, DatabaseWriteError> {
        let session_json =
            serde_json::to_vec(session).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let competition_id_str = competition_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE competitions
                    SET keymeld_session = ?
                    WHERE id = ?",
                )
                .bind(session_json)
                .bind(competition_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// Retrieve a Keymeld session for a competition
    pub async fn get_keymeld_session(
        &self,
        competition_id: Uuid,
    ) -> Result<Option<StoredDlcKeygenSession>, sqlx::Error> {
        let session_bytes: Option<Option<Vec<u8>>> =
            sqlx::query_scalar("SELECT keymeld_session FROM competitions WHERE id = ?")
                .bind(competition_id.to_string())
                .fetch_optional(self.db_connection.read())
                .await?;

        match session_bytes {
            Some(Some(bytes)) => {
                let session =
                    serde_json::from_slice(&bytes).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                Ok(Some(session))
            }
            _ => Ok(None),
        }
    }

    /// Clear a Keymeld session for a competition (e.g., on failure or completion)
    pub async fn clear_keymeld_session(
        &self,
        competition_id: Uuid,
    ) -> Result<bool, DatabaseWriteError> {
        let competition_id_str = competition_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE competitions
                    SET keymeld_session = NULL
                    WHERE id = ?",
                )
                .bind(competition_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
    }

    /// Get a single entry by its ID
    pub async fn get_entry_by_id(&self, entry_id: Uuid) -> Result<Option<UserEntry>, sqlx::Error> {
        let query = "WITH latest_payouts AS (
              SELECT
                  entry_id,
                  payout_payment_request,
                  ROW_NUMBER() OVER (
                      PARTITION BY entry_id
                      ORDER BY COALESCE(succeed_at, initiated_at) DESC
                  ) as rn,
                  COALESCE(succeed_at, initiated_at) as latest_payout_time
              FROM payouts
              WHERE failed_at IS NULL
          )
          SELECT
              entries.id as id,
              ticket_id,
              entries.event_id as event_id,
              pubkey,
              entries.ephemeral_pubkey as ephemeral_pubkey,
              ephemeral_privatekey,
              encrypted_keymeld_private_key,
              keymeld_auth_pubkey,
              keymeld_registration_context,
              keymeld_escrow_policy,
              public_nonces,
              partial_signatures,
              funding_psbt_base64,
              entry_submission,
              payout_hash,
              payout_preimage,
              signed_at,
              tickets.paid_at AS paid_at,
              sellback_broadcasted_at,
              reclaimed_broadcasted_at,
              sweep_uneconomic_at,
              split_output_spent_at,
              latest_payouts.latest_payout_time as paid_out_at,
              latest_payouts.payout_payment_request as payout_ln_invoice
          FROM entries
          LEFT JOIN tickets ON entries.ticket_id = tickets.id
          LEFT JOIN latest_payouts ON entries.id = latest_payouts.entry_id AND latest_payouts.rn = 1
          WHERE entries.id = ?";

        sqlx::query_as::<_, UserEntry>(query)
            .bind(entry_id.to_string())
            .fetch_optional(self.db_connection.read())
            .await
    }

    /// Delete a competition and all related data (tickets, entries, payouts)
    /// This should only be used for competitions that have not started (no paid entries)
    pub async fn delete_competition(&self, competition_id: Uuid) -> Result<(), DatabaseWriteError> {
        let id_str = competition_id.to_string();
        self.db_connection
            .execute_write(move |pool| async move {
                let mut transaction = pool.begin().await?;
                // Delete payouts for entries in this competition
                sqlx::query(
                    "DELETE FROM payouts WHERE entry_id IN (SELECT id FROM entries WHERE event_id = ?)"
                )
                .bind(&id_str)
                .execute(&mut *transaction)
                .await?;

                // Delete entries for this competition
                sqlx::query("DELETE FROM entries WHERE event_id = ?")
                    .bind(&id_str)
                    .execute(&mut *transaction)
                    .await?;

                // Delete tickets for this competition
                sqlx::query("DELETE FROM tickets WHERE event_id = ?")
                    .bind(&id_str)
                    .execute(&mut *transaction)
                    .await?;

                // Delete the competition itself
                sqlx::query("DELETE FROM competitions WHERE id = ?")
                    .bind(&id_str)
                    .execute(&mut *transaction)
                    .await?;

                transaction.commit().await?;
                Ok(())
            })
            .await
    }
}
