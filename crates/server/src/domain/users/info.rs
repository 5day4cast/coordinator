use std::sync::Arc;

use super::{User, UserStore};
use crate::{domain::Error, RegisterPayload};

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
}
