use std::sync::Arc;

use super::{NewUsernameUser, User, UserStore};
use crate::{api::routes::RegisterPayload, domain::Error};

pub struct UserInfo {
    user_store: Arc<UserStore>,
}

impl UserInfo {
    pub(crate) fn auth_database(&self) -> crate::infra::db::DBConnection {
        self.user_store.auth_database()
    }

    pub fn new(user_store: UserStore) -> Self {
        Self {
            user_store: Arc::new(user_store),
        }
    }

    pub async fn register(&self, pubkey: String, payload: RegisterPayload) -> Result<User, Error> {
        self.user_store.register_user(pubkey, payload).await
    }

    pub async fn login(&self, pubkey: String) -> Result<User, Error> {
        self.user_store.login(pubkey).await
    }

    /// Accounts created at or after `since`.
    pub async fn count_signups_since(&self, since: time::OffsetDateTime) -> Result<u64, Error> {
        self.user_store.count_signups_since(since).await
    }

    pub async fn register_username_user(&self, user: NewUsernameUser) -> Result<User, Error> {
        self.user_store.register_username_user(user).await
    }

    pub async fn update_lightning_address(
        &self,
        nostr_pubkey: &str,
        lightning_address: String,
    ) -> Result<(), Error> {
        self.user_store
            .update_lightning_address(nostr_pubkey, lightning_address)
            .await
    }

    pub async fn get_username_by_pubkey(
        &self,
        nostr_pubkey: &str,
    ) -> Result<Option<String>, Error> {
        self.user_store.get_username_by_pubkey(nostr_pubkey).await
    }

    pub async fn get_display_name_by_pubkey(
        &self,
        nostr_pubkey: &str,
    ) -> Result<Option<String>, Error> {
        self.user_store
            .get_display_name_by_pubkey(nostr_pubkey)
            .await
    }

    /// Sets the player's leaderboard name, or clears it with `None`. Another account's name,
    /// in any case, is refused.
    pub async fn set_display_name(
        &self,
        nostr_pubkey: &str,
        display_name: Option<String>,
    ) -> Result<(), Error> {
        if let Some(name) = &display_name {
            if self
                .user_store
                .display_name_taken(name, nostr_pubkey)
                .await?
            {
                return Err(Error::BadRequest(
                    "Another player already uses that name".to_string(),
                ));
            }
        }
        self.user_store
            .update_display_name(nostr_pubkey, display_name)
            .await
    }

    pub async fn get_user_by_username(&self, username: &str) -> Result<User, Error> {
        self.user_store.get_user_by_username(username).await
    }

    pub async fn username_exists(&self, username: &str) -> Result<bool, Error> {
        self.user_store.username_exists(username).await
    }

    pub async fn update_password(
        &self,
        nostr_pubkey: &str,
        new_password_hash: String,
        new_encrypted_nsec: String,
    ) -> Result<(), Error> {
        self.user_store
            .update_password(nostr_pubkey, new_password_hash, new_encrypted_nsec)
            .await
    }

    pub async fn get_pubkey_by_username(&self, username: &str) -> Result<String, Error> {
        self.user_store.get_pubkey_by_username(username).await
    }

    pub async fn update_username(&self, nostr_pubkey: &str, username: String) -> Result<(), Error> {
        self.user_store
            .update_username(nostr_pubkey, username)
            .await
    }
}
