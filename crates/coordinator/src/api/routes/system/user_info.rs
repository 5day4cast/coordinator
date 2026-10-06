use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use blake2::{
    digest::{consts::U32, KeyInit, Mac},
    Blake2bMac,
};
use log::{debug, error, info};
use nostr::{Event, ToBech32};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};

use crate::{
    api::{
        extractors::{AuthError, AuthedJson, NostrAuth},
        routes::ApiError,
    },
    domain::{
        self,
        users::{hash_auth_key, verify_auth_key, AuthKey, NewUsernameUser, PasswordError},
    },
    infra::lnurl::{LightningAddress, LnurlError, LnurlPay},
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

// Bound both active Argon2 work and admission. The permit lives in the blocking
// closure, so cancelling its HTTP request cannot admit replacement work early.
static CREDENTIAL_WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
const MAX_RESET_CHALLENGES: usize = 4096;
const RESET_CHALLENGE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Reject overload before creating or queueing an expensive blocking task.
async fn hash_auth_key_blocking(key: AuthKey) -> Result<String, ApiError> {
    let permit = CREDENTIAL_WORK
        .try_acquire()
        .map_err(|_| ApiError::Status(StatusCode::TOO_MANY_REQUESTS))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash_auth_key(&key)
    })
    .await
    .map_err(credential_task_error)?
    .map_err(|e| ApiError::from(credential_error(e)))
}

async fn verify_auth_key_blocking(
    key: AuthKey,
    stored_hash: Option<String>,
) -> Result<bool, ApiError> {
    let permit = CREDENTIAL_WORK
        .try_acquire()
        .map_err(|_| ApiError::Status(StatusCode::TOO_MANY_REQUESTS))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        verify_auth_key(&key, stored_hash.as_deref())
    })
    .await
    .map_err(credential_task_error)?
    .map_err(|e| ApiError::from(credential_error(e)))
}

