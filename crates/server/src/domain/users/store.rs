use duckdb::{params, types::Type, Connection};
use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, macros::format_description, OffsetDateTime};

use super::run_migrations;
use crate::{
    domain::{DBConnection, Error},
    RegisterPayload,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub nostr_pubkey: String,
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

pub struct UserStore {
    db_connection: DBConnection,
}

impl UserStore {
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

    pub async fn register_user(
        &self,
        nostr_pubkey: String,
        user: RegisterPayload,
    ) -> Result<User, Error> {
        let now = OffsetDateTime::now_utc();
        let created_at = now
            .format(&Rfc3339)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
        let updated_at = now
            .format(&Rfc3339)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
        let conn = self.db_connection.new_write_connection_retry().await?;
        let mut stmt = conn.prepare(
            "INSERT INTO user (
                nostr_pubkey,
                encrypted_bitcoin_private_key,
                network,
                created_at,
                updated_at
            ) VALUES (?, ?, ?, ?, ?)",
        )?;

        stmt.execute(params![
            nostr_pubkey,
            user.encrypted_bitcoin_private_key,
            user.network,
            created_at,
            updated_at,
        ])?;

        Ok(User {
            nostr_pubkey,
            encrypted_bitcoin_private_key: user.encrypted_bitcoin_private_key,
            network: user.network,
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn login(&self, pubkey: String) -> Result<User, Error> {
        //raw date format 2024-08-11 00:27:39.013046-04
        let sql_time_format = format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second][optional [.[subsecond]]][offset_hour]"
        );
        let conn = self.db_connection.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(
            "SELECT
                nostr_pubkey,
                encrypted_bitcoin_private_key,
                network,
                created_at::TEXT as created_at,
                updated_at::TEXT as updated_at
            FROM user
            WHERE nostr_pubkey = ?",
        )?;

        let mut rows = stmt.query(params![pubkey])?;

        if let Some(row) = rows.next()? {
            let user = User {
                nostr_pubkey: row.get(0)?,
                encrypted_bitcoin_private_key: row.get(1)?,
                network: row.get(2)?,
                created_at: row
                    .get::<usize, String>(3)
                    .map(|val| {
                        println!("{}", val);
                        OffsetDateTime::parse(&val, &sql_time_format)
                    })?
                    .map_err(|e| {
                        duckdb::Error::FromSqlConversionFailure(5, Type::Any, Box::new(e))
                    })?,
                updated_at: row
                    .get::<usize, String>(4)
                    .map(|val| OffsetDateTime::parse(&val, &sql_time_format))?
                    .map_err(|e| {
                        duckdb::Error::FromSqlConversionFailure(6, Type::Any, Box::new(e))
                    })?,
            };
            Ok(user)
        } else {
            Err(Error::NotFound(format!(
                "User not found with pubkey: {}",
                pubkey
            )))
        }
    }
}
