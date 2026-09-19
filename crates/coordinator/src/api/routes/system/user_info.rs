use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use log::{debug, error};
use nostr_sdk::{Event, ToBech32};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    api::{
        extractors::{AuthError, AuthedJson, NostrAuth},
        routes::ApiError,
    },
    domain::{
        self,
        users::{hash_auth_key, verify_auth_key, AuthKey, PasswordError},
    },
    startup::AppState,
};

/// Map a credential-hashing failure to a safe client error, logging the cause.
fn credential_error(e: PasswordError) -> domain::Error {
    error!("Credential processing failed: {}", e);
    domain::Error::BadRequest("Failed to process credentials".to_string())
}

fn credential_task_error(e: tokio::task::JoinError) -> ApiError {
    error!("Credential task failed: {}", e);
    ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR)
}

/// Argon2 takes tens of milliseconds; keep it off the async workers so a burst
/// of login attempts cannot stall every other request.
async fn hash_auth_key_blocking(key: AuthKey) -> Result<String, ApiError> {
    tokio::task::spawn_blocking(move || hash_auth_key(&key))
        .await
        .map_err(credential_task_error)?
        .map_err(|e| ApiError::from(credential_error(e)))
}

async fn verify_auth_key_blocking(
    key: AuthKey,
    stored_hash: Option<String>,
) -> Result<bool, ApiError> {
    tokio::task::spawn_blocking(move || verify_auth_key(&key, stored_hash.as_deref()))
        .await
        .map_err(credential_task_error)?
        .map_err(|e| ApiError::from(credential_error(e)))
}