pub async fn login(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let pubkey = pubkey.to_bech32().unwrap_or_else(|never| match never {});
    debug!("login with pubkey: {}", pubkey);

    match state.users_info.login(pubkey).await {
        Ok(user_info) => Ok((StatusCode::CREATED, Json(user_info))),
        Err(domain::Error::NotFound(e)) => {
            // A key with no account yet: the client offers registration.
            info!("Login for an unknown account: {}", e);
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
    /// LUD-16 address winnings are paid to.
    pub lightning_address: String,
}

/// How long saving a Lightning Address waits for its provider, resolving it and making an
/// invoice together, so signing up stays quick.
const ADDRESS_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Parse a Lightning Address and check it can be paid before it is stored, so a typo, a dead
/// provider or an address on another network fails at signup instead of at payout time.
async fn checked_lightning_address(
    state: &AppState,
    raw: &str,
) -> Result<LightningAddress, ApiError> {
    check_lightning_address(state.lnurl.as_ref(), &state.network, raw)
        .await
        .map_err(|message| ApiError::from(domain::Error::BadRequest(message)))
}

/// Resolve `raw` and ask its provider for an invoice of the smallest amount it takes, which must
/// decode, match that amount and be for `network`, the network payouts are made on (the check
/// payouts make; see `PayRequest::verify_invoice`). The invoice is never paid. Every failure is
/// one sentence for the player.
pub(crate) async fn check_lightning_address(
    lnurl: &dyn LnurlPay,
    network: &str,
    raw: &str,
) -> Result<LightningAddress, String> {
    let address = LightningAddress::parse(raw).map_err(|e| e.to_string())?;
    let domain = address.domain();
    let checked = tokio::time::timeout(ADDRESS_CHECK_TIMEOUT, async {
        let request = lnurl
            .resolve(&address)
            .await
            .map_err(|error| unresolved(&address, error))?;
        lnurl
            .request_invoice(&request, request.min_sendable_msat())
            .await
            .map_err(|error| no_invoice(&address, network, error))
    })
    .await;
    match checked {
        Ok(Ok(_invoice)) => Ok(address),
        Ok(Err(message)) => Err(message),
        Err(_) => Err(format!(
            "{domain} took too long to answer for {address} — check the address, or try again shortly."
        )),
    }
}

/// Why `address` did not resolve to an LNURL-pay endpoint.
fn unresolved(address: &LightningAddress, error: LnurlError) -> String {
    let domain = address.domain();
    match error {
        LnurlError::Status(404) => format!("{domain} has no Lightning Address {address}"),
        LnurlError::NotPublic(_)
        | LnurlError::Request(_)
        | LnurlError::Timeout
        | LnurlError::Status(_) => format!(
            "Could not reach {domain} to check {address} — check the address, or try again shortly."
        ),
        LnurlError::Provider(reason) => {
            format!("{domain} says {address} cannot receive payments: {reason}")
        }
        error => format!("{address} is not a Lightning Address this site can pay: {error}."),
    }
}

/// Why `address`'s provider gave no invoice payouts on `network` could pay.
fn no_invoice(address: &LightningAddress, network: &str, error: LnurlError) -> String {
    let domain = address.domain();
    match error {
        LnurlError::InvoiceMismatch("wrong network") => {
            let network = match network {
                "bitcoin" => "Bitcoin mainnet",
                other => other,
            };
            format!(
                "{address} receives on a different Bitcoin network. This site pays out on {network}: use a Lightning Address for {network}."
            )
        }
        LnurlError::NotPublic(_)
        | LnurlError::Request(_)
        | LnurlError::Timeout
        | LnurlError::Status(_) => format!(
            "{domain} did not return an invoice for {address} — try again shortly, or use another address."
        ),
        LnurlError::Provider(reason) => {
            format!("{domain} would not make an invoice for {address}: {reason}")
        }
        error => format!("{domain} did not return a usable invoice for {address}: {error}."),
    }
}

pub async fn register(
    State(state): State<Arc<AppState>>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<RegisterPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let pubkey = pubkey.to_bech32().unwrap_or_else(|never| match never {});

    debug!("registering user: {}", pubkey);
    let address = checked_lightning_address(&state, &body.lightning_address).await?;
    let body = RegisterPayload {
        lightning_address: address.to_string(),
        ..body
    };
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
    pub lightning_address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsernameRegisterResponse {
    pub nostr_pubkey: String,
    pub username: String,
    pub lightning_address: String,
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
    let nostr_pubkey = pubkey.to_bech32().unwrap_or_else(|never| match never {});
    debug!("registering user with username: {}", body.username);

    if let Err(e) = validate_username(&body.username) {
        return Err(ApiError::from(domain::Error::BadRequest(e)));
    }
    let lightning_address = checked_lightning_address(&state, &body.lightning_address)
        .await?
        .to_string();

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
                    lightning_address,
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
        .register_username_user(NewUsernameUser {
            nostr_pubkey,
            username: body.username.clone(),
            password_hash,
            encrypted_nsec: body.encrypted_nsec,
            encrypted_bitcoin_private_key: body.encrypted_bitcoin_private_key,
            network: body.network,
            lightning_address,
        })
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
            lightning_address: user.lightning_address.unwrap_or_default(),
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
    pub lightning_address: Option<String>,
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
            lightning_address: user.lightning_address,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LightningAddressPayload {
    pub lightning_address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightningAddressResponse {
    pub lightning_address: String,
}

/// Change where winnings are paid. Signed with the account's Nostr key.
pub async fn set_lightning_address(
    State(state): State<Arc<AppState>>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<LightningAddressPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let nostr_pubkey = match pubkey.to_bech32() {
        Ok(encoded) => encoded,
        Err(never) => match never {},
    };
    let address = checked_lightning_address(&state, &body.lightning_address).await?;
    state
        .users_info
        .update_lightning_address(&nostr_pubkey, address.to_string())
        .await?;
    Ok((
        StatusCode::OK,
        Json(LightningAddressResponse {
            lightning_address: address.to_string(),
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
    let pubkey_str = pubkey.to_bech32().unwrap_or_else(|never| match never {});
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
}

// Issuance is stateless: anonymous callers cannot fill or evict recovery slots.
// Restarting a process invalidates its outstanding five-minute challenges.
static RESET_CHALLENGE_KEY: LazyLock<[u8; 32]> = LazyLock::new(|| {
    use rand::Rng;
    let mut key = [0u8; 32];
    rand::rng().fill(&mut key);
    key
});
type ResetMac = Blake2bMac<U32>;

fn reset_challenge_mac(username: &str, issued: &str, nonce: &str) -> Option<ResetMac> {
    let mut mac = <ResetMac as KeyInit>::new_from_slice(&*RESET_CHALLENGE_KEY).ok()?;
    mac.update(b"coordinator/password-reset/v1\0");
    mac.update(&(username.len() as u64).to_be_bytes());
    mac.update(username.as_bytes());
    mac.update(issued.as_bytes());
    mac.update(b":");
    mac.update(nonce.as_bytes());
    Some(mac)
}

fn reset_now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|t| t.as_secs())
}

fn issue_reset_challenge(username: &str, now: u64) -> Option<String> {
    use rand::Rng;
    let mut nonce = [0u8; 32];
    rand::rng().fill(&mut nonce);
    let nonce = hex::encode(nonce);
    let issued = now.to_string();
    let tag = reset_challenge_mac(username, &issued, &nonce)?
        .finalize()
        .into_bytes();
    Some(format!("{issued}:{nonce}:{}", hex::encode(tag)))
}

fn valid_reset_challenge(username: &str, challenge: &str, now: u64) -> bool {
    let mut parts = challenge.split(':');
    let (Some(issued), Some(nonce), Some(tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Ok(timestamp) = issued.parse::<u64>() else {
        return false;
    };
    if now < timestamp || now - timestamp >= RESET_CHALLENGE_TTL.as_secs() || nonce.len() != 64 {
        return false;
    }
    let Ok(tag) = hex::decode(tag) else {
        return false;
    };
    reset_challenge_mac(username, issued, nonce).is_some_and(|mac| mac.verify_slice(&tag).is_ok())
}

pub async fn forgot_password_challenge(
    Json(body): Json<ForgotPasswordRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if body.username.is_empty() || body.username.len() > 256 {
        return Err(ApiError::Status(StatusCode::BAD_REQUEST));
    }
    // The response has the same shape for existing and unknown accounts.
    let challenge = reset_now()
        .and_then(|now| issue_reset_challenge(&body.username, now))
        .ok_or(ApiError::Status(StatusCode::SERVICE_UNAVAILABLE))?;
    Ok((StatusCode::OK, Json(ForgotPasswordChallenge { challenge })))
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

    let challenge_valid =
        reset_now().is_some_and(|now| valid_reset_challenge(&body.username, &body.challenge, now));

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

    let event_pubkey = event
        .pubkey
        .to_bech32()
        .unwrap_or_else(|never| match never {});
    if event_pubkey != nostr_pubkey
        || pubkey.to_bech32().unwrap_or_else(|never| match never {}) != nostr_pubkey
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

    // Admission failure must leave the valid challenge usable.
    let new_password_hash = hash_auth_key_blocking(body.new_auth_key).await?;

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
    if !reset_now().is_some_and(|now| valid_reset_challenge(username, challenge, now)) {
        return false;
    }
    challenges.retain(|_, (_, consumed)| consumed.elapsed() < RESET_CHALLENGE_TTL);
    if challenges.contains_key(challenge) || challenges.len() >= MAX_RESET_CHALLENGES {
        return false;
    }
    // Only authenticated account owners can consume storage, after both proofs verify.
    challenges.insert(
        challenge.to_owned(),
        (username.to_owned(), std::time::Instant::now()),
    );
    true
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    #[tokio::test]
    async fn concurrent_password_resets_can_claim_a_challenge_only_once() {
        let challenges = RwLock::new(HashMap::new());
        let challenge = issue_reset_challenge("alice", reset_now().unwrap()).unwrap();
        let (first, second) = tokio::join!(
            claim_reset_challenge(&challenges, "alice", &challenge),
            claim_reset_challenge(&challenges, "alice", &challenge),
        );
        assert_ne!(first, second);
        assert!(!claim_reset_challenge(&challenges, "alice", &challenge).await);
    }

    #[tokio::test]
    async fn issuance_cannot_replace_a_live_challenge_and_proofs_bind_owner_and_expiry() {
        let challenges = RwLock::new(HashMap::new());
        let now = reset_now().unwrap();
        let first = issue_reset_challenge("alice", now).unwrap();
        let second = issue_reset_challenge("alice", now).unwrap();
        let unknown = issue_reset_challenge("missing", now).unwrap();
        assert_eq!(first.len(), unknown.len());
        assert_ne!(first, second);
        assert!(!valid_reset_challenge("bob", &first, now));
        assert!(!valid_reset_challenge("alice", &first, now + 300));
        assert!(!valid_reset_challenge("alice", &first, now - 1));
        assert!(!valid_reset_challenge("alice", &format!("{first}0"), now));
        assert!(!claim_reset_challenge(&challenges, "bob", &first).await);
        assert!(claim_reset_challenge(&challenges, "alice", &first).await);
        assert!(claim_reset_challenge(&challenges, "alice", &second).await);
    }
}

#[cfg(test)]
mod address_check_tests {
    use super::*;
    use crate::infra::{lnurl::PayRequest, lnurl_mock::MockLnurlPay};
    use async_trait::async_trait;
    use bitcoin::Network;
    use lightning_invoice::Bolt11Invoice;

    #[tokio::test]
    async fn an_address_is_kept_only_when_its_provider_makes_an_invoice_for_this_network() {
        let mock = MockLnurlPay::new(Network::Signet);
        let address = check_lightning_address(&mock, "signet", " Player@Mock-Wallet.dev ")
            .await
            .unwrap();
        assert_eq!(address.to_string(), "player@mock-wallet.dev");

        for (raw, error) in [
            ("not an address", "expected user@domain"),
            (
                "unreachable@mock-wallet.dev",
                "Could not reach mock-wallet.dev to check unreachable@mock-wallet.dev",
            ),
            (
                "unknown@mock-wallet.dev",
                "mock-wallet.dev has no Lightning Address unknown@mock-wallet.dev",
            ),
            (
                "no-invoice@mock-wallet.dev",
                "mock-wallet.dev would not make an invoice for no-invoice@mock-wallet.dev",
            ),
            (
                "wrong-network@mock-wallet.dev",
                "wrong-network@mock-wallet.dev receives on a different Bitcoin network. This site pays out on signet",
            ),
        ] {
            let refused = check_lightning_address(&mock, "signet", raw)
                .await
                .unwrap_err();
            assert!(refused.contains(error), "{raw}: {refused}");
        }

        // A signet address on a mainnet coordinator is refused the same way.
        let mainnet = MockLnurlPay::new(Network::Bitcoin);
        let refused = check_lightning_address(&mainnet, "bitcoin", "wrong-network@mock-wallet.dev")
            .await
            .unwrap_err();
        assert!(
            refused.contains("This site pays out on Bitcoin mainnet"),
            "{refused}"
        );
    }

    /// A provider that never answers.
    struct Silent;

    #[async_trait]
    impl LnurlPay for Silent {
        async fn resolve(&self, _: &LightningAddress) -> Result<PayRequest, LnurlError> {
            std::future::pending().await
        }

        async fn request_invoice(
            &self,
            _: &PayRequest,
            _: u64,
        ) -> Result<Bolt11Invoice, LnurlError> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_provider_fails_the_check_within_its_timeout() {
        let refused = check_lightning_address(&Silent, "signet", "slow@mock-wallet.dev")
            .await
            .unwrap_err();
        assert!(
            refused.contains("mock-wallet.dev took too long to answer for slow@mock-wallet.dev"),
            "{refused}"
        );
    }
}
