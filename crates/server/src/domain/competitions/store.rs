use dlctix::bitcoin::XOnlyPublicKey;
use duckdb::{params, params_from_iter, types::Value, Connection};
use scooby::postgres::{select, with, Aliasable, Joinable};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::domain::DBConnection;

use super::{run_migrations, Competition, SearchBy, UserEntry};

pub struct CompetitionStore {
    db_connection: DBConnection,
}

impl CompetitionStore {
    pub fn new(db_connection: DBConnection) -> Result<Self, duckdb::Error> {
        let mut conn = Connection::open(db_connection.connection_path.clone())?;
        run_migrations(&mut conn)?;
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

    pub async fn get_competition_entries(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<UserEntry>, duckdb::Error> {
        let select = select((
            "id",
            "event_id",
            "pubkey",
            "ticket_preimage",
            "signed_at::TEXT",
            "paid_at::TEXT",
        ))
        .from("entries");
        let query_str = select.where_("event_id = ?").to_string();
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
            "ephemeral_privatekey_encrypted",
        ))
        .and_select((
            "payout_hash",
            "payout_preimage_encrypted",
            "ticket_hash",
            "signed_at::TEXT",
            "paid_at::TEXT",
        ))
        .from("entries");
        if let Some(ids) = filter.event_ids.clone() {
            let mut event_ids_val = String::new();
            event_ids_val.push('(');
            for (index, _) in ids.iter().enumerate() {
                event_ids_val.push('?');
                if index < ids.len() {
                    event_ids_val.push(',');
                }
            }
            event_ids_val.push(')');
            let where_clause = format!("event_id IN {}", event_ids_val);
            select = select.clone().where_(where_clause);
        }
        let query_str = select.where_("pubkey = ?").to_string();
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
        let mut params: Vec<Value> = vec![];
        let mut values = String::from("VALUES");
        let number_competitions = competitions.len();
        for (index, competition) in competitions.iter().enumerate() {
            values.push_str("(?,?,?,?,?,?,?,?,?,?)");
            if index + 1 < number_competitions {
                values.push(',');
            }
            params.push(Value::Text(competition.id.to_string()));
            if let Some(funding_transaction) = competition.funding_transaction {
                let funding_transaction = serde_json::to_vec(&funding_transaction)
                    .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Blob(funding_transaction));
            } else {
                params.push(Value::Null)
            }
            if let Some(contract_parameters) = competition.contract_parameters.clone() {
                let contract = serde_json::to_vec(&contract_parameters)
                    .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Blob(contract));
            } else {
                params.push(Value::Null)
            }
            if let Some(public_nonces) = competition.public_nonces.clone() {
                let nonces = serde_json::to_vec(&public_nonces)
                    .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Blob(nonces));
            } else {
                params.push(Value::Null)
            }
            if let Some(cancelled_at) = competition.cancelled_at.clone() {
                let cancelled_at = OffsetDateTime::format(cancelled_at, &Rfc3339)
                    .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Text(cancelled_at));
            } else {
                params.push(Value::Null)
            }
            if let Some(contracted_at) = competition.contracted_at.clone() {
                let contracted_at = OffsetDateTime::format(contracted_at, &Rfc3339)
                    .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Text(contracted_at));
            } else {
                params.push(Value::Null)
            }
            if let Some(signed_at) = competition.signed_at.clone() {
                let signed_at = OffsetDateTime::format(signed_at, &Rfc3339)
                    .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Text(signed_at));
            } else {
                params.push(Value::Null)
            }
            if let Some(funding_broadcasted_at) = competition.funding_broadcasted_at.clone() {
                let funding_broadcasted_at =
                    OffsetDateTime::format(funding_broadcasted_at, &Rfc3339)
                        .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Text(funding_broadcasted_at));
            } else {
                params.push(Value::Null)
            }
            if let Some(failed_at) = competition.funding_broadcasted_at.clone() {
                let failed_at = OffsetDateTime::format(failed_at, &Rfc3339)
                    .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Text(failed_at));
            } else {
                params.push(Value::Null)
            }
            if !competition.errors.is_empty() {
                let errors = serde_json::to_vec(&competition.errors)
                    .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
                params.push(Value::Blob(errors));
            } else {
                params.push(Value::Null)
            }
        }

        let competition_to_update = select((
            "id",
            "funding_transaction",
            "contract_parameters",
            "public_nonces",
            "cancelled_at",
            "contracted_at",
            "signed_at",
            "funding_broadcasted_at",
            "failed_at",
            "errors",
        ))
        .from(values.as_("comp_updates(id, funding_transaction, contract_parameters, public_nonces, cancelled_at, contracted_at, signed_at, funding_broadcasted_at, failed_at, errors)"));

        let competition_update_query = with("update_competitions")
            .as_(competition_to_update)
            .update("competitions")
            .set(
                "funding_transaction",
                "update_competitions.funding_transaction",
            )
            .set(
                "contract_parameters",
                "update_competitions.contract_parameters",
            )
            .set("public_nonces", "update_competitions.public_nonces")
            .set("cancelled_at", "update_competitions.cancelled_at")
            .set("contracted_at", "update_competitions.contracted_at")
            .set("signed_at", "update_competitions.signed_at")
            .set(
                "funding_broadcasted_at",
                "update_competitions.funding_broadcasted_at",
            )
            .set("failed_at", "update_competitions.failed_at")
            .set("errors", "update_competitions.errors")
            .where_("competitions.id = update_competitions.id")
            .to_string();

        let conn = self.db_connection.new_write_connection_retry().await?;
        let mut stmt = conn.prepare(&competition_update_query)?;
        stmt.execute(params_from_iter(params))?;
        Ok(())
    }

    pub async fn get_competitions(&self) -> Result<Vec<Competition>, duckdb::Error> {
        let query_str = select((
            "competitions.id as id",
            "created_at::TEXT",
            "total_competition_pool",
            "total_allowed_entries",
            "entry_fee",
            "COUNT(entries.id) as total_entries",
            "COUNT(entries.id) FILTER (entries.signed_at IS NOT NULL) as total_signed_entries",
            "COUNT(entries.id) FILTER (entries.paid_at IS NOT NULL) as total_paid_entries",
        ))
        .and_select((
            "funding_transaction",
            "contract_parameters",
            "public_nonces",
            "cancelled_at::TEXT",
            "contracted_at::TEXT",
            "competitions.signed_at::TEXT as signed_at",
            "funding_broadcasted_at::TEXT",
            "failed_at::TEXT",
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
            "funding_transaction",
            "contract_parameters",
            "public_nonces",
            "cancelled_at",
        ))
        .group_by((
            "contracted_at",
            "competitions.signed_at",
            "funding_broadcasted_at",
            "failed_at",
            "errors",
        ))
        .to_string();

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
            "created_at::TEXT",
            "total_competition_pool",
            "total_allowed_entries",
            "entry_fee",
            "COUNT(entries.id) as total_entries",
            "COUNT(entries.id) FILTER (entries.signed_at IS NOT NULL) as total_signed_entries",
            "COUNT(entries.id) FILTER (entries.paid_at IS NOT NULL) as total_paid_entries",
        ))
        .and_select((
            "funding_transaction",
            "contract_parameters",
            "public_nonces",
            "cancelled_at::TEXT",
            "contracted_at::TEXT",
            "competitions.signed_at::TEXT as signed_at",
            "funding_broadcasted_at::TEXT",
            "failed_at::TEXT",
            "errors",
        ))
        .from(
            "competitions"
                .join("entries")
                .on("entries.event_id = competitions.id"),
        )
        // filters out competitions in terminal states
        .where_("funding_broadcasted_at IS NULL AND cancelled_at IS NULL AND failed_at IS NULL AND competitions.id = ? ")
        .group_by((
            "competitions.id",
            "created_at",
            "total_competition_pool",
            "total_allowed_entries",
            "entry_fee",
            "funding_transaction",
            "contract_parameters",
            "public_nonces",
            "cancelled_at",
        ))
        .group_by((
            "contracted_at",
            "competitions.signed_at",
            "funding_broadcasted_at",
            "failed_at",
            "errors",
        ))
        .to_string();

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
