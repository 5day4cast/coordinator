use std::sync::Arc;

use super::{User, UserStore};
use crate::{api::routes::RegisterPayload, domain::Error};

pub struct UserInfo {
    user_store: Arc<UserStore>,
}

impl UserInfo {
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

    // ==================== Email Auth Methods ====================

    pub async fn register_email_user(
        &self,
        nostr_pubkey: String,
        email: String,
        password_hash: String,
        encrypted_nsec: String,
        encrypted_bitcoin_private_key: String,
        network: String,
    ) -> Result<User, Error> {
        self.user_store
            .register_email_user(
                nostr_pubkey,
                email,
                password_hash,
                encrypted_nsec,
                encrypted_bitcoin_private_key,
                network,
            )
            .await
    }

    pub async fn get_user_by_email(&self, email: &str) -> Result<User, Error> {
        self.user_store.get_user_by_email(email).await
    }

    pub async fn email_exists(&self, email: &str) -> Result<bool, Error> {
        self.user_store.email_exists(email).await
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

    pub async fn get_pubkey_by_email(&self, email: &str) -> Result<String, Error> {
        self.user_store.get_pubkey_by_email(email).await
    }
}
