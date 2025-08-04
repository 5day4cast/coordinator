use dlctix::{bitcoin::XOnlyPublicKey, musig2::PubNonce, SigMap};
use log::{debug, info};
use std::collections::HashMap;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::{db::DBConnection, domain::EntryPayout, FinalSignatures};

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
        let pubkey_raw = pubkey.serialize();

        sqlx::query("INSERT INTO coordinator_metadata (pubkey, name) VALUES (?, ?)")
            .bind(&pubkey_raw[..])
            .bind(&name)
            .execute(self.db_connection.write())
            .await?;

        Ok(())
    }

    pub async fn add_entry(
        &self,
        entry: UserEntry,
        ticket_id: Uuid,
    ) -> Result<UserEntry, sqlx::Error> {
        info!("entry: {:?}", entry);

        let entry_submission = serde_json::to_string(&entry.entry_submission)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

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
                entry_submission
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(entry.id.to_string())
        .bind(ticket_id.to_string())
        .bind(entry.event_id.to_string())
        .bind(&entry.pubkey)
        .bind(&entry.ephemeral_pubkey)
        .bind(&entry.ephemeral_privatekey_encrypted)
        .bind(&entry.payout_preimage_encrypted)
        .bind(&entry.payout_hash)
        .bind(&entry_submission)
        .execute(self.db_connection.write())
        .await?;

        Ok(entry)
    }

    pub async fn add_final_signatures(
        &self,
        entry_id: Uuid,
        final_signatures: FinalSignatures,
    ) -> Result<bool, sqlx::Error> {
        let sigs_json = serde_json::to_string(&final_signatures.partial_signatures)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let result = sqlx::query(
            "UPDATE entries
            SET partial_signatures = ?,
                funding_psbt_base64 = ?,
                signed_at = datetime('now')
            WHERE id = ?",
        )
        .bind(&sigs_json)
        .bind(&final_signatures.funding_psbt_base64)
        .bind(entry_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn add_public_nonces(
        &self,
        entry_id: Uuid,
        public_nonces: SigMap<PubNonce>,
    ) -> Result<bool, sqlx::Error> {
        let nonces_json =
            serde_json::to_string(&public_nonces).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let result = sqlx::query(
            "UPDATE entries
            SET public_nonces = ?
            WHERE id = ?",
        )
        .bind(&nonces_json)
        .bind(entry_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_entry_sellback_broadcast(
        &self,
        entry_id: Uuid,
        broadcast_time: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let broadcast_time_str = broadcast_time
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let result = sqlx::query(
            "UPDATE entries
            SET sellback_broadcasted_at = ?
            WHERE id = ?",
        )
        .bind(&broadcast_time_str)
        .bind(entry_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_entry_reclaim_broadcast(
        &self,
        entry_id: Uuid,
        broadcast_time: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let broadcast_time_str = broadcast_time
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let result = sqlx::query(
            "UPDATE entries
            SET reclaimed_broadcasted_at = ?
            WHERE id = ?",
        )
        .bind(&broadcast_time_str)
        .bind(entry_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn store_payout_info_pending(
        &self,
        entry_id: Uuid,
        payout_preimage: String,
        ephemeral_private_key: String,
        ln_invoice: String,
        payout_amount_sats: u64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE entries
            SET payout_preimage = ?,
                ephemeral_privatekey = ?,
                payout_ln_invoice = ?,
                payout_amount_sats = ?
            WHERE id = ?",
        )
        .bind(&payout_preimage)
        .bind(&ephemeral_private_key)
        .bind(&ln_invoice)
        .bind(payout_amount_sats as i64)
        .bind(entry_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(())
    }

    pub async fn get_pending_payouts(&self) -> Result<Vec<EntryPayout>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT
                id,
                payout_ln_invoice,
                payout_amount_sats,
            FROM entries
            WHERE payout_ln_invoice IS NOT NULL
            AND paid_out_at IS NULL
            AND sellback_broadcasted_at IS NULL
            AND reclaimed_broadcasted_at IS NULL",
        )
        .fetch_all(self.db_connection.read())
        .await?;

        let mut payouts = Vec::new();
        for row in rows {
            let entry_id = uuid::Uuid::parse_str(&row.get::<String, _>("id")).unwrap();
            let ln_invoice: String = row.get("payout_ln_invoice");
            let amount_sats: i64 = row.get("payout_amount_sats");

            let payment_hash = crate::ln_client::extract_payment_hash_from_invoice(&ln_invoice)
                .map_err(|e| sqlx::Error::Protocol(format!("Invalid invoice: {}", e)))?;

            payouts.push(EntryPayout {
                entry_id,
                payment_hash,
                ln_invoice,
                amount_sats: amount_sats as u64,
            });
        }

        Ok(payouts)
    }

    pub async fn mark_entry_paid_out(
        &self,
        entry_id: uuid::Uuid,
        paid_out_at: OffsetDateTime,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE entries
            SET paid_out_at = ?
            WHERE id = ?",
        )
        .bind(
            paid_out_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        )
        .bind(entry_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(())
    }

    pub async fn get_competition_entries(
        &self,
        event_id: Uuid,
        statuses: Vec<EntryStatus>,
    ) -> Result<Vec<UserEntry>, sqlx::Error> {
        let mut base_query = String::from(
            "SELECT
                entries.id as id,
                ticket_id,
                entries.event_id as event_id,
                pubkey,
                entries.ephemeral_pubkey as ephemeral_pubkey,
                ephemeral_privatekey_encrypted,
                ephemeral_privatekey,
                public_nonces,
                partial_signatures,
                funding_psbt_base64,
                entry_submission,
                payout_preimage_encrypted,
                payout_hash,
                payout_preimage,
                payout_ln_invoice,
                signed_at,
                tickets.settled_at AS paid_at,
                paid_out_at,
                sellback_broadcasted_at,
                reclaimed_broadcasted_at
            FROM entries
            LEFT JOIN tickets ON entries.ticket_id = tickets.id
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
        let base_query = "SELECT
            entries.id as id,
            ticket_id,
            entries.event_id as event_id,
            pubkey,
            entries.ephemeral_pubkey as ephemeral_pubkey,
            ephemeral_privatekey_encrypted,
            ephemeral_privatekey,
            public_nonces,
            partial_signatures,
            funding_psbt_base64,
            entry_submission,
            payout_preimage_encrypted,
            payout_hash,
            payout_preimage,
            payout_ln_invoice,
            signed_at,
            tickets.paid_at AS paid_at,
            paid_out_at,
            sellback_broadcasted_at,
            reclaimed_broadcasted_at
        FROM entries
        LEFT JOIN tickets ON entries.ticket_id = tickets.id
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

    pub async fn add_competition_with_tickets(
        &self,
        competition: Competition,
        tickets: Vec<Ticket>,
    ) -> Result<Competition, sqlx::Error> {
        let mut tx = self.db_connection.write().begin().await?;

        let created_at = competition
            .created_at
            .format(&Rfc3339)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        let event_submission = serde_json::to_string(&competition.event_submission)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

        sqlx::query(
            "INSERT INTO competitions (
                id,
                created_at,
                event_submission
            ) VALUES (?, ?, ?)",
        )
        .bind(competition.id.to_string())
        .bind(&created_at)
        .bind(&event_submission)
        .execute(&mut *tx)
        .await?;

        for ticket in &tickets {
            sqlx::query(
                "INSERT INTO tickets (
                    id,
                    event_id,
                    encrypted_preimage,
                    hash,
                    payment_request
                ) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(ticket.id.to_string())
            .bind(ticket.competition_id.to_string())
            .bind(&ticket.encrypted_preimage)
            .bind(&ticket.hash)
            .bind(&ticket.payment_request)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(competition)
    }

    pub async fn update_competitions(
        &self,
        competitions: Vec<Competition>,
    ) -> Result<(), sqlx::Error> {
        for competition in competitions {
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
                expiry_broadcasted_at = ?,
                outcome_broadcasted_at = ?,
                delta_broadcasted_at = ?,
                completed_at = ?,
                failed_at = ?,
                errors = ?
                WHERE id = ?";

            sqlx::query(query)
                .bind(
                    competition
                        .event_announcement
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .outcome_transaction
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(competition.funding_psbt_base64.as_ref())
                .bind(
                    competition
                        .funding_transaction
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .funding_outpoint
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .contract_parameters
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .public_nonces
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .aggregated_nonces
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .partial_signatures
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .signed_contract
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .attestation
                        .as_ref()
                        .map(|v| serde_json::to_string(v))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .cancelled_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .contracted_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .signed_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .escrow_funds_confirmed_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .event_created_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .entries_submitted_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .funding_broadcasted_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .funding_confirmed_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .funding_settled_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .expiry_broadcasted_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .outcome_broadcasted_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .delta_broadcasted_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .completed_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(
                    competition
                        .failed_at
                        .map(|ts| ts.format(&Rfc3339))
                        .transpose()
                        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                )
                .bind(if !competition.errors.is_empty() {
                    Some(
                        serde_json::to_string(&competition.errors)
                            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?,
                    )
                } else {
                    None
                })
                .bind(competition.id.to_string())
                .execute(self.db_connection.write())
                .await?;
        }

        Ok(())
    }

    pub async fn get_competitions(
        &self,
        active_only: bool,
        use_write_pool: bool,
    ) -> Result<Vec<Competition>, sqlx::Error> {
        let base_query = r#"
            SELECT
                competitions.id as id,
                created_at as created_at,
                event_submission,
                event_announcement,
                COUNT(entries.id) as total_entries,
                COUNT(CASE WHEN entries.public_nonces IS NOT NULL THEN entries.id END) as total_entry_nonces,
                COUNT(CASE WHEN entries.signed_at IS NOT NULL THEN entries.id END) as total_signed_entries,
                COUNT(tickets.settled_at) as total_paid_entries,
                COUNT(CASE WHEN entries.paid_out_at IS NOT NULL THEN entries.id END) as total_paid_out_entries,
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
                expiry_broadcasted_at as expiry_broadcasted_at,
                outcome_broadcasted_at as outcome_broadcasted_at,
                delta_broadcasted_at as delta_broadcasted_at,
                completed_at as completed_at,
                failed_at as failed_at,
                errors
            FROM competitions
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
                    expiry_broadcasted_at,
                    outcome_broadcasted_at,
                    delta_broadcasted_at,
                    completed_at,
                    failed_at,
                    errors",
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
                    expiry_broadcasted_at,
                    outcome_broadcasted_at,
                    delta_broadcasted_at,
                    completed_at,
                    failed_at,
                    errors",
                base_query
            )
        };

        let pool = if use_write_pool {
            self.db_connection.write()
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
            SELECT
                competitions.id as id,
                created_at as created_at,
                event_submission,
                event_announcement,
                COUNT(entries.id) as total_entries,
                COUNT(CASE WHEN entries.public_nonces IS NOT NULL THEN entries.id END) as total_entry_nonces,
                COUNT(CASE WHEN entries.signed_at IS NOT NULL THEN entries.id END) as total_signed_entries,
                COUNT(tickets.settled_at) as total_paid_entries,
                COUNT(CASE WHEN entries.paid_out_at IS NOT NULL THEN entries.id END) as total_paid_out_entries,
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
                expiry_broadcasted_at as expiry_broadcasted_at,
                outcome_broadcasted_at as outcome_broadcasted_at,
                delta_broadcasted_at as delta_broadcasted_at,
                completed_at as completed_at,
                failed_at as failed_at,
                errors
            FROM competitions
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
                expiry_broadcasted_at,
                outcome_broadcasted_at,
                delta_broadcasted_at,
                completed_at,
                failed_at,
                errors"#;

        let competition = sqlx::query_as::<_, Competition>(query_str)
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
        let mut tx = self.db_connection.write().begin().await?;

        // First, find an available ticket
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
        .bind(competition_id.to_string())
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
        .bind(pubkey)
        .bind(&ticket_id)
        .bind(competition_id.to_string())
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
        .bind(ticket_hash)
        .bind(competition_id.to_string())
        .bind(&interval)
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_ticket_settled(&self, ticket_id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE tickets SET settled_at = datetime('now') WHERE id = ?
            AND settled_at IS NULL
            AND paid_at IS NOT NULL
            AND reserved_at IS NOT NULL",
        )
        .bind(ticket_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn update_ticket_escrow(
        &self,
        ticket_id: Uuid,
        ephemeral_pubkey: String,
        escrow_tx: String,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE tickets
            SET escrow_transaction = ?, ephemeral_pubkey = ?
            WHERE id = ?",
        )
        .bind(&escrow_tx)
        .bind(&ephemeral_pubkey)
        .bind(ticket_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn update_ticket_payment_request(
        &self,
        ticket_id: Uuid,
        payment_request: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE tickets SET payment_request = ? WHERE id = ?")
            .bind(payment_request)
            .bind(ticket_id.to_string())
            .execute(self.db_connection.write())
            .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn clear_ticket_reservation(&self, ticket_id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE tickets
            SET reserved_at = NULL,
                reserved_by = NULL,
                paid_at = NULL,
                escrow_transaction = NULL
            WHERE id = ?
            AND settled_at IS NULL",
        )
        .bind(ticket_id.to_string())
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn update_ticket_escrow_transaction(
        &self,
        ticket_id: uuid::Uuid,
        escrow_transaction: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE tickets SET escrow_transaction = ? WHERE id = ?")
            .bind(escrow_transaction)
            .bind(ticket_id.to_string())
            .execute(self.db_connection.write())
            .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn reset_ticket_after_failed_escrow(
        &self,
        ticket_id: uuid::Uuid,
        new_encrypted_preimage: &str,
        new_hash: &str,
    ) -> Result<bool, sqlx::Error> {
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
        .bind(ticket_id.to_string())
        .bind(new_encrypted_preimage)
        .bind(new_hash)
        .execute(self.db_connection.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }
}
