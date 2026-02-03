use dlctix::{bitcoin::XOnlyPublicKey, musig2::PubNonce, SigMap};
use log::{debug, info};
use sqlx::{Execute, Sqlite};
use std::collections::HashMap;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::{
    api::routes::FinalSignatures,
    domain::{EntryPayout, PayoutError, PayoutStatus},
    infra::db::DBConnection,
};

use super::{Competition, EntryStatus, SearchBy, Ticket, UserEntry};

#[derive(Debug, Clone)]
pub struct CompetitionStore {
    db_connection: DBConnection,
}

impl CompetitionStore {
    pub fn new(db_connection: DBConnection) -> Self {
        Self { db_connection }
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
    ) -> Result<(), sqlx::Error> {
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
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn add_entry(
        &self,
        entry: UserEntry,
        ticket_id: Uuid,
    ) -> Result<UserEntry, sqlx::Error> {
        info!("entry: {:?}", entry);

        let entry_submission = serde_json::to_string(&entry.entry_submission)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let entry_id = entry.id.to_string();
        let ticket_id_str = ticket_id.to_string();
        let event_id = entry.event_id.to_string();
        let pubkey = entry.pubkey.clone();
        let ephemeral_pubkey = entry.ephemeral_pubkey.clone();
        let ephemeral_privatekey_encrypted = entry.ephemeral_privatekey_encrypted.clone();
        let payout_preimage_encrypted = entry.payout_preimage_encrypted.clone();
        let payout_hash = entry.payout_hash.clone();
        let encrypted_keymeld_private_key = entry.encrypted_keymeld_private_key.clone();
        let keymeld_auth_pubkey = entry.keymeld_auth_pubkey.clone();

        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO entries (
                        id,
                        ticket_id,
                        event_id,
                        pubkey,
                        ephemeral_pubkey,
                        ephemeral_privatekey_encrypted,
                        payout_preimage_encrypted,
                        payout_hash,
                        entry_submission,
                        encrypted_keymeld_private_key,
                        keymeld_auth_pubkey
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(entry_id)
                .bind(ticket_id_str)
                .bind(event_id)
                .bind(pubkey)
                .bind(ephemeral_pubkey)
                .bind(ephemeral_privatekey_encrypted)
                .bind(payout_preimage_encrypted)
                .bind(payout_hash)
                .bind(entry_submission)
                .bind(encrypted_keymeld_private_key)
                .bind(keymeld_auth_pubkey)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })?;

        Ok(entry)
    }

