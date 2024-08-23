use dlctix::bitcoin::XOnlyPublicKey;
use duckdb::{params, params_from_iter, types::Value, AccessMode, Config, Connection};
use log::info;
use scooby::postgres::select;
use std::time::Duration as StdDuration;
use tokio::time::timeout;

use super::{run_migrations, SearchBy, UserEntry};

pub struct CompetitionData {
    connection_path: String,
    retry_duration: StdDuration,
    retry_max_attemps: i32,
}

impl CompetitionData {
    pub fn new(path: &str) -> Result<Self, duckdb::Error> {
        let connection_path = format!("{}/competitions.db3", path);
        let mut conn = Connection::open(connection_path.clone())?;
        run_migrations(&mut conn)?;
        Ok(Self {
            connection_path,
            retry_duration: StdDuration::from_millis(100),
            retry_max_attemps: 5,
        })
    }

    async fn new_readonly_connection(&self) -> Result<Connection, duckdb::Error> {
        let config = Config::default().access_mode(AccessMode::ReadOnly)?;
        Connection::open_with_flags(self.connection_path.clone(), config)
    }

    pub async fn new_readonly_connection_retry(&self) -> Result<Connection, duckdb::Error> {
        let mut attempt = 0;
        loop {
            match timeout(self.retry_duration, self.new_readonly_connection()).await {
                Ok(Ok(connection)) => return Ok(connection),
                Ok(Err(e)) => {
                    if attempt >= self.retry_max_attemps
                        || !e.to_string().contains("Could not set lock on file")
                    {
                        return Err(e);
                    }
                    info!("Retrying: {}", e);
                    attempt += 1;
                }
                Err(_) => {
                    return Err(duckdb::Error::DuckDBFailure(
                        duckdb::ffi::Error {
                            code: duckdb::ErrorCode::DatabaseLocked,
                            extended_code: 0,
                        },
                        None,
                    ));
                }
            }
        }
    }

    async fn new_write_connection(&self) -> Result<Connection, duckdb::Error> {
        let config = Config::default().access_mode(AccessMode::ReadWrite)?;
        Connection::open_with_flags(self.connection_path.clone(), config)
    }

    pub async fn new_write_connection_retry(&self) -> Result<Connection, duckdb::Error> {
        let mut attempt = 0;
        loop {
            match timeout(self.retry_duration, self.new_write_connection()).await {
                Ok(Ok(connection)) => return Ok(connection),
                Ok(Err(e)) => {
                    if attempt >= self.retry_max_attemps
                        || !e.to_string().contains("Could not set lock on file")
                    {
                        return Err(e);
                    }
                    info!("Retrying: {}", e);
                    attempt += 1;
                }
                Err(_) => {
                    return Err(duckdb::Error::DuckDBFailure(
                        duckdb::ffi::Error {
                            code: duckdb::ErrorCode::DatabaseLocked,
                            extended_code: 0,
                        },
                        None,
                    ));
                }
            }
        }
    }

    pub async fn get_stored_public_key(&self) -> Result<XOnlyPublicKey, duckdb::Error> {
        let select = select("pubkey").from("oracle_metadata");
        let conn = self.new_readonly_connection_retry().await?;
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
        let conn = self.new_write_connection_retry().await?;
        let mut stmt =
            conn.prepare("INSERT INTO coordinator_metadata (pubkey,name) VALUES(?,?)")?;
        stmt.execute([pubkey_raw, name.into()])?;
        Ok(())
    }

    pub async fn add_entry(&self, entry: UserEntry) -> Result<UserEntry, duckdb::Error> {
        let conn = self.new_write_connection_retry().await?;
        let mut stmt = conn.prepare("INSERT INTO entries (id, event_id, pubkey) VALUES(?,?,?)")?;
        stmt.execute(params![
            entry.id.to_string(),
            entry.event_id.to_string(),
            entry.pubkey
        ])?;
        Ok(entry)
    }

    pub async fn get_entries(&self, filter: SearchBy) -> Result<Vec<UserEntry>, duckdb::Error> {
        let mut select = select(("id", "event_id", "pubkey")).from("entries");
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
        let conn = self.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let mut rows = if let Some(ids) = filter.event_ids {
            let mut params: Vec<Value> = ids
                .iter()
                .map(|event_id| Value::Text(event_id.to_string()))
                .collect();
            params.push(Value::Text(filter.pubkey));
            stmt.query(params_from_iter(params.iter()))
        } else {
            stmt.query(params![filter.pubkey])
        }?;

        let mut user_entries: Vec<UserEntry> = vec![];
        while let Some(row) = rows.next()? {
            let data: UserEntry = row.try_into()?;
            user_entries.push(data.clone());
        }

        Ok(user_entries)
    }
}