pub async fn login(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let pubkey = pubkey.to_bech32().expect("public bech32 format");
    debug!("login with pubkey: {}", pubkey);

    match state.users_info.login(pubkey).await {
        Ok(user_info) => Ok((StatusCode::CREATED, Json(user_info))),
        Err(domain::Error::NotFound(e)) => {
            error!("Failed to login: {}", e);
            Err(ApiError::from(AuthError::InvalidLogin))
        }
        Err(e) => {
            error!("Failed to login: {}", e);
            Err(ApiError::from(e))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
}

pub async fn register(
    State(state): State<Arc<AppState>>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<RegisterPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let pubkey = pubkey.to_bech32().expect("public bech32 format");

    debug!("registering user: {}", pubkey);
    match state.users_info.register(pubkey, body).await {
        Ok(user_info) => Ok((StatusCode::CREATED, Json(user_info))),
        Err(e) => {
            error!("failed to register: {}", e);
            Err(ApiError::from(e))
        }
    }
}

// No `Debug`/`Clone`: the payloads carry login credentials.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsernameRegisterPayload {
    pub username: String,
    pub auth_key: AuthKey,
    pub encrypted_nsec: String,
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsernameRegisterResponse {
    pub nostr_pubkey: String,
    pub username: String,
}

fn validate_username(username: &str) -> Result<(), String> {
    if username.len() < 3 {
        return Err("Username must be at least 3 characters".to_string());
    }
    if username.len() > 32 {
        return Err("Username must be at most 32 characters".to_string());
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(
            "Username can only contain letters, numbers, underscores, and hyphens".to_string(),
        );
    }
    if !username
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        return Err("Username must start with a letter".to_string());
    }
    Ok(())
}

/// Signed with the account's Nostr key, so a username can only ever be bound
/// to a pubkey whose owner asked for it.
pub async fn register_username(
    State(state): State<Arc<AppState>>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<UsernameRegisterPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let nostr_pubkey = pubkey.to_bech32().expect("public bech32 format");
    debug!("registering user with username: {}", body.username);

    if let Err(e) = validate_username(&body.username) {
        return Err(ApiError::from(domain::Error::BadRequest(e)));
    }

    match state
        .users_info
        .get_pubkey_by_username(&body.username)
        .await
    {
        // A retry by the same account is idempotent.
        Ok(owner) if owner == nostr_pubkey => {
            return Ok((
                StatusCode::CREATED,
                Json(UsernameRegisterResponse {
                    nostr_pubkey,
                    username: body.username,
                }),
            ));
        }
        Ok(_) => {
            return Err(ApiError::from(domain::Error::BadRequest(
                "Username is already taken".to_string(),
            )));
        }
        Err(domain::Error::NotFound(_)) => {}
        Err(e) => return Err(ApiError::from(e)),
    }

    let password_hash = hash_auth_key_blocking(body.auth_key).await?;

    let user = match state
        .users_info
        .register_username_user(
            nostr_pubkey,
            body.username.clone(),
            password_hash,
            body.encrypted_nsec,
            body.encrypted_bitcoin_private_key,
            body.network,
        )
        .await
    {
        Ok(user) => user,
        Err(e) => {
            error!("failed to register username user {}: {}", body.username, e);
            return Err(ApiError::from(e));
        }
    };

    Ok((
        StatusCode::CREATED,
        Json(UsernameRegisterResponse {
            nostr_pubkey: user.nostr_pubkey,
            username: user.username.unwrap_or_default(),
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsernameLoginPayload {
    pub username: String,
    pub auth_key: AuthKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsernameLoginResponse {
    pub encrypted_nsec: String,
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
    pub nostr_pubkey: String,
}

pub async fn login_username(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UsernameLoginPayload>,
) -> Result<impl IntoResponse, ApiError> {
    debug!("username login attempt for: {}", body.username);

    let user_result = state.users_info.get_user_by_username(&body.username).await;

    let user = match user_result {
        Ok(user) => Some(user),
        Err(domain::Error::NotFound(_)) => None,
        Err(e) => {
            error!("Failed to get user by username: {}", e);
            return Err(ApiError::from(e));
        }
    };

    // Unknown users are checked against a dummy hash so timing does not
    // reveal whether the username exists.
    let stored_hash = user.as_ref().and_then(|u| u.password_hash.clone());
    let valid = verify_auth_key_blocking(body.auth_key, stored_hash).await?;

    let user = match user {
        Some(u) if valid => u,
        _ => return Err(ApiError::from(AuthError::InvalidLogin)),
    };

    let encrypted_nsec = user.encrypted_nsec.ok_or_else(|| {
        error!("User {} has no encrypted nsec", body.username);
        AuthError::InvalidLogin
    })?;

    Ok((
        StatusCode::OK,
        Json(UsernameLoginResponse {
            encrypted_nsec,
            encrypted_bitcoin_private_key: user.encrypted_bitcoin_private_key,
            network: user.network,
            nostr_pubkey: user.nostr_pubkey,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordChangePayload {
    pub current_auth_key: AuthKey,
    pub new_auth_key: AuthKey,
    pub new_encrypted_nsec: String,
}

pub async fn change_password(
    State(state): State<Arc<AppState>>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<PasswordChangePayload>,
) -> Result<impl IntoResponse, ApiError> {
    let pubkey_str = pubkey.to_bech32().expect("public bech32 format");
    debug!("password change for user: {}", pubkey_str);

    let user = state.users_info.login(pubkey_str.clone()).await?;

    let password_hash = user.password_hash.as_ref().ok_or_else(|| {
        domain::Error::BadRequest("User does not have password authentication".to_string())
    })?;

    if !verify_auth_key_blocking(body.current_auth_key, Some(password_hash.clone())).await? {
        return Err(ApiError::from(domain::Error::BadRequest(
            "Invalid current password".to_string(),
        )));
    }

    let new_password_hash = hash_auth_key_blocking(body.new_auth_key).await?;

    state
        .users_info
        .update_password(&pubkey_str, new_password_hash, body.new_encrypted_nsec)
        .await?;

    Ok(StatusCode::OK)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgotPasswordRequest {
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgotPasswordChallenge {
    pub challenge: String,
    pub nostr_pubkey: String,
}

pub async fn forgot_password_challenge(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ForgotPasswordRequest>,
) -> Result<impl IntoResponse, ApiError> {
    debug!("forgot password request for: {}", body.username);

    let challenge = {
        use rand::Rng;
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        hex::encode(bytes)
    };

    let nostr_pubkey = match state
        .users_info
        .get_pubkey_by_username(&body.username)
        .await
    {
        Ok(pubkey) => {
            let mut challenges = state.forgot_password_challenges.write().await;
            challenges.insert(
                body.username.clone(),
                (challenge.clone(), std::time::Instant::now()),
            );
            pubkey
        }
        Err(domain::Error::NotFound(_)) => {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            body.username.hash(&mut hasher);
            format!(
                "npub1fake{:016x}0000000000000000000000000000",
                hasher.finish()
            )
        }
        Err(e) => {
            error!("Failed to get pubkey by username: {}", e);
            return Err(ApiError::from(e));
        }
    };

    Ok((
        StatusCode::OK,
        Json(ForgotPasswordChallenge {
            challenge,
            nostr_pubkey,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgotPasswordReset {
    pub username: String,
    pub challenge: String,
    pub signed_event: String,
    pub new_auth_key: AuthKey,
    pub new_encrypted_nsec: String,
}

pub async fn forgot_password_reset(
    State(state): State<Arc<AppState>>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<ForgotPasswordReset>,
) -> Result<impl IntoResponse, ApiError> {
    debug!("forgot password reset for: {}", body.username);

    let challenge_valid = {
        let challenges = state.forgot_password_challenges.read().await;
        if let Some((stored_challenge, created_at)) = challenges.get(&body.username) {
            stored_challenge == &body.challenge
                && created_at.elapsed() < std::time::Duration::from_secs(300)
        } else {
            false
        }
    };

    if !challenge_valid {
        return Err(ApiError::from(domain::Error::BadRequest(
            "Invalid or expired challenge".to_string(),
        )));
    }

    let nostr_pubkey = match state
        .users_info
        .get_pubkey_by_username(&body.username)
        .await
    {
        Ok(pubkey) => pubkey,
        Err(domain::Error::NotFound(_)) => {
            return Err(ApiError::from(domain::Error::BadRequest(
                "Invalid or expired challenge".to_string(),
            )));
        }
        Err(e) => return Err(ApiError::from(e)),
    };

    let event: Event = serde_json::from_str(&body.signed_event).map_err(|e| {
        error!("Failed to parse signed event: {}", e);
        domain::Error::BadRequest("Invalid signed event format".to_string())
    })?;

    event.verify().map_err(|e| {
        error!("Invalid event signature: {}", e);
        domain::Error::BadRequest("Invalid event signature".to_string())
    })?;

    let event_pubkey = event.pubkey.to_bech32().expect("public bech32 format");
    if event_pubkey != nostr_pubkey
        || pubkey.to_bech32().expect("public bech32 format") != nostr_pubkey
    {
        return Err(ApiError::from(domain::Error::BadRequest(
            "Event pubkey does not match account".to_string(),
        )));
    }

    if event.content != body.challenge {
        return Err(ApiError::from(domain::Error::BadRequest(
            "Challenge mismatch in signed event".to_string(),
        )));
    }

    // Verify the account proof and body-bound authorization before consuming the
    // challenge. Claim it atomically so concurrent requests cannot reset twice.
    if !claim_reset_challenge(
        &state.forgot_password_challenges,
        &body.username,
        &body.challenge,
    )
    .await
    {
        return Err(ApiError::from(domain::Error::BadRequest(
            "Invalid or expired challenge".to_string(),
        )));
    }

    let new_password_hash = hash_auth_key_blocking(body.new_auth_key).await?;

    state
        .users_info
        .update_password(&nostr_pubkey, new_password_hash, body.new_encrypted_nsec)
        .await?;

    Ok(StatusCode::OK)
}

async fn claim_reset_challenge(
    challenges: &tokio::sync::RwLock<
        std::collections::HashMap<String, (String, std::time::Instant)>,
    >,
    username: &str,
    challenge: &str,
) -> bool {
    let mut challenges = challenges.write().await;
    let valid = challenges.get(username).is_some_and(|(stored, created)| {
        stored == challenge && created.elapsed() < std::time::Duration::from_secs(300)
    });
    if valid {
        challenges.remove(username);
    }
    valid
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    use std::{
        collections::HashMap,
        time::{Duration, Instant},
    };
    use tokio::sync::RwLock;

    #[tokio::test]
    async fn concurrent_password_resets_can_claim_a_challenge_only_once() {
        let challenges = RwLock::new(HashMap::from([(
            "alice".into(),
            ("challenge".into(), Instant::now()),
        )]));
        let (first, second) = tokio::join!(
            claim_reset_challenge(&challenges, "alice", "challenge"),
            claim_reset_challenge(&challenges, "alice", "challenge"),
        );
        assert_ne!(first, second);
        assert!(!claim_reset_challenge(&challenges, "alice", "challenge").await);
    }

    #[tokio::test]
    async fn invalid_challenges_cannot_consume_a_live_challenge() {
        let challenges = RwLock::new(HashMap::from([
            ("alice".into(), ("challenge".into(), Instant::now())),
            (
                "expired".into(),
                ("old".into(), Instant::now() - Duration::from_secs(301)),
            ),
        ]));
        assert!(!claim_reset_challenge(&challenges, "alice", "wrong").await);
        assert!(!claim_reset_challenge(&challenges, "missing", "challenge").await);
        assert!(!claim_reset_challenge(&challenges, "expired", "old").await);
        assert!(claim_reset_challenge(&challenges, "alice", "challenge").await);
    }
}