    pub async fn add_final_signatures(
        &self,
        entry_id: Uuid,
        final_signatures: FinalSignatures,
    ) -> Result<bool, sqlx::Error> {
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
                    WHERE id = ?",
                )
                .bind(sigs_json)
                .bind(funding_psbt)
                .bind(entry_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn add_public_nonces(
        &self,
        entry_id: Uuid,
        public_nonces: SigMap<PubNonce>,
    ) -> Result<bool, sqlx::Error> {
        let nonces_json =
            serde_json::to_string(&public_nonces).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let entry_id_str = entry_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE entries
                    SET public_nonces = ?
                    WHERE id = ?",
                )
                .bind(nonces_json)
                .bind(entry_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn mark_entry_sellback_broadcast(
        &self,
        entry_id: Uuid,
        broadcast_time: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
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
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn mark_entry_reclaim_broadcast(
        &self,
        entry_id: Uuid,
        broadcast_time: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
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
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    /// Update the keymeld_auth_pubkey for an entry.
    /// This is called after the keygen session is created and the user has derived their auth pubkey.
    pub async fn update_keymeld_auth_pubkey(
        &self,
        entry_id: Uuid,
        keymeld_auth_pubkey: String,
    ) -> Result<bool, sqlx::Error> {
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
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn store_payout_info_pending(
        &self,
        entry_id: Uuid,
        payout_preimage: String,
        ephemeral_private_key: String,
        ln_invoice: String,
        payout_amount_sats: u64,
    ) -> Result<Uuid, sqlx::Error> {
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
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn mark_payout_succeeded(
        &self,
        payout_id: Uuid,
        succeed_at: OffsetDateTime,
    ) -> Result<(), sqlx::Error> {
        let succeed_at_str = succeed_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let payout_id_str = payout_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE payouts
                    SET succeed_at = ?
                    WHERE id = ?",
                )
                .bind(succeed_at_str)
                .bind(payout_id_str)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn mark_payout_failed(
        &self,
        payout_id: Uuid,
        failed_at: OffsetDateTime,
        error: PayoutError,
    ) -> Result<(), sqlx::Error> {
        let error_blob =
            serde_json::to_string(&error).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let failed_at_str = failed_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let payout_id_str = payout_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                sqlx::query(
                    "UPDATE payouts
                    SET failed_at = ?, error = ?
                    WHERE id = ?",
                )
                .bind(failed_at_str)
                .bind(error_blob)
                .bind(payout_id_str)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
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
                error
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
                error
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
                error
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
                ephemeral_privatekey_encrypted,
                ephemeral_privatekey,
                encrypted_keymeld_private_key,
                keymeld_auth_pubkey,
                public_nonces,
                partial_signatures,
                funding_psbt_base64,
                entry_submission,
                payout_preimage_encrypted,
                payout_hash,
                payout_preimage,
                signed_at,
                tickets.settled_at AS paid_at,
                sellback_broadcasted_at,
                reclaimed_broadcasted_at,
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

    pub async fn get_user_entries(
        &self,
        pubkey: String,
        filter: SearchBy,
    ) -> Result<Vec<UserEntry>, sqlx::Error> {
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
              ephemeral_privatekey_encrypted,
              ephemeral_privatekey,
              encrypted_keymeld_private_key,
              keymeld_auth_pubkey,
              public_nonces,
              partial_signatures,
              funding_psbt_base64,
              entry_submission,
              payout_preimage_encrypted,
              payout_hash,
              payout_preimage,
              signed_at,
              tickets.paid_at AS paid_at,
              sellback_broadcasted_at,
              reclaimed_broadcasted_at,
              latest_payouts.latest_payout_time as paid_out_at,
              latest_payouts.payout_payment_request as payout_ln_invoice
          FROM entries
          LEFT JOIN tickets ON entries.ticket_id = tickets.id
          LEFT JOIN latest_payouts ON entries.id = latest_payouts.entry_id AND latest_payouts.rn = 1
          WHERE pubkey = ?";

        let (final_query, params) = if let Some(event_ids) = filter.event_ids {
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
    ) -> Result<Competition, sqlx::Error> {
        let created_at = competition
            .created_at
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let event_submission = serde_json::to_string(&competition.event_submission)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let competition_id_str = competition.id.to_string();

        // Prepare ticket data for the closure
        let ticket_data: Vec<(String, String, String, String, Option<String>)> = tickets
            .iter()
            .map(|t| {
                (
                    t.id.to_string(),
                    t.competition_id.to_string(),
                    t.encrypted_preimage.clone(),
                    t.hash.clone(),
                    t.payment_request.clone(),
                )
            })
            .collect();

        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;

                sqlx::query(
                    "INSERT INTO competitions (
                        id,
                        created_at,
                        event_submission
                    ) VALUES (?, ?, ?)",
                )
                .bind(&competition_id_str)
                .bind(&created_at)
                .bind(&event_submission)
                .execute(&mut *tx)
                .await?;

                for (id, event_id, encrypted_preimage, hash, payment_request) in &ticket_data {
                    sqlx::query(
                        "INSERT INTO tickets (
                            id,
                            event_id,
                            encrypted_preimage,
                            hash,
                            payment_request
                        ) VALUES (?, ?, ?, ?, ?)",
                    )
                    .bind(id)
                    .bind(event_id)
                    .bind(encrypted_preimage)
                    .bind(hash)
                    .bind(payment_request)
                    .execute(&mut *tx)
                    .await?;
                }

                tx.commit().await?;
                Ok(())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })?;

        Ok(competition)
    }

    pub async fn update_competitions(
        &self,
        competitions: Vec<Competition>,
    ) -> Result<(), sqlx::Error> {
        // Prepare all competition data before moving into closure
        let mut prepared_updates = Vec::with_capacity(competitions.len());

        for competition in competitions {
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
            let competition_id = competition.id.to_string();

            prepared_updates.push((
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
                competition_id,
            ));
        }

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

                for (
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
                    competition_id,
                ) in prepared_updates
                {
                    sqlx::query(query)
                        .bind(event_announcement)
                        .bind(outcome_transaction)
                        .bind(funding_psbt_base64)
                        .bind(funding_transaction)
                        .bind(funding_outpoint)
                        .bind(contract_parameters)
                        .bind(public_nonces)
                        .bind(aggregated_nonces)
                        .bind(partial_signatures)
                        .bind(signed_contract)
                        .bind(attestation)
                        .bind(cancelled_at)
                        .bind(contracted_at)
                        .bind(signed_at)
                        .bind(escrow_funds_confirmed_at)
                        .bind(event_created_at)
                        .bind(entries_submitted_at)
                        .bind(funding_broadcasted_at)
                        .bind(funding_confirmed_at)
                        .bind(funding_settled_at)
                        .bind(awaiting_attestation_at)
                        .bind(expiry_broadcasted_at)
                        .bind(outcome_broadcasted_at)
                        .bind(delta_broadcasted_at)
                        .bind(completed_at)
                        .bind(failed_at)
                        .bind(keymeld_keygen_completed_at)
                        .bind(invoices_settled_at)
                        .bind(errors)
                        .bind(competition_id)
                        .execute(&pool)
                        .await?;
                }
                Ok(())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn get_competitions(
        &self,
        active_only: bool,
        use_write_pool: bool,
    ) -> Result<Vec<Competition>, sqlx::Error> {
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
                errors
            FROM competitions
            LEFT JOIN payout_stats ON competitions.id = payout_stats.event_id
            LEFT JOIN entries ON entries.event_id = competitions.id
            LEFT JOIN tickets ON entries.ticket_id = tickets.id"#;

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
                    payout_stats.total_paid_out_entries",
                base_query
            )
        };

        let pool = if use_write_pool {
            self.db_connection.write_pool()
        } else {
            self.db_connection.read()
        };

        let competitions = sqlx::query_as::<_, Competition>(&final_query)
            .fetch_all(pool)
            .await?;

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
                errors
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
                errors"#;

        let competition = sqlx::query_as::<_, Competition>(query_str)
            .bind(competition_id.to_string())
            .bind(competition_id.to_string())
            .fetch_one(self.db_connection.read())
            .await?;

        Ok(competition)
    }

    pub async fn get_and_reserve_ticket(
        &self,
        competition_id: Uuid,
        pubkey: &str,
    ) -> Result<Ticket, sqlx::Error> {
        let competition_id_str = competition_id.to_string();
        let pubkey_owned = pubkey.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let mut tx = pool.begin().await?;

                // First, check if this user already has a reserved ticket for this competition
                // (that hasn't been used for an entry yet)
                let existing_ticket: Option<Ticket> = sqlx::query_as::<_, Ticket>(
                    r#"SELECT tickets.id as id,
                              tickets.event_id as competition_id,
                              entries.id as entry_id,
                              tickets.ephemeral_pubkey as ephemeral_pubkey,
                              encrypted_preimage,
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
                         AND entries.id IS NULL
                       LIMIT 1"#,
                )
                .bind(&competition_id_str)
                .bind(&pubkey_owned)
                .fetch_optional(&mut *tx)
                .await?;

                if let Some(ticket) = existing_ticket {
                    debug!("Found existing reserved ticket {} for user", ticket.id);
                    tx.commit().await?;
                    return Ok(ticket);
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
                         )
                       ORDER BY
                           reserved_at IS NULL DESC,
                           reserved_at,
                           tickets.id
                       LIMIT 1"#,
                )
                .bind(&competition_id_str)
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

                // Update the ticket to reserve it
                let rows_affected = sqlx::query(
                    r#"UPDATE tickets
                       SET reserved_at = datetime('now'),
                           reserved_by = ?
                       WHERE id = ?
                         AND event_id = ?"#,
                )
                .bind(&pubkey_owned)
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

                // Get the updated ticket
                let ticket = sqlx::query_as::<_, Ticket>(
                    r#"SELECT tickets.id as id,
                              tickets.event_id as competition_id,
                              entries.id as entry_id,
                              tickets.ephemeral_pubkey as ephemeral_pubkey,
                              encrypted_preimage,
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
                tx.commit().await?;

                debug!("Successfully reserved ticket {}", ticket_id);

                Ok(ticket)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn get_pending_tickets(&self) -> Result<Vec<Ticket>, sqlx::Error> {
        let tickets = sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
                      tickets.event_id as competition_id,
                      entries.id as entry_id,
                      tickets.ephemeral_pubkey as ephemeral_pubkey,
                      encrypted_preimage,
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
                 AND paid_at IS NULL
                 AND entry_id IS NULL
                 AND reserved_at > datetime('now', '-10 minutes')"#,
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

    pub async fn get_ticket(&self, ticket_id: Uuid) -> Result<Ticket, sqlx::Error> {
        let ticket = sqlx::query_as::<_, Ticket>(
            r#"SELECT tickets.id as id,
                      tickets.event_id as competition_id,
                      entries.id as entry_id,
                      tickets.ephemeral_pubkey as ephemeral_pubkey,
                      encrypted_preimage,
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
               AND tickets.paid_at IS NULL"#,
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
    ) -> Result<bool, sqlx::Error> {
        let interval = format!("-{} minutes", 10);
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
                    AND reserved_at IS NOT NULL
                    AND reserved_at > datetime('now', ?)",
                )
                .bind(ticket_hash_owned)
                .bind(competition_id_str)
                .bind(interval)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn mark_ticket_settled(&self, ticket_id: Uuid) -> Result<bool, sqlx::Error> {
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
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn update_ticket_escrow(
        &self,
        ticket_id: Uuid,
        ephemeral_pubkey: String,
        escrow_tx: String,
    ) -> Result<bool, sqlx::Error> {
        let ticket_id_str = ticket_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets
                    SET escrow_transaction = ?, ephemeral_pubkey = ?
                    WHERE id = ?",
                )
                .bind(escrow_tx)
                .bind(ephemeral_pubkey)
                .bind(ticket_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn update_ticket_payment_request(
        &self,
        ticket_id: Uuid,
        payment_request: &str,
        invoice_expires_at: time::OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let ticket_id_str = ticket_id.to_string();
        let payment_request_owned = payment_request.to_string();
        // Use SQLite datetime format: YYYY-MM-DD HH:MM:SS
        let format =
            time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
                .expect("valid format");
        let expires_at_str = invoice_expires_at.format(&format).unwrap_or_default();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets SET payment_request = ?, invoice_expires_at = ? WHERE id = ?",
                )
                .bind(payment_request_owned)
                .bind(expires_at_str)
                .bind(ticket_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn clear_ticket_reservation(&self, ticket_id: Uuid) -> Result<bool, sqlx::Error> {
        let ticket_id_str = ticket_id.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets
                    SET reserved_at = NULL,
                        reserved_by = NULL,
                        paid_at = NULL,
                        escrow_transaction = NULL,
                        payment_request = NULL,
                        invoice_expires_at = NULL
                    WHERE id = ?
                    AND settled_at IS NULL",
                )
                .bind(ticket_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn update_ticket_escrow_transaction(
        &self,
        ticket_id: uuid::Uuid,
        escrow_transaction: &str,
    ) -> Result<bool, sqlx::Error> {
        let ticket_id_str = ticket_id.to_string();
        let escrow_transaction_owned = escrow_transaction.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query("UPDATE tickets SET escrow_transaction = ? WHERE id = ?")
                    .bind(escrow_transaction_owned)
                    .bind(ticket_id_str)
                    .execute(&pool)
                    .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    pub async fn reset_ticket_after_failed_escrow(
        &self,
        ticket_id: uuid::Uuid,
        new_encrypted_preimage: &str,
        new_hash: &str,
    ) -> Result<bool, sqlx::Error> {
        let ticket_id_str = ticket_id.to_string();
        let new_encrypted_preimage_owned = new_encrypted_preimage.to_string();
        let new_hash_owned = new_hash.to_string();

        self.db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE tickets
                    SET
                        encrypted_preimage = ?,
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
                .bind(new_encrypted_preimage_owned)
                .bind(new_hash_owned)
                .bind(ticket_id_str)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected() > 0)
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    /// Store a Keymeld session for a competition
    pub async fn store_keymeld_session(
        &self,
        competition_id: Uuid,
        session: &crate::infra::keymeld::StoredDlcKeygenSession,
    ) -> Result<bool, sqlx::Error> {
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
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }

    /// Retrieve a Keymeld session for a competition
    pub async fn get_keymeld_session(
        &self,
        competition_id: Uuid,
    ) -> Result<Option<crate::infra::keymeld::StoredDlcKeygenSession>, sqlx::Error> {
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
    pub async fn clear_keymeld_session(&self, competition_id: Uuid) -> Result<bool, sqlx::Error> {
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
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
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
              ephemeral_privatekey_encrypted,
              ephemeral_privatekey,
              encrypted_keymeld_private_key,
              keymeld_auth_pubkey,
              public_nonces,
              partial_signatures,
              funding_psbt_base64,
              entry_submission,
              payout_preimage_encrypted,
              payout_hash,
              payout_preimage,
              signed_at,
              tickets.paid_at AS paid_at,
              sellback_broadcasted_at,
              reclaimed_broadcasted_at,
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
    pub async fn delete_competition(&self, competition_id: Uuid) -> Result<(), sqlx::Error> {
        let id_str = competition_id.to_string();
        self.db_connection
            .execute_write(move |pool| async move {
                // Delete payouts for entries in this competition
                sqlx::query(
                    "DELETE FROM payouts WHERE entry_id IN (SELECT id FROM entries WHERE event_id = ?)"
                )
                .bind(&id_str)
                .execute(&pool)
                .await?;

                // Delete entries for this competition
                sqlx::query("DELETE FROM entries WHERE event_id = ?")
                    .bind(&id_str)
                    .execute(&pool)
                    .await?;

                // Delete tickets for this competition
                sqlx::query("DELETE FROM tickets WHERE event_id = ?")
                    .bind(&id_str)
                    .execute(&pool)
                    .await?;

                // Delete the competition itself
                sqlx::query("DELETE FROM competitions WHERE id = ?")
                    .bind(&id_str)
                    .execute(&pool)
                    .await?;

                Ok(())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => e,
                e => sqlx::Error::Protocol(e.to_string()),
            })
    }
}
