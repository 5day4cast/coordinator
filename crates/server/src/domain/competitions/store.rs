use dlctix::{bitcoin::XOnlyPublicKey, musig2::PubNonce, SigMap};
use duckdb::{params, params_from_iter, types::Value, Connection};
use log::{debug, info, trace};
use scooby::postgres::{select, Joinable};
use std::collections::HashMap;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::{domain::DBConnection, FinalSignatures};

use super::{run_competition_migrations, Competition, EntryStatus, SearchBy, Ticket, UserEntry};

#[derive(Clone)]
pub struct CompetitionStore {
    db_connection: DBConnection,
}

impl CompetitionStore {
    pub fn new(db_connection: DBConnection) -> Result<Self, duckdb::Error> {
        let mut conn = Connection::open(db_connection.connection_path.clone())?;
        run_competition_migrations(&mut conn)?;
        Ok(Self { db_connection })
    }

    pub async fn ping(&self) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare("SELECT 1")?;
        let _ = stmt.query([])?;
        Ok(())
    }

    pub async fn get_stored_public_key(&self) -> Result<XOnlyPublicKey, duckdb::Error> {
        let select = select("pubkey").from("coordinator_metadata");
        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&select.to_string())?;
        let key: Vec<u8> = stmt.query_row([], |row| row.get(0))?;
        //TODO: use a custom error here so we don't need to panic
        let converted_key = XOnlyPublicKey::from_slice(&key).expect("invalid pubkey");
        Ok(converted_key)
    }

    pub async fn add_coordinator_metadata(
        &self,
        pubkey: XOnlyPublicKey,
    ) -> Result<(), duckdb::Error> {
        let pubkey_raw = pubkey.serialize().to_vec();
        //TODO: Add the ability to change the name via config
        let name = String::from("5day4cast");
        let conn = self.db_connection.new_write_connection_retry().await?;
        let mut stmt =
            conn.prepare("INSERT INTO coordinator_metadata (pubkey,name) VALUES(?,?)")?;
        stmt.execute([pubkey_raw, name.into()])?;
        Ok(())
    }

    pub async fn add_entry(
        &self,
        entry: UserEntry,
        ticket_id: Uuid,
    ) -> Result<UserEntry, duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;
        info!("entry: {:?}", entry);

        let entry_submission = serde_json::to_vec(&entry.entry_submission)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;

        // Insert the entry with ticket_id
        let mut stmt = conn.prepare(
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
            ) VALUES(?,?,?,?,?,?,?,?,?)",
        )?;

        stmt.execute(params![
            entry.id.to_string(),
            ticket_id.to_string(),
            entry.event_id.to_string(),
            entry.pubkey,
            entry.ephemeral_pubkey,
            entry.ephemeral_privatekey_encrypted,
            entry.payout_preimage_encrypted,
            entry.payout_hash,
            Value::Blob(entry_submission)
        ])?;

        Ok(entry)
    }

    pub async fn add_final_signatures(
        &self,
        entry_id: Uuid,
        final_signatures: FinalSignatures,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;
        let sigs_blob = serde_json::to_vec(&final_signatures.partial_signatures)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;

        let mut stmt = conn.prepare(
            "UPDATE entries
                SET partial_signatures = ?,
                    funding_psbt_base64 = ?,
                    signed_at = NOW()
                WHERE id = ?",
        )?;

        stmt.execute(params![
            Value::Blob(sigs_blob),
            final_signatures.funding_psbt_base64,
            entry_id.to_string(),
        ])?;

        Ok(())
    }

    pub async fn add_public_nonces(
        &self,
        entry_id: Uuid,
        public_nonces: SigMap<PubNonce>,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;
        let nonces_blob = serde_json::to_vec(&public_nonces)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;

        let mut stmt = conn.prepare(
            "UPDATE entries
            SET public_nonces = ?
            WHERE id = ?",
        )?;

        stmt.execute(params![Value::Blob(nonces_blob), entry_id.to_string(),])?;

        Ok(())
    }

    pub async fn mark_entry_sellback_broadcast(
        &self,
        entry_id: Uuid,
        broadcast_time: OffsetDateTime,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        let mut stmt = conn.prepare(
            "UPDATE entries
                SET sellback_broadcasted_at = ?
                WHERE id = ?",
        )?;

        stmt.execute(params![
            broadcast_time.format(&Rfc3339).unwrap(),
            entry_id.to_string(),
        ])?;

        Ok(())
    }

    pub async fn mark_entry_reclaim_broadcast(
        &self,
        entry_id: Uuid,
        broadcast_time: OffsetDateTime,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        let mut stmt = conn.prepare(
            "UPDATE entries
                SET reclaimed_broadcasted_at = ?
                WHERE id = ?",
        )?;

        stmt.execute(params![
            broadcast_time.format(&Rfc3339).unwrap(),
            entry_id.to_string(),
        ])?;

        Ok(())
    }

    pub async fn store_payout_info(
        &self,
        entry_id: Uuid,
        payout_preimage: String,
        ephemeral_private_key: String,
        ln_invoice: String,
        paid_time: OffsetDateTime,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        let mut stmt = conn.prepare(
            "UPDATE entries
            SET payout_preimage = ?,
                ephemeral_privatekey = ?,
                payout_ln_invoice = ?,
                paid_out_at = ?
            WHERE id = ?",
        )?;

        stmt.execute(params![
            payout_preimage,
            ephemeral_private_key,
            ln_invoice,
            paid_time.format(&Rfc3339).unwrap(),
            entry_id.to_string(),
        ])?;

        Ok(())
    }

    pub async fn get_competition_entries(
        &self,
        event_id: Uuid,
        statuses: Vec<EntryStatus>,
    ) -> Result<Vec<UserEntry>, duckdb::Error> {
        let mut select = select((
            "entries.id as id",
            "ticket_id",
            "entries.event_id as event_id",
            "pubkey",
            "entries.ephemeral_pubkey as ephemeral_pubkey",
            "ephemeral_privatekey_encrypted",
            "ephemeral_privatekey",
            "public_nonces",
        ))
        .and_select((
            "partial_signatures",
            "funding_psbt_base64",
            "entry_submission",
            "payout_preimage_encrypted",
            "payout_hash",
            "payout_preimage",
            "payout_ln_invoice",
        ))
        .and_select((
            "signed_at::TEXT",
            "tickets.settled_at::TEXT AS paid_at",
            "paid_out_at::TEXT",
            "sellback_broadcasted_at::TEXT",
            "reclaimed_broadcasted_at::TEXT",
        ))
        .from(
            "entries"
                .left_join("tickets")
                .on("entries.ticket_id = tickets.id"),
        );
        if !statuses.is_empty() {
            for status in statuses {
                match status {
                    EntryStatus::Paid => {
                        select = select.where_("tickets.paid_at IS NOT NULL");
                    }
                    EntryStatus::Signed => {
                        select = select.where_("signed_at IS NOT NULL");
                    }
                }
            }
        }
        let query_str = select.where_("entries.event_id = ?").to_string();
        trace!("get competition entries query: {}", query_str);
        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let mut rows = stmt.query(params![event_id.to_string()])?;

        let mut user_entries: Vec<UserEntry> = vec![];
        while let Some(row) = rows.next()? {
            let data: UserEntry = row.try_into()?;
            user_entries.push(data.clone());
        }

        Ok(user_entries)
    }

    pub async fn get_user_entries(
        &self,
        pubkey: String,
        filter: SearchBy,
    ) -> Result<Vec<UserEntry>, duckdb::Error> {
        let mut select = select((
            "entries.id as id",
            "ticket_id",
            "entries.event_id as event_id",
            "pubkey",
            "entries.ephemeral_pubkey as ephemeral_pubkey",
            "ephemeral_privatekey_encrypted",
            "ephemeral_privatekey",
            "public_nonces",
        ))
        .and_select((
            "partial_signatures",
            "funding_psbt_base64",
            "entry_submission",
            "payout_preimage_encrypted",
            "payout_hash",
            "payout_preimage",
            "payout_ln_invoice",
        ))
        .and_select((
            "signed_at::TEXT",
            "tickets.paid_at::TEXT AS paid_at",
            "paid_out_at::TEXT",
            "sellback_broadcasted_at::TEXT",
            "reclaimed_broadcasted_at::TEXT",
        ))
        .from(
            "entries"
                .left_join("tickets")
                .on("entries.ticket_id = tickets.id"),
        );
        if let Some(ids) = filter.event_ids.clone() {
            let mut event_ids_val = String::new();
            event_ids_val.push('(');
            for (index, _) in ids.iter().enumerate() {
                event_ids_val.push('?');
                if index < ids.len() - 1 {
                    event_ids_val.push(',');
                }
            }
            event_ids_val.push(')');
            let where_clause = format!("entries.event_id IN {}", event_ids_val);
            select = select.clone().where_(where_clause);
        }
        let query_str = select.where_("pubkey = ?").to_string();
        trace!("get user entries query: {}", query_str);
        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let mut rows = if let Some(ids) = filter.event_ids {
            let mut params: Vec<Value> = ids
                .iter()
                .map(|event_id| Value::Text(event_id.to_string()))
                .collect();
            params.push(Value::Text(pubkey));
            stmt.query(params_from_iter(params.iter()))
        } else {
            stmt.query(params![pubkey])
        }?;

        let mut user_entries: Vec<UserEntry> = vec![];
        while let Some(row) = rows.next()? {
            let data: UserEntry = row.try_into()?;
            user_entries.push(data.clone());
        }

        Ok(user_entries)
    }

    pub async fn add_competition_with_tickets(
        &self,
        competition: Competition,
        tickets: Vec<Ticket>,
    ) -> Result<Competition, duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        // Start a transaction
        conn.execute("BEGIN TRANSACTION", [])?;

        let created_at = OffsetDateTime::format(competition.created_at, &Rfc3339)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;

        let mut stmt = conn.prepare(
            "INSERT INTO competitions (
                      id,
                      created_at,
                      event_submission) VALUES(?,?,?)",
        )?;

        let event_submission = Value::Blob(
            serde_json::to_vec(&competition.event_submission)
                .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?,
        );

        stmt.execute(params![
            competition.id.to_string(),
            created_at,
            event_submission,
        ])?;

        let mut stmt = conn.prepare(
            "INSERT INTO tickets (
                id,
                event_id,
                encrypted_preimage,
                hash,
                payment_request
            ) VALUES (?, ?, ?, ?, ?)",
        )?;

        for ticket in tickets {
            stmt.execute(params![
                ticket.id.to_string(),
                ticket.competition_id.to_string(),
                ticket.encrypted_preimage,
                ticket.hash,
                ticket.payment_request,
            ])?;
        }

        conn.execute("COMMIT", [])?;

        Ok(competition)
    }

    pub async fn update_competitions(
        &self,
        competitions: Vec<Competition>,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        for competition in competitions {
            let mut params: Vec<Value> = vec![];

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

            if let Some(event_announcement) = &competition.event_announcement {
                params.push(Value::Blob(
                    serde_json::to_vec(event_announcement)
                        .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?,
                ));
            } else {
                params.push(Value::Null);
            }

            if let Some(outcome_transaction) = &competition.outcome_transaction {
                params.push(Value::Blob(
                    serde_json::to_vec(outcome_transaction)
                        .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?,
                ));
            } else {
                params.push(Value::Null);
            }

            if let Some(funding_psbt) = &competition.funding_psbt_base64 {
                params.push(Value::Text(funding_psbt.to_owned()));
            } else {
                params.push(Value::Null);
            }

            if let Some(funding_transaction) = &competition.funding_transaction {
                params.push(Value::Blob(
                    serde_json::to_vec(funding_transaction)
                        .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?,
                ));
            } else {
                params.push(Value::Null);
            }

            if let Some(funding_outpoint) = &competition.funding_outpoint {
                params
                    .push(Value::Blob(serde_json::to_vec(funding_outpoint).map_err(
                        |e| duckdb::Error::ToSqlConversionFailure(Box::new(e)),
                    )?));
            } else {
                params.push(Value::Null);
            }

            if let Some(contract_parameters) = &competition.contract_parameters {
                params.push(Value::Blob(
                    serde_json::to_vec(contract_parameters)
                        .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?,
                ));
            } else {
                params.push(Value::Null);
            }

            if let Some(public_nonces) = &competition.public_nonces {
                params
                    .push(Value::Blob(serde_json::to_vec(public_nonces).map_err(
                        |e| duckdb::Error::ToSqlConversionFailure(Box::new(e)),
                    )?));
            } else {
                params.push(Value::Null);
            }

            if let Some(aggregated_nonces) = &competition.aggregated_nonces {
                params
                    .push(Value::Blob(serde_json::to_vec(aggregated_nonces).map_err(
                        |e| duckdb::Error::ToSqlConversionFailure(Box::new(e)),
                    )?));
            } else {
                params.push(Value::Null);
            }

            if let Some(partial_signatures) = &competition.partial_signatures {
                params.push(Value::Blob(
                    serde_json::to_vec(partial_signatures)
                        .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?,
                ));
            } else {
                params.push(Value::Null);
            }

            if let Some(signed_contract) = &competition.signed_contract {
                params
                    .push(Value::Blob(serde_json::to_vec(signed_contract).map_err(
                        |e| duckdb::Error::ToSqlConversionFailure(Box::new(e)),
                    )?));
            } else {
                params.push(Value::Null);
            }

            if let Some(attestation) = &competition.attestation {
                params
                    .push(Value::Blob(serde_json::to_vec(attestation).map_err(
                        |e| duckdb::Error::ToSqlConversionFailure(Box::new(e)),
                    )?));
            } else {
                params.push(Value::Null);
            }

            for timestamp in [
                &competition.cancelled_at,
                &competition.contracted_at,
                &competition.signed_at,
                &competition.escrow_funds_confirmed_at,
                &competition.event_created_at,
                &competition.entries_submitted_at,
                &competition.funding_broadcasted_at,
                &competition.funding_confirmed_at,
                &competition.funding_settled_at,
                &competition.expiry_broadcasted_at,
                &competition.outcome_broadcasted_at,
                &competition.delta_broadcasted_at,
                &competition.completed_at,
                &competition.failed_at,
            ] {
                if let Some(ts) = timestamp {
                    params
                        .push(Value::Text(OffsetDateTime::format(*ts, &Rfc3339).map_err(
                            |e| duckdb::Error::ToSqlConversionFailure(Box::new(e)),
                        )?));
                } else {
                    params.push(Value::Null);
                }
            }

            if !competition.errors.is_empty() {
                params.push(Value::Blob(
                    serde_json::to_vec(&competition.errors)
                        .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?,
                ));
            } else {
                params.push(Value::Null);
            }

            params.push(Value::Text(competition.id.to_string()));

            let mut stmt = conn.prepare(query)?;
            stmt.execute(params_from_iter(params))?;
        }

        Ok(())
    }

    pub async fn get_competitions(
        &self,
        active_only: bool,
    ) -> Result<Vec<Competition>, duckdb::Error> {
        let mut query = select((
            "competitions.id as id",
            "created_at::TEXT as created_at",
            "event_submission",
            "event_announcement",
        ))
        .and_select((
            "COUNT(entries.id) as total_entries",
            "COUNT(entries.id) FILTER (entries.public_nonces IS NOT NULL) as total_entry_nonces",
            "COUNT(entries.id) FILTER (entries.signed_at IS NOT NULL) as total_signed_entries",
            "COUNT(tickets.settled_at) as total_paid_entries",
            "COUNT(entries.id) FILTER (entries.paid_out_at IS NOT NULL) as total_paid_out_entries",
        ))
        .and_select((
            "outcome_transaction",
            "competitions.funding_psbt_base64 as funding_psbt_base64",
            "funding_outpoint",
            "funding_transaction",
            "contract_parameters",
            "competitions.public_nonces as public_nonces",
        ))
        .and_select((
            "aggregated_nonces",
            "competitions.partial_signatures as partial_signatures",
            "signed_contract",
            "attestation",
        ))
        .and_select((
            "cancelled_at::TEXT as cancelled_at",
            "contracted_at::TEXT as contracted_at",
            "competitions.signed_at::TEXT as signed_at",
            "escrow_funds_confirmed_at::TEXT as escrow_funds_confirmed_at",
            "event_created_at::TEXT as event_created_at",
            "entries_submitted_at::TEXT as entries_submitted_at",
            "funding_broadcasted_at::TEXT as funding_broadcasted_at",
            "funding_confirmed_at::TEXT as funding_confirmed_at",
            "funding_settled_at::TEXT as funding_settled_at",
        ))
        .and_select((
            "expiry_broadcasted_at::TEXT as expiry_broadcasted_at",
            "outcome_broadcasted_at::TEXT as outcome_broadcasted_at",
            "delta_broadcasted_at::TEXT as delta_broadcasted_at",
            "completed_at::TEXT as completed_at",
            "failed_at::TEXT as failed_at",
            "errors",
        ))
        .from(
            "competitions"
                .left_join("entries")
                .on("entries.event_id = competitions.id")
                .left_join("tickets")
                .on("entries.ticket_id = tickets.id"),
        );

        if active_only {
            // filters out competitions in terminal states
            query = query.where_(
                "expiry_broadcasted_at IS NULL AND completed_at IS NULL AND cancelled_at IS NULL",
            );
        }

        let query_str = query
            .group_by((
                "competitions.id",
                "created_at",
                "event_submission",
                "event_announcement",
                "outcome_transaction",
                "competitions.funding_psbt_base64",
            ))
            .group_by((
                "funding_outpoint",
                "funding_transaction",
                "contract_parameters",
                "competitions.public_nonces",
                "aggregated_nonces",
                "competitions.partial_signatures",
                "signed_contract",
                "attestation",
            ))
            .group_by((
                "cancelled_at",
                "contracted_at",
                "competitions.signed_at",
                "escrow_funds_confirmed_at",
                "event_created_at",
                "entries_submitted_at",
                "funding_broadcasted_at",
                "funding_confirmed_at",
            ))
            .group_by((
                "funding_settled_at",
                "expiry_broadcasted_at",
                "outcome_broadcasted_at",
                "delta_broadcasted_at",
                "completed_at",
                "failed_at",
                "errors",
            ))
            .to_string();
        trace!("competitions query: {}", query_str);
        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let mut rows = stmt.query([])?;
        let mut competitions: Vec<Competition> = vec![];
        while let Some(row) = rows.next()? {
            let data: Competition = row.try_into()?;
            competitions.push(data.clone());
        }

        Ok(competitions)
    }

    pub async fn get_competition(
        &self,
        competition_id: Uuid,
    ) -> Result<Competition, duckdb::Error> {
        let query_str = select((
            "competitions.id as id",
            "created_at::TEXT as created_at",
            "event_submission",
            "event_announcement",
        ))
        .and_select((
            "COUNT(entries.id) as total_entries",
            "COUNT(entries.id) FILTER (entries.public_nonces IS NOT NULL) as total_entry_nonces",
            "COUNT(entries.id) FILTER (entries.signed_at IS NOT NULL) as total_signed_entries",
            "COUNT(tickets.settled_at) as total_paid_entries",
            "COUNT(entries.id) FILTER (entries.paid_out_at IS NOT NULL) as total_paid_out_entries",
        ))
        .and_select((
            "outcome_transaction",
            "competitions.funding_psbt_base64 as funding_psbt_base64",
            "funding_outpoint",
            "funding_transaction",
            "contract_parameters",
            "competitions.public_nonces as public_nonces",
        ))
        .and_select((
            "aggregated_nonces",
            "competitions.partial_signatures as partial_signatures",
            "signed_contract",
            "attestation",
        ))
        .and_select((
            "cancelled_at::TEXT as cancelled_at",
            "contracted_at::TEXT as contracted_at",
            "competitions.signed_at::TEXT as signed_at",
            "escrow_funds_confirmed_at::TEXT as escrow_funds_confirmed_at",
            "event_created_at::TEXT as event_created_at",
            "entries_submitted_at::TEXT as entries_submitted_at",
            "funding_broadcasted_at::TEXT as funding_broadcasted_at",
            "funding_confirmed_at::TEXT as funding_confirmed_at",
            "funding_settled_at::TEXT as funding_settled_at",
        ))
        .and_select((
            "expiry_broadcasted_at::TEXT as expiry_broadcasted_at",
            "outcome_broadcasted_at::TEXT as outcome_broadcasted_at",
            "delta_broadcasted_at::TEXT as delta_broadcasted_at",
            "completed_at::TEXT as completed_at",
            "failed_at::TEXT as failed_at",
            "errors",
        ))
        .from(
            "competitions"
                .left_join("entries")
                .on("entries.event_id = competitions.id")
                .left_join("tickets")
                .on("entries.ticket_id = tickets.id"),
        )
        .where_("competitions.id = ? ")
        .group_by((
            "competitions.id",
            "created_at",
            "event_submission",
            "event_announcement",
            "outcome_transaction",
            "competitions.funding_psbt_base64",
        ))
        .group_by((
            "funding_outpoint",
            "funding_transaction",
            "contract_parameters",
            "competitions.public_nonces",
            "aggregated_nonces",
            "competitions.partial_signatures",
            "signed_contract",
            "attestation",
        ))
        .group_by((
            "cancelled_at",
            "contracted_at",
            "competitions.signed_at",
            "escrow_funds_confirmed_at",
            "event_created_at",
            "entries_submitted_at",
            "funding_broadcasted_at",
            "funding_confirmed_at",
        ))
        .group_by((
            "funding_settled_at",
            "expiry_broadcasted_at",
            "outcome_broadcasted_at",
            "delta_broadcasted_at",
            "completed_at",
            "failed_at",
            "errors",
        ))
        .to_string();
        trace!("competition query: {}", query_str);

        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let mut rows = stmt.query(params![competition_id.to_string()])?;

        if let Some(row) = rows.next()? {
            let competition = row.try_into()?;
            Ok(competition)
        } else {
            Err(duckdb::Error::QueryReturnedNoRows)
        }
    }

    pub async fn get_and_reserve_ticket(
        &self,
        competition_id: Uuid,
        pubkey: &str,
    ) -> Result<Ticket, duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;
        conn.execute("BEGIN TRANSACTION", [])?;

        // First, find an available ticket
        let select_query = r#"
            SELECT tickets.id as id,
                   tickets.event_id as event_id,
                   entries.id as entry_id,
                   tickets.ephemeral_pubkey as ephemeral_pubkey,
                   encrypted_preimage,
                   hash,
                   payment_request,
                   reserved_by,
                   reserved_at::TEXT,
                   paid_at::TEXT,
                   settled_at::TEXT,
                   escrow_transaction
            FROM tickets
            LEFT JOIN entries ON tickets.id = entries.ticket_id
            WHERE tickets.event_id = ?
              AND entries.id IS NULL
              AND (
                  reserved_at IS NULL
                  OR (
                      reserved_at < NOW() - INTERVAL '10 minutes'
                      AND paid_at IS NULL
                  )
              )
            ORDER BY
                reserved_at IS NOT NULL,
                reserved_at NULLS FIRST,
                id
            LIMIT 1"#;

        let mut select_stmt = conn.prepare(select_query)?;
        let mut select_rows = select_stmt.query(params![competition_id.to_string()])?;

        let ticket_id = if let Some(row) = select_rows.next()? {
            let id: String = row.get(0)?;
            debug!("Found available ticket: {}", id);
            id
        } else {
            debug!("No available tickets found");
            conn.execute("ROLLBACK", [])?;
            return Err(duckdb::Error::QueryReturnedNoRows);
        };

        // Then do a simple update without RETURNING
        let update_query = r#"
            UPDATE tickets
            SET reserved_at = NOW(),
                reserved_by = ?
            WHERE id = ?
            AND event_id = ?"#;

        conn.execute(
            update_query,
            params![pubkey, ticket_id, competition_id.to_string()],
        )?;

        let get_ticket_query = r#"
            SELECT tickets.id as id,
                   tickets.event_id as event_id,
                   entries.id as entry_id,
                   tickets.ephemeral_pubkey as ephemeral_pubkey,
                   encrypted_preimage,
                   hash,
                   payment_request,
                   (NOW() + INTERVAL '10 minutes')::TEXT as expiry,
                   reserved_by,
                   reserved_at::TEXT,
                   paid_at::TEXT,
                   settled_at::TEXT,
                   escrow_transaction
            FROM tickets
            LEFT JOIN entries ON tickets.id = entries.ticket_id
            WHERE tickets.id = ?"#;

        let mut get_ticket_stmt = conn.prepare(get_ticket_query)?;
        let mut get_ticket_rows = get_ticket_stmt.query(params![ticket_id])?;

        if let Some(row) = get_ticket_rows.next()? {
            let ticket: Ticket = row.try_into()?;
            debug!("Successfully reserved ticket {}", ticket_id);
            conn.execute("COMMIT", [])?;
            Ok(ticket)
        } else {
            debug!("Failed to get updated ticket");
            conn.execute("ROLLBACK", [])?;
            Err(duckdb::Error::QueryReturnedNoRows)
        }
    }

    pub async fn get_pending_tickets(&self) -> Result<Vec<Ticket>, duckdb::Error> {
        let query = r#"
            SELECT tickets.id as id,
                   tickets.event_id as event_id,
                   entries.id as entry_id,
                   tickets.ephemeral_pubkey as ephemeral_pubkey,
                   encrypted_preimage,
                   hash,
                   payment_request,
                   (NOW() + INTERVAL '10 minutes')::TEXT as expiry,
                   reserved_by,
                   reserved_at::TEXT,
                   paid_at::TEXT,
                   settled_at::TEXT,
                   escrow_transaction
            FROM tickets
            LEFT JOIN entries ON tickets.id = entries.ticket_id
            WHERE reserved_at IS NOT NULL
              AND paid_at IS NULL
              AND entry_id IS NULL
              AND reserved_at > NOW() - INTERVAL '10 minutes'"#;

        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(query)?;
        let mut rows = stmt.query([])?;

        let mut tickets = Vec::new();
        while let Some(row) = rows.next()? {
            tickets.push(row.try_into()?);
        }

        Ok(tickets)
    }

    pub async fn get_paid_tickets(&self) -> Result<Vec<Ticket>, duckdb::Error> {
        let query = r#"
            SELECT tickets.id as id,
                   tickets.event_id as event_id,
                   entries.id as entry_id,
                   tickets.ephemeral_pubkey as ephemeral_pubkey,
                   encrypted_preimage,
                   hash,
                   payment_request,
                   (NOW() + INTERVAL '10 minutes')::TEXT as expiry,
                   reserved_by,
                   reserved_at::TEXT,
                   paid_at::TEXT,
                   settled_at::TEXT,
                   escrow_transaction
            FROM tickets
            LEFT JOIN entries ON tickets.id = entries.ticket_id
            WHERE paid_at IS NOT NULL
              AND settled_at IS NOT NULL
              AND reserved_at IS NOT NULL"#;

        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(query)?;
        let mut rows = stmt.query([])?;

        let mut tickets = Vec::new();
        while let Some(row) = rows.next()? {
            tickets.push(row.try_into()?);
        }

        Ok(tickets)
    }

    pub async fn get_paid_tickets_for_competition(
        &self,
        competition_id: Uuid,
    ) -> Result<Vec<Ticket>, duckdb::Error> {
        let query = r#"
            SELECT tickets.id as id,
                   tickets.event_id as event_id,
                   entries.id as entry_id,
                   tickets.ephemeral_pubkey as ephemeral_pubkey,
                   encrypted_preimage,
                   hash,
                   payment_request,
                   (NOW() + INTERVAL '10 minutes')::TEXT as expiry,
                   reserved_by,
                   reserved_at::TEXT,
                   paid_at::TEXT,
                   settled_at::TEXT,
                   escrow_transaction
            FROM tickets
            LEFT JOIN entries ON tickets.id = entries.ticket_id
            WHERE paid_at IS NOT NULL
              AND settled_at IS NOT NULL
              AND reserved_at IS NOT NULL
              AND tickets.event_id = ?"#;

        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(query)?;
        let mut rows = stmt.query(params![competition_id.to_string()])?;

        let mut tickets = Vec::new();
        while let Some(row) = rows.next()? {
            tickets.push(row.try_into()?);
        }

        Ok(tickets)
    }

    pub async fn get_ticket(&self, ticket_id: Uuid) -> Result<Ticket, duckdb::Error> {
        let query = r#"
            SELECT tickets.id as id,
                   tickets.event_id as event_id,
                   entries.id as entry_id,
                   tickets.ephemeral_pubkey as ephemeral_pubkey,
                   encrypted_preimage,
                   hash,
                   payment_request,
                   (NOW() + INTERVAL '10 minutes')::TEXT as expiry,
                   reserved_by,
                   reserved_at::TEXT,
                   paid_at::TEXT,
                   settled_at::TEXT,
                   escrow_transaction
            FROM tickets
            LEFT JOIN entries ON tickets.id = entries.ticket_id
            WHERE tickets.id = ?"#;

        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(query)?;
        let mut rows = stmt.query(params![ticket_id.to_string()])?;

        if let Some(row) = rows.next()? {
            row.try_into()
        } else {
            Err(duckdb::Error::QueryReturnedNoRows)
        }
    }

    pub async fn mark_ticket_paid(
        &self,
        ticket_hash: &str,
        competition_id: Uuid,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        let query = r#"
            UPDATE tickets
            SET paid_at = NOW()
            WHERE hash = ?
            AND event_id = ?
            AND paid_at IS NULL
            AND settled_at IS NULL
            AND reserved_at IS NOT NULL
            AND reserved_at > NOW() - INTERVAL '10 minutes'"#;

        let mut stmt = conn.prepare(query)?;
        let affected = stmt.execute(params![ticket_hash, competition_id.to_string(),])?;

        if affected == 0 {
            return Err(duckdb::Error::QueryReturnedNoRows);
        }

        Ok(())
    }

    pub async fn mark_ticket_settled(&self, ticket_id: Uuid) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        let query = r#"
            UPDATE tickets
            SET settled_at = NOW()
            WHERE id = ?
            AND settled_at IS NULL
            AND paid_at IS NOT NULL
            AND reserved_at IS NOT NULL"#;

        let mut stmt = conn.prepare(query)?;
        let affected = stmt.execute(params![ticket_id.to_string(),])?;

        if affected == 0 {
            return Err(duckdb::Error::QueryReturnedNoRows);
        }

        Ok(())
    }

    pub async fn update_ticket_escrow(
        &self,
        ticket_id: Uuid,
        ephemeral_pubkey: String,
        escrow_tx: String,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        let query = r#"
            UPDATE tickets
            SET escrow_transaction = ?, ephemeral_pubkey = ?
            WHERE id = ?"#;

        let mut stmt = conn.prepare(query)?;
        let affected =
            stmt.execute(params![escrow_tx, ephemeral_pubkey, ticket_id.to_string(),])?;

        if affected == 0 {
            return Err(duckdb::Error::QueryReturnedNoRows);
        }

        Ok(())
    }

    pub async fn get_tickets(
        &self,
        competition_id: Uuid,
    ) -> Result<HashMap<Uuid, Ticket>, duckdb::Error> {
        let query = select((
            "tickets.id as id",
            "tickets.event_id as event_id",
            "entries.id as entry_id",
            "tickets.ephemeral_pubkey as ephemeral_pubkey",
            "encrypted_preimage",
            "hash",
            "payment_request",
            "(NOW() + INTERVAL '10 minutes')::TEXT as expiry",
            "reserved_by::TEXT",
        ))
        .and_select((
            "reserved_at::TEXT",
            "paid_at::TEXT",
            "settled_at::TEXT",
            "escrow_transaction",
        ))
        .from(
            "tickets"
                .left_join("entries")
                .on("entries.ticket_id = tickets.id"),
        )
        .where_("tickets.event_id = ?")
        .to_string();

        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query)?;
        let mut rows = stmt.query(params![competition_id.to_string()])?;

        let mut tickets = HashMap::new();
        while let Some(row) = rows.next()? {
            let ticket: Ticket = row.try_into()?;
            if let Some(entry_id) = ticket.entry_id {
                tickets.insert(entry_id, ticket);
            }
        }

        Ok(tickets)
    }

    pub async fn update_ticket_payment_request(
        &self,
        ticket_id: Uuid,
        payment_request: &str,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;
        let affected = conn.execute(
            "UPDATE tickets SET payment_request = ? WHERE id = ?",
            params![payment_request, ticket_id.to_string()],
        )?;

        if affected == 0 {
            return Err(duckdb::Error::QueryReturnedNoRows);
        }

        Ok(())
    }

    pub async fn clear_ticket_reservation(&self, ticket_id: Uuid) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;

        let query = r#"
            UPDATE tickets
            SET reserved_at = NULL,
                reserved_by = NULL,
                paid_at = NULL,
                escrow_transaction = NULL
            WHERE id = ?
            AND settled_at IS NULL"#;

        let mut stmt = conn.prepare(query)?;
        let affected = stmt.execute(params![ticket_id.to_string()])?;

        if affected == 0 {
            debug!(
                "No ticket reservation found for ticket ID, skipping delete: {}",
                ticket_id
            );
            return Ok(());
        }

        Ok(())
    }
}
