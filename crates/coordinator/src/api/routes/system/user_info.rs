use axum::{
    extract::State,
    http::StatusCode,
    response::{ErrorResponse, IntoResponse},
    Json,
};
use log::{debug, error};
use nostr_sdk::{Event, ToBech32};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    api::extractors::{AuthError, NostrAuth},
    domain::{
        self,
        users::{hash_password, verify_password},
    },
    startup::AppState,
};

fn validate_password_strength(password: &str) -> Result<(), String> {
    if password.len() < 10 {
        return Err("Password must be at least 10 characters".to_string());
    }
    if !password.chars().any(|c| c.is_ascii_lowercase()) {
        return Err("Password must contain a lowercase letter".to_string());
    }
    if !password.chars().any(|c| c.is_ascii_uppercase()) {
        return Err("Password must contain an uppercase letter".to_string());
    }
    if !password.chars().any(|c| c.is_ascii_digit()) {
        return Err("Password must contain a number".to_string());
    }
    if !password.chars().any(|c| !c.is_ascii_alphanumeric()) {
        return Err("Password must contain a special character".to_string());
    }
    Ok(())
}

pub async fn login(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ErrorResponse> {
    let pubkey = pubkey.to_bech32().expect("public bech32 format");
    debug!("login with pubkey: {}", pubkey);

    match state.users_info.login(pubkey).await {
        Ok(user_info) => Ok((StatusCode::CREATED, Json(user_info))),
        Err(domain::Error::NotFound(e)) => {
            error!("Failed to login: {}", e);
            Err(ErrorResponse::from(AuthError::InvalidLogin))
        }
        Err(e) => {
            error!("Failed to login: {}", e);
            Err(ErrorResponse::from(e))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
}

pub async fn register(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<RegisterPayload>,
) -> Result<impl IntoResponse, ErrorResponse> {
    let pubkey = pubkey.to_bech32().expect("public bech32 format");

    debug!("registering user: {}", pubkey);
    match state.users_info.register(pubkey, body).await {
        Ok(user_info) => Ok((StatusCode::CREATED, Json(user_info))),
        Err(e) => {
            error!("failed to register: {}", e);
            Err(ErrorResponse::from(e))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsernameRegisterPayload {
    pub username: String,
    pub password: String,
    pub encrypted_nsec: String,
    pub nostr_pubkey: String,
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

pub async fn register_username(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UsernameRegisterPayload>,
) -> Result<impl IntoResponse, ErrorResponse> {
    debug!("registering user with username: {}", body.username);

    if let Err(e) = validate_username(&body.username) {
        return Err(ErrorResponse::from(domain::Error::BadRequest(e)));
    }

    if let Err(e) = validate_password_strength(&body.password) {
        return Err(ErrorResponse::from(domain::Error::BadRequest(e)));
    }

    if state.users_info.username_exists(&body.username).await? {
        return Ok((
            StatusCode::CREATED,
            Json(UsernameRegisterResponse {
                nostr_pubkey: body.nostr_pubkey,
                username: body.username,
            }),
        ));
    }

    let password_hash = hash_password(&body.password).map_err(|e| {
        error!("Failed to hash password: {}", e);
        domain::Error::BadRequest("Failed to process password".to_string())
    })?;

    let user = state
        .users_info
        .register_username_user(
            body.nostr_pubkey.clone(),
            body.username.clone(),
            password_hash,
            body.encrypted_nsec,
            body.encrypted_bitcoin_private_key,
            body.network,
        )
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(UsernameRegisterResponse {
            nostr_pubkey: user.nostr_pubkey,
            username: user.username.unwrap_or_default(),
        }),
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsernameLoginPayload {
    pub username: String,
    pub password: String,
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
) -> Result<impl IntoResponse, ErrorResponse> {
    debug!("username login attempt for: {}", body.username);

    let user_result = state.users_info.get_user_by_username(&body.username).await;

    // Always verify against something to prevent timing attacks
    let (password_hash, user) = match user_result {
        Ok(user) => {
            let hash = user.password_hash.clone().unwrap_or_default();
            (hash, Some(user))
        }
        Err(domain::Error::NotFound(_)) => {
            // Use a dummy hash so we still spend time on verification
            let dummy_hash = "$argon2id$v=19$m=19456,t=2,p=1$dummysalt1234567$dummyhash123456789012345678901234567890".to_string();
            (dummy_hash, None)
        }
        Err(e) => {
            error!("Failed to get user by username: {}", e);
            return Err(ErrorResponse::from(e));
        }
    };

    let valid = verify_password(&body.password, &password_hash).unwrap_or(false);

    let user = match user {
        Some(u) if valid && u.password_hash.is_some() => u,
        _ => return Err(ErrorResponse::from(AuthError::InvalidLogin)),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasswordChangePayload {
    pub current_password: String,
    pub new_password: String,
    pub new_encrypted_nsec: String,
}

pub async fn change_password(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<PasswordChangePayload>,
) -> Result<impl IntoResponse, ErrorResponse> {
    let pubkey_str = pubkey.to_bech32().expect("public bech32 format");
    debug!("password change for user: {}", pubkey_str);

    if let Err(e) = validate_password_strength(&body.new_password) {
        return Err(ErrorResponse::from(domain::Error::BadRequest(e)));
    }

    let user = state.users_info.login(pubkey_str.clone()).await?;

    let password_hash = user.password_hash.as_ref().ok_or_else(|| {
        domain::Error::BadRequest("User does not have password authentication".to_string())
    })?;

    let valid = verify_password(&body.current_password, password_hash).map_err(|e| {
        error!("Password verification error: {}", e);
        domain::Error::BadRequest("Invalid current password".to_string())
    })?;

    if !valid {
        return Err(ErrorResponse::from(domain::Error::BadRequest(
            "Invalid current password".to_string(),
        )));
    }

    let new_password_hash = hash_password(&body.new_password).map_err(|e| {
        error!("Failed to hash password: {}", e);
        domain::Error::BadRequest("Failed to process password".to_string())
    })?;

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
) -> Result<impl IntoResponse, ErrorResponse> {
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
            return Err(ErrorResponse::from(e));
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgotPasswordReset {
    pub username: String,
    pub challenge: String,
    pub signed_event: String,
    pub new_password: String,
    pub new_encrypted_nsec: String,
}

pub async fn forgot_password_reset(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ForgotPasswordReset>,
) -> Result<impl IntoResponse, ErrorResponse> {
    debug!("forgot password reset for: {}", body.username);

    if let Err(e) = validate_password_strength(&body.new_password) {
        return Err(ErrorResponse::from(domain::Error::BadRequest(e)));
    }

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
        return Err(ErrorResponse::from(domain::Error::BadRequest(
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
            return Err(ErrorResponse::from(domain::Error::BadRequest(
                "Invalid or expired challenge".to_string(),
            )));
        }
        Err(e) => return Err(ErrorResponse::from(e)),
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
    if event_pubkey != nostr_pubkey {
        return Err(ErrorResponse::from(domain::Error::BadRequest(
            "Event pubkey does not match account".to_string(),
        )));
    }

    if event.content != body.challenge {
        return Err(ErrorResponse::from(domain::Error::BadRequest(
            "Challenge mismatch in signed event".to_string(),
        )));
    }

    let new_password_hash = hash_password(&body.new_password).map_err(|e| {
        error!("Failed to hash password: {}", e);
        domain::Error::BadRequest("Failed to process password".to_string())
    })?;

    state
        .users_info
        .update_password(&nostr_pubkey, new_password_hash, body.new_encrypted_nsec)
        .await?;

    {
        let mut challenges = state.forgot_password_challenges.write().await;
        challenges.remove(&body.username);
    }

    Ok(StatusCode::OK)
}
