use dlctix::{
    bitcoin::XOnlyPublicKey,
    musig2::{PartialSignature, PubNonce},
    SigMap,
};
use duckdb::{params, params_from_iter, types::Value, Connection};
use log::debug;
use scooby::postgres::{select, Joinable};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::domain::DBConnection;

use super::{run_comeptition_migrations, Competition, SearchBy, UserEntry};

#[derive(Clone)]
pub struct CompetitionStore {
    db_connection: DBConnection,
}

impl CompetitionStore {
    pub fn new(db_connection: DBConnection) -> Result<Self, duckdb::Error> {
        let mut conn = Connection::open(db_connection.connection_path.clone())?;
        run_comeptition_migrations(&mut conn)?;
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
        ticket_hash: String,
        ticket_preimage: String,
    ) -> Result<UserEntry, duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;
        let mut stmt = conn.prepare(
            "INSERT INTO entries (
                id,
                event_id,
                pubkey,
                ephemeral_pubkey,
                ephemeral_privatekey_user_encrypted,
                ticket_preimage,
                ticket_hash,
                payout_preimage_user_encrypted,
                payout_hash) VALUES(?,?,?,?,?,?,?,?,?)",
        )?;
        stmt.execute(params![
            entry.id.to_string(),
            entry.event_id.to_string(),
            entry.pubkey,
            entry.ephemeral_pubkey,
            entry.ephemeral_privatekey_encrypted,
            ticket_preimage,
            ticket_hash,
            entry.payout_preimage_encrypted,
            entry.payout_hash
        ])?;
        Ok(entry)
    }

    pub async fn add_partial_signatures(
        &self,
        entry_id: Uuid,
        partial_sigs: SigMap<PartialSignature>,
    ) -> Result<(), duckdb::Error> {
        let conn = self.db_connection.new_write_connection_retry().await?;
        let sigs_blob = serde_json::to_vec(&partial_sigs)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;

        let mut stmt = conn.prepare(
            "UPDATE entries
                SET partial_signatures = ?,
                    signed_at = NOW()
                WHERE id = ?",
        )?;

        stmt.execute(params![Value::Blob(sigs_blob), entry_id.to_string(),])?;

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

    pub async fn get_competition_entries(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<UserEntry>, duckdb::Error> {
        let select = select((
            "id",
            "event_id",
            "pubkey",
            "ephemeral_pubkey",
            "ephemeral_privatekey_user_encrypted",
        ))
        .and_select((
            "ephemeral_privatekey",
            "public_nonces",
            "partial_signatures",
            "ticket_preimage",
            "ticket_hash",
            "payout_preimage_user_encrypted",
        ))
        .and_select((
            "payout_hash",
            "payout_preimage",
            "signed_at::TEXT",
            "paid_at::TEXT",
        ))
        .from("entries");
        let query_str = select.where_("event_id = ?").to_string();
        debug!("get competition entries: {}", query_str);
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
            "id",
            "event_id",
            "pubkey",
            "ephemeral_pubkey",
            "ephemeral_privatekey_user_encrypted",
            "ephemeral_privatekey",
            "public_nonces",
        ))
        .and_select((
            "partial_signatures",
            "ticket_preimage",
            "ticket_hash",
            "payout_preimage_user_encrypted",
            "payout_hash",
            "payout_preimage",
            "signed_at::TEXT",
            "paid_at::TEXT",
        ))
        .from("entries");
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
            let where_clause = format!("event_id IN {}", event_ids_val);
            select = select.clone().where_(where_clause);
        }
        let query_str = select.where_("pubkey = ?").to_string();
        debug!("get user entries query: {}", query_str);
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

    pub async fn add_competition(
        &self,
        competition: Competition,
    ) -> Result<Competition, duckdb::Error> {
        let created_at = OffsetDateTime::format(competition.created_at, &Rfc3339)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;

        let conn = self.db_connection.new_write_connection_retry().await?;
        let mut stmt = conn.prepare(
            "INSERT INTO competitions (
                      id,
                      created_at,
                      total_competition_pool,
                      total_allowed_entries,
                      entry_fee,
                      event_announcement) VALUES(?,?,?,?,?,?)",
        )?;
        let announcement = serde_json::to_vec(&competition.event_announcement)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
        stmt.execute(params![
            competition.id.to_string(),
            created_at,
            competition.total_competition_pool,
            competition.total_allowed_entries,
            competition.entry_fee,
            Value::Blob(announcement)
        ])?;
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
                funding_transaction = ?,
                contract_parameters = ?,
                public_nonces = ?,
                aggregated_nonces = ?,
                partial_signatures = ?,
                signed_contract = ?,
                contracted_at = ?,
                signed_at = ?,
                funding_broadcasted_at = ?,
                cancelled_at = ?,
                failed_at = ?,
                errors = ?
                WHERE id = ?";

            if let Some(funding_transaction) = &competition.funding_transaction {
                params.push(Value::Blob(
                    serde_json::to_vec(funding_transaction)
                        .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?,
                ));
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

            for timestamp in [
                &competition.contracted_at,
                &competition.signed_at,
                &competition.funding_broadcasted_at,
                &competition.cancelled_at,
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

    pub async fn get_competitions(&self) -> Result<Vec<Competition>, duckdb::Error> {
        let query_str = select((
            // Basic identification and metadata (indices 0-5)
            "competitions.id as id",
            "created_at::TEXT as created_at",
            "total_competition_pool",
            "total_allowed_entries",
            "entry_fee",
            "event_announcement",
        ))
        .and_select((
            // Count fields (indices 6-9)
            "COUNT(entries.id) as total_entries",
            "COUNT(entries.id) FILTER (entries.public_nonces IS NOT NULL) as total_entry_nonces",
            "COUNT(entries.id) FILTER (entries.signed_at IS NOT NULL) as total_signed_entries",
            "COUNT(entries.id) FILTER (entries.paid_at IS NOT NULL) as total_paid_entries",
        ))
        .and_select((
            // Transaction and contract fields (indices 10-15)
            "funding_transaction",
            "contract_parameters",
            "competitions.public_nonces as public_nonces",
            "aggregated_nonces",
            "competitions.partial_signatures as partial_signatures",
            "signed_contract",
        ))
        .and_select((
            // Timestamp fields (indices 16-20)
            "cancelled_at::TEXT as cancelled_at",
            "contracted_at::TEXT as contracted_at",
            "competitions.signed_at::TEXT as signed_at",
            "funding_broadcasted_at::TEXT as funding_broadcasted_at",
            "failed_at::TEXT as failed_at",
            "errors",
        ))
        .from(
            "competitions"
                .join("entries")
                .on("entries.event_id = competitions.id"),
        )
        // filters out competitions in terminal states
        .where_("funding_broadcasted_at IS NULL AND cancelled_at IS NULL AND failed_at IS NULL")
        .group_by((
            "competitions.id",
            "created_at",
            "total_competition_pool",
            "total_allowed_entries",
            "entry_fee",
            "event_announcement",
            "funding_transaction",
        ))
        .group_by((
            "contract_parameters",
            "competitions.public_nonces",
            "aggregated_nonces",
            "competitions.partial_signatures",
        ))
        .group_by((
            "signed_contract",
            "cancelled_at",
            "contracted_at",
            "competitions.signed_at",
            "funding_broadcasted_at",
            "failed_at",
            "errors",
        ))
        .to_string();
        debug!("competitions query: {}", query_str);
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
            // Basic identification and metadata (indices 0-5)
            "competitions.id as id",
            "created_at::TEXT as created_at",
            "total_competition_pool",
            "total_allowed_entries",
            "entry_fee",
            "event_announcement",
        ))
        .and_select((
            // Count fields (indices 6-9)
            "COUNT(entries.id) as total_entries",
            "COUNT(entries.id) FILTER (entries.public_nonces IS NOT NULL) as total_entry_nonces",
            "COUNT(entries.id) FILTER (entries.signed_at IS NOT NULL) as total_signed_entries",
            "COUNT(entries.id) FILTER (entries.paid_at IS NOT NULL) as total_paid_entries",
        ))
        .and_select((
            // Transaction and contract fields (indices 10-15)
            "funding_transaction",
            "contract_parameters",
            "competitions.public_nonces as public_nonces",
            "aggregated_nonces",
            "competitions.partial_signatures as partial_signatures",
            "signed_contract",
        ))
        .and_select((
            // Timestamp fields (indices 16-20)
            "cancelled_at::TEXT as cancelled_at",
            "contracted_at::TEXT as contracted_at",
            "competitions.signed_at::TEXT as signed_at",
            "funding_broadcasted_at::TEXT as funding_broadcasted_at",
            "failed_at::TEXT as failed_at",
            "errors",
        ))
        .from(
            "competitions"
                .join("entries")
                .on("entries.event_id = competitions.id"),
        )
        .where_("competitions.id = ? ")
        .group_by((
            "competitions.id",
            "created_at",
            "total_competition_pool",
            "total_allowed_entries",
            "entry_fee",
            "event_announcement",
            "funding_transaction",
        ))
        .group_by((
            "contract_parameters",
            "competitions.public_nonces",
            "aggregated_nonces",
            "competitions.partial_signatures",
        ))
        .group_by((
            "signed_contract",
            "cancelled_at",
            "contracted_at",
            "competitions.signed_at",
            "funding_broadcasted_at",
            "failed_at",
            "errors",
        ))
        .to_string();
        debug!("competition query: {}", query_str);

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
}
