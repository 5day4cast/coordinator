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

/// Validates password strength requirements:
/// - At least 10 characters
/// - Contains lowercase letter
/// - Contains uppercase letter
/// - Contains number
/// - Contains special character
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

// ==================== Email Auth Endpoints ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailRegisterPayload {
    pub email: String,
    pub password: String,
    pub encrypted_nsec: String,
    pub nostr_pubkey: String,
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailRegisterResponse {
    pub nostr_pubkey: String,
    pub email: String,
}

pub async fn register_email(
    State(state): State<Arc<AppState>>,
    Json(body): Json<EmailRegisterPayload>,
) -> Result<impl IntoResponse, ErrorResponse> {
    debug!("registering user with email: {}", body.email);

    // Validate email format (basic check)
    if !body.email.contains('@') || body.email.len() < 5 {
        return Err(ErrorResponse::from(domain::Error::BadRequest(
            "Invalid email format".to_string(),
        )));
    }

    // Validate password strength
    if let Err(e) = validate_password_strength(&body.password) {
        return Err(ErrorResponse::from(domain::Error::BadRequest(e)));
    }

    // Check if email already exists
    if state.users_info.email_exists(&body.email).await? {
        return Err(ErrorResponse::from(domain::Error::BadRequest(
            "Email already registered".to_string(),
        )));
    }

    // Hash the password
    let password_hash = hash_password(&body.password).map_err(|e| {
        error!("Failed to hash password: {}", e);
        domain::Error::BadRequest("Failed to process password".to_string())
    })?;

    // Register the user
    let user = state
        .users_info
        .register_email_user(
            body.nostr_pubkey.clone(),
            body.email.clone(),
            password_hash,
            body.encrypted_nsec,
            body.encrypted_bitcoin_private_key,
            body.network,
        )
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(EmailRegisterResponse {
            nostr_pubkey: user.nostr_pubkey,
            email: user.email.unwrap_or_default(),
        }),
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailLoginPayload {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailLoginResponse {
    pub encrypted_nsec: String,
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
    pub nostr_pubkey: String,
}

pub async fn login_email(
    State(state): State<Arc<AppState>>,
    Json(body): Json<EmailLoginPayload>,
) -> Result<impl IntoResponse, ErrorResponse> {
    debug!("email login attempt for: {}", body.email);

    // Get user by email
    let user = match state.users_info.get_user_by_email(&body.email).await {
        Ok(user) => user,
        Err(domain::Error::NotFound(_)) => {
            return Err(ErrorResponse::from(AuthError::InvalidLogin));
        }
        Err(e) => {
            error!("Failed to get user by email: {}", e);
            return Err(ErrorResponse::from(e));
        }
    };

    // Verify password
    let password_hash = user.password_hash.as_ref().ok_or_else(|| {
        error!("User {} has no password hash", body.email);
        AuthError::InvalidLogin
    })?;

    let valid = verify_password(&body.password, password_hash).map_err(|e| {
        error!("Password verification error: {}", e);
        AuthError::InvalidLogin
    })?;

    if !valid {
        return Err(ErrorResponse::from(AuthError::InvalidLogin));
    }

    // Return encrypted data for client-side decryption
    let encrypted_nsec = user.encrypted_nsec.ok_or_else(|| {
        error!("User {} has no encrypted nsec", body.email);
        AuthError::InvalidLogin
    })?;

    Ok((
        StatusCode::OK,
        Json(EmailLoginResponse {
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

    // Validate new password strength
    if let Err(e) = validate_password_strength(&body.new_password) {
        return Err(ErrorResponse::from(domain::Error::BadRequest(e)));
    }

    // Get user to verify current password
    let user = state.users_info.login(pubkey_str.clone()).await?;

    let password_hash = user.password_hash.as_ref().ok_or_else(|| {
        domain::Error::BadRequest("User does not have password authentication".to_string())
    })?;

    // Verify current password
    let valid = verify_password(&body.current_password, password_hash).map_err(|e| {
        error!("Password verification error: {}", e);
        domain::Error::BadRequest("Invalid current password".to_string())
    })?;

    if !valid {
        return Err(ErrorResponse::from(domain::Error::BadRequest(
            "Invalid current password".to_string(),
        )));
    }

    // Hash new password
    let new_password_hash = hash_password(&body.new_password).map_err(|e| {
        error!("Failed to hash password: {}", e);
        domain::Error::BadRequest("Failed to process password".to_string())
    })?;

    // Update password
    state
        .users_info
        .update_password(&pubkey_str, new_password_hash, body.new_encrypted_nsec)
        .await?;

    Ok(StatusCode::OK)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgotPasswordRequest {
    pub email: String,
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
    debug!("forgot password request for: {}", body.email);

    // Get user's pubkey by email
    let nostr_pubkey = match state.users_info.get_pubkey_by_email(&body.email).await {
        Ok(pubkey) => pubkey,
        Err(domain::Error::NotFound(_)) => {
            // Don't reveal whether email exists - return generic error
            return Err(ErrorResponse::from(domain::Error::BadRequest(
                "If this email is registered, a challenge has been generated".to_string(),
            )));
        }
        Err(e) => {
            error!("Failed to get pubkey by email: {}", e);
            return Err(ErrorResponse::from(e));
        }
    };

    // Generate random challenge
    let challenge = {
        use rand::Rng;
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        hex::encode(bytes)
    };

    // Store challenge with expiry (5 minutes)
    {
        let mut challenges = state.forgot_password_challenges.write().await;
        challenges.insert(
            body.email.clone(),
            (challenge.clone(), std::time::Instant::now()),
        );
    }

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
    pub email: String,
    pub challenge: String,
    pub signed_event: String,
    pub new_password: String,
    pub new_encrypted_nsec: String,
}

pub async fn forgot_password_reset(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ForgotPasswordReset>,
) -> Result<impl IntoResponse, ErrorResponse> {
    debug!("forgot password reset for: {}", body.email);

    // Validate new password strength
    if let Err(e) = validate_password_strength(&body.new_password) {
        return Err(ErrorResponse::from(domain::Error::BadRequest(e)));
    }

    // Verify challenge exists and hasn't expired (5 min timeout)
    let challenge_valid = {
        let challenges = state.forgot_password_challenges.read().await;
        if let Some((stored_challenge, created_at)) = challenges.get(&body.email) {
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

    // Get user's pubkey
    let nostr_pubkey = state.users_info.get_pubkey_by_email(&body.email).await?;

    // Parse and verify the signed event
    let event: Event = serde_json::from_str(&body.signed_event).map_err(|e| {
        error!("Failed to parse signed event: {}", e);
        domain::Error::BadRequest("Invalid signed event format".to_string())
    })?;

    // Verify signature
    event.verify().map_err(|e| {
        error!("Invalid event signature: {}", e);
        domain::Error::BadRequest("Invalid event signature".to_string())
    })?;

    // Verify pubkey matches
    let event_pubkey = event.pubkey.to_bech32().expect("public bech32 format");
    if event_pubkey != nostr_pubkey {
        return Err(ErrorResponse::from(domain::Error::BadRequest(
            "Event pubkey does not match account".to_string(),
        )));
    }

    // Verify challenge is in event content
    if event.content != body.challenge {
        return Err(ErrorResponse::from(domain::Error::BadRequest(
            "Challenge mismatch in signed event".to_string(),
        )));
    }

    // Hash new password
    let new_password_hash = hash_password(&body.new_password).map_err(|e| {
        error!("Failed to hash password: {}", e);
        domain::Error::BadRequest("Failed to process password".to_string())
    })?;

    // Update password
    state
        .users_info
        .update_password(&nostr_pubkey, new_password_hash, body.new_encrypted_nsec)
        .await?;

    // Remove used challenge
    {
        let mut challenges = state.forgot_password_challenges.write().await;
        challenges.remove(&body.email);
    }

    Ok(StatusCode::OK)
}
