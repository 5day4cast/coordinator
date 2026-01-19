use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqliteRow, FromRow, Row};
use time::OffsetDateTime;

use crate::{
    api::routes::RegisterPayload,
    domain::Error,
    infra::db::{parse_required_datetime, DBConnection},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub nostr_pubkey: String,
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
    pub username: Option<String>,
    pub password_hash: Option<String>,
    pub encrypted_nsec: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl FromRow<'_, SqliteRow> for User {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(User {
            nostr_pubkey: row.get("nostr_pubkey"),
            encrypted_bitcoin_private_key: row.get("encrypted_bitcoin_private_key"),
            network: row.get("network"),
            username: row.get("username"),
            password_hash: row.get("password_hash"),
            encrypted_nsec: row.get("encrypted_nsec"),
            created_at: parse_required_datetime(row, "created_at")?,
            updated_at: parse_required_datetime(row, "updated_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct UserStore {
    db_connection: DBConnection,
}

impl UserStore {
    pub fn new(db_connection: DBConnection) -> Self {
        Self { db_connection }
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        self.db_connection.ping().await
    }

    pub async fn register_user(
        &self,
        nostr_pubkey: String,
        user: RegisterPayload,
    ) -> Result<User, Error> {
        let now = OffsetDateTime::now_utc();
        let encrypted_key = user.encrypted_bitcoin_private_key.clone();
        let network = user.network.clone();

        let user = self
            .db_connection
            .execute_write(move |pool| async move {
                sqlx::query_as::<_, User>(
                    "INSERT INTO user (
                        nostr_pubkey,
                        encrypted_bitcoin_private_key,
                        network,
                        created_at,
                        updated_at
                    ) VALUES (?, ?, ?, ?, ?)
                    RETURNING nostr_pubkey, encrypted_bitcoin_private_key, network, username, password_hash, encrypted_nsec, created_at, updated_at",
                )
                .bind(nostr_pubkey)
                .bind(encrypted_key)
                .bind(network)
                .bind(now)
                .bind(now)
                .fetch_one(&pool)
                .await
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => Error::DbError(e),
                e => Error::BadRequest(e.to_string()),
            })?;

        Ok(user)
    }

    pub async fn login(&self, pubkey: String) -> Result<User, Error> {
        let user = sqlx::query_as::<_, User>(
            "SELECT
                nostr_pubkey,
                encrypted_bitcoin_private_key,
                network,
                username,
                password_hash,
                encrypted_nsec,
                created_at,
                updated_at
            FROM user
            WHERE nostr_pubkey = ?",
        )
        .bind(&pubkey)
        .fetch_optional(self.db_connection.read())
        .await?;

        user.ok_or_else(|| Error::NotFound(format!("User not found with pubkey: {}", pubkey)))
    }

    pub async fn update_user(
        &self,
        nostr_pubkey: &str,
        user: RegisterPayload,
    ) -> Result<User, Error> {
        let now = OffsetDateTime::now_utc();
        let nostr_pubkey_owned = nostr_pubkey.to_string();
        let encrypted_key = user.encrypted_bitcoin_private_key.clone();
        let network = user.network.clone();

        let rows_affected = self
            .db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE user
                     SET encrypted_bitcoin_private_key = ?,
                         network = ?,
                         updated_at = ?
                     WHERE nostr_pubkey = ?",
                )
                .bind(encrypted_key)
                .bind(network)
                .bind(now)
                .bind(nostr_pubkey_owned)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => Error::DbError(e),
                e => Error::BadRequest(e.to_string()),
            })?;

        if rows_affected == 0 {
            return Err(Error::NotFound(format!(
                "User not found with pubkey: {}",
                nostr_pubkey
            )));
        }

        // Return the updated user
        self.login(nostr_pubkey.to_string()).await
    }

    pub async fn delete_user(&self, nostr_pubkey: &str) -> Result<(), Error> {
        let nostr_pubkey_owned = nostr_pubkey.to_string();

        let rows_affected = self
            .db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query("DELETE FROM user WHERE nostr_pubkey = ?")
                    .bind(nostr_pubkey_owned)
                    .execute(&pool)
                    .await?;
                Ok(result.rows_affected())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => Error::DbError(e),
                e => Error::BadRequest(e.to_string()),
            })?;

        if rows_affected == 0 {
            return Err(Error::NotFound(format!(
                "User not found with pubkey: {}",
                nostr_pubkey
            )));
        }

        Ok(())
    }

    pub async fn get_all_users(&self) -> Result<Vec<User>, Error> {
        let users = sqlx::query_as::<_, User>(
            r#"SELECT
                nostr_pubkey,
                encrypted_bitcoin_private_key,
                network,
                username,
                password_hash,
                encrypted_nsec,
                created_at,
                updated_at
            FROM user
            ORDER BY created_at DESC"#,
        )
        .fetch_all(self.db_connection.read())
        .await?;

        Ok(users)
    }

    pub async fn get_user_count(&self) -> Result<i64, Error> {
        let result: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user")
            .fetch_one(self.db_connection.read())
            .await?;

        Ok(result)
    }

    pub async fn user_exists(&self, nostr_pubkey: &str) -> Result<bool, Error> {
        let result: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user WHERE nostr_pubkey = ?)")
                .bind(nostr_pubkey)
                .fetch_one(self.db_connection.read())
                .await?;

        Ok(result == 1)
    }

    pub async fn get_users_by_network(&self, network: &str) -> Result<Vec<User>, Error> {
        let users = sqlx::query_as::<_, User>(
            r#"SELECT
                nostr_pubkey,
                encrypted_bitcoin_private_key,
                network,
                username,
                password_hash,
                encrypted_nsec,
                created_at,
                updated_at
            FROM user
            WHERE network = ?1
            ORDER BY created_at DESC"#,
        )
        .bind(network)
        .fetch_all(self.db_connection.read())
        .await?;

        Ok(users)
    }

    pub async fn get_recent_users(&self, limit: i64) -> Result<Vec<User>, Error> {
        let users = sqlx::query_as::<_, User>(
            r#"SELECT
                nostr_pubkey,
                encrypted_bitcoin_private_key,
                network,
                username,
                password_hash,
                encrypted_nsec,
                created_at,
                updated_at
            FROM user
            ORDER BY created_at DESC
            LIMIT ?1"#,
        )
        .bind(limit)
        .fetch_all(self.db_connection.read())
        .await?;

        Ok(users)
    }

    pub async fn register_username_user(
        &self,
        nostr_pubkey: String,
        username: String,
        password_hash: String,
        encrypted_nsec: String,
        encrypted_bitcoin_private_key: String,
        network: String,
    ) -> Result<User, Error> {
        let now = OffsetDateTime::now_utc();

        let user = self
            .db_connection
            .execute_write(move |pool| async move {
                sqlx::query_as::<_, User>(
                    "INSERT INTO user (
                        nostr_pubkey,
                        encrypted_bitcoin_private_key,
                        network,
                        username,
                        password_hash,
                        encrypted_nsec,
                        created_at,
                        updated_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    RETURNING nostr_pubkey, encrypted_bitcoin_private_key, network, username, password_hash, encrypted_nsec, created_at, updated_at",
                )
                .bind(nostr_pubkey)
                .bind(encrypted_bitcoin_private_key)
                .bind(network)
                .bind(username)
                .bind(password_hash)
                .bind(encrypted_nsec)
                .bind(now)
                .bind(now)
                .fetch_one(&pool)
                .await
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => Error::DbError(e),
                e => Error::BadRequest(e.to_string()),
            })?;

        Ok(user)
    }

    pub async fn get_user_by_username(&self, username: &str) -> Result<User, Error> {
        let user = sqlx::query_as::<_, User>(
            "SELECT
                nostr_pubkey,
                encrypted_bitcoin_private_key,
                network,
                username,
                password_hash,
                encrypted_nsec,
                created_at,
                updated_at
            FROM user
            WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(self.db_connection.read())
        .await?;

        user.ok_or_else(|| Error::NotFound(format!("User not found with username: {}", username)))
    }

    pub async fn username_exists(&self, username: &str) -> Result<bool, Error> {
        let result: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user WHERE username = ?)")
                .bind(username)
                .fetch_one(self.db_connection.read())
                .await?;

        Ok(result == 1)
    }

    pub async fn update_password(
        &self,
        nostr_pubkey: &str,
        new_password_hash: String,
        new_encrypted_nsec: String,
    ) -> Result<(), Error> {
        let now = OffsetDateTime::now_utc();
        let nostr_pubkey_owned = nostr_pubkey.to_string();

        let rows_affected = self
            .db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE user
                     SET password_hash = ?,
                         encrypted_nsec = ?,
                         updated_at = ?
                     WHERE nostr_pubkey = ?",
                )
                .bind(new_password_hash)
                .bind(new_encrypted_nsec)
                .bind(now)
                .bind(nostr_pubkey_owned)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => Error::DbError(e),
                e => Error::BadRequest(e.to_string()),
            })?;

        if rows_affected == 0 {
            return Err(Error::NotFound(format!(
                "User not found with pubkey: {}",
                nostr_pubkey
            )));
        }

        Ok(())
    }

    pub async fn get_pubkey_by_username(&self, username: &str) -> Result<String, Error> {
        let pubkey: Option<String> =
            sqlx::query_scalar("SELECT nostr_pubkey FROM user WHERE username = ?")
                .bind(username)
                .fetch_optional(self.db_connection.read())
                .await?;

        pubkey.ok_or_else(|| Error::NotFound(format!("User not found with username: {}", username)))
    }

    pub async fn update_username(&self, nostr_pubkey: &str, username: String) -> Result<(), Error> {
        let now = OffsetDateTime::now_utc();
        let nostr_pubkey_owned = nostr_pubkey.to_string();

        let rows_affected = self
            .db_connection
            .execute_write(move |pool| async move {
                let result = sqlx::query(
                    "UPDATE user
                     SET username = ?,
                         updated_at = ?
                     WHERE nostr_pubkey = ?",
                )
                .bind(username)
                .bind(now)
                .bind(nostr_pubkey_owned)
                .execute(&pool)
                .await?;
                Ok(result.rows_affected())
            })
            .await
            .map_err(|e| match e {
                crate::infra::db::DatabaseWriteError::Sqlx(e) => Error::DbError(e),
                e => Error::BadRequest(e.to_string()),
            })?;

        if rows_affected == 0 {
            return Err(Error::NotFound(format!(
                "User not found with pubkey: {}",
                nostr_pubkey
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;

    use super::*;

    fn create_store(pool: SqlitePool) -> UserStore {
        let db = DBConnection::new_with_pools(
            "test".to_string(),
            ":memory:".to_string(),
            pool.clone(),
            pool,
        );
        UserStore::new(db)
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn test_user_registration_and_login(pool: SqlitePool) {
        let store = create_store(pool);

        let register_payload = RegisterPayload {
            encrypted_bitcoin_private_key: "encrypted_key_123".to_string(),
            network: "testnet".to_string(),
        };

        let user = store
            .register_user("test_pubkey".to_string(), register_payload.clone())
            .await
            .unwrap();

        assert_eq!(user.nostr_pubkey, "test_pubkey");
        assert_eq!(user.encrypted_bitcoin_private_key, "encrypted_key_123");
        assert_eq!(user.network, "testnet");

        let logged_in_user = store.login("test_pubkey".to_string()).await.unwrap();
        assert_eq!(logged_in_user.nostr_pubkey, user.nostr_pubkey);
        assert_eq!(
            logged_in_user.encrypted_bitcoin_private_key,
            user.encrypted_bitcoin_private_key
        );

        let result = store.login("non_existent".to_string()).await;
        assert!(matches!(result, Err(Error::NotFound(_))));
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn test_user_exists(pool: SqlitePool) {
        let store = create_store(pool);

        let register_payload = RegisterPayload {
            encrypted_bitcoin_private_key: "encrypted_key_123".to_string(),
            network: "testnet".to_string(),
        };

        assert!(!store.user_exists("test_pubkey").await.unwrap());

        store
            .register_user("test_pubkey".to_string(), register_payload)
            .await
            .unwrap();

        assert!(store.user_exists("test_pubkey").await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn test_user_update(pool: SqlitePool) {
        let store = create_store(pool);

        let register_payload = RegisterPayload {
            encrypted_bitcoin_private_key: "encrypted_key_123".to_string(),
            network: "testnet".to_string(),
        };

        store
            .register_user("test_pubkey".to_string(), register_payload)
            .await
            .unwrap();

        let update_payload = RegisterPayload {
            encrypted_bitcoin_private_key: "new_encrypted_key_456".to_string(),
            network: "mainnet".to_string(),
        };

        let updated_user = store
            .update_user("test_pubkey", update_payload)
            .await
            .unwrap();
        assert_eq!(
            updated_user.encrypted_bitcoin_private_key,
            "new_encrypted_key_456"
        );
        assert_eq!(updated_user.network, "mainnet");
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn test_get_all_users(pool: SqlitePool) {
        let store = create_store(pool);

        for i in 0..3 {
            let payload = RegisterPayload {
                encrypted_bitcoin_private_key: format!("key_{}", i),
                network: "testnet".to_string(),
            };
            store
                .register_user(format!("pubkey_{}", i), payload)
                .await
                .unwrap();
        }

        let users = store.get_all_users().await.unwrap();
        assert_eq!(users.len(), 3);

        let count = store.get_user_count().await.unwrap();
        assert_eq!(count, 3);
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn test_get_users_by_network(pool: SqlitePool) {
        let store = create_store(pool);

        let testnet_payload = RegisterPayload {
            encrypted_bitcoin_private_key: "testnet_key".to_string(),
            network: "testnet".to_string(),
        };
        let mainnet_payload = RegisterPayload {
            encrypted_bitcoin_private_key: "mainnet_key".to_string(),
            network: "mainnet".to_string(),
        };

        store
            .register_user("testnet_user".to_string(), testnet_payload)
            .await
            .unwrap();
        store
            .register_user("mainnet_user".to_string(), mainnet_payload)
            .await
            .unwrap();

        let testnet_users = store.get_users_by_network("testnet").await.unwrap();
        let mainnet_users = store.get_users_by_network("mainnet").await.unwrap();

        assert_eq!(testnet_users.len(), 1);
        assert_eq!(mainnet_users.len(), 1);
        assert_eq!(testnet_users[0].network, "testnet");
        assert_eq!(mainnet_users[0].network, "mainnet");
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn test_get_recent_users(pool: SqlitePool) {
        let store = create_store(pool);

        for i in 0..5 {
            let payload = RegisterPayload {
                encrypted_bitcoin_private_key: format!("key_{}", i),
                network: "testnet".to_string(),
            };
            store
                .register_user(format!("pubkey_{}", i), payload)
                .await
                .unwrap();
        }

        let recent_users = store.get_recent_users(3).await.unwrap();
        assert_eq!(recent_users.len(), 3);
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn test_delete_user(pool: SqlitePool) {
        let store = create_store(pool);

        let register_payload = RegisterPayload {
            encrypted_bitcoin_private_key: "encrypted_key_123".to_string(),
            network: "testnet".to_string(),
        };

        store
            .register_user("test_pubkey".to_string(), register_payload)
            .await
            .unwrap();

        assert!(store.user_exists("test_pubkey").await.unwrap());

        store.delete_user("test_pubkey").await.unwrap();

        assert!(!store.user_exists("test_pubkey").await.unwrap());
        let result = store.login("test_pubkey".to_string()).await;
        assert!(matches!(result, Err(Error::NotFound(_))));
    }

    #[sqlx::test(migrations = "./migrations/users")]
    async fn test_ping(pool: SqlitePool) {
        let store = create_store(pool);

        store.ping().await.unwrap();
    }
}
