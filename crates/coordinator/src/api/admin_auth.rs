//! Operator authentication for the admin listener.
//!
//! Trust boundary: every route on the admin listener except `/admin/login` and the
//! static UI assets requires one of
//!
//! - `Authorization: Bearer <token>` for scripts such as synth, or
//! - the session cookie issued by `/admin/login` for the browser pages.
//!
//! The token is read once from `admin_settings.token_file`. Only keys derived from it
//! stay in memory. A session cookie carries its expiry and a keyed BLAKE2b tag, so
//! sessions survive restarts and rotating the token revokes them all. Cookie-authenticated
//! requests other than GET, HEAD, and OPTIONS must echo a CSRF token derived from the
//! session in `X-CSRF-Token`: `SameSite=Strict` does not separate ports on one host, so
//! the public app on the same hostname counts as the same site. Bearer requests carry
//! no ambient credential and need no CSRF token.

use std::{fs::File, io::Read, path::Path, sync::Arc};

use axum::{
    extract::{Request, State},
    http::{
        header::{AUTHORIZATION, CACHE_CONTROL, COOKIE, SET_COOKIE, WWW_AUTHENTICATE},
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
    },
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use blake2::{
    digest::{consts::U32, generic_array::GenericArray, KeyInit, Mac},
    Blake2b, Blake2bMac, Digest,
};
use log::warn;
use maud::{html, DOCTYPE};
use serde::Deserialize;
use time::OffsetDateTime;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::config::AdminSettings;

pub const SESSION_COOKIE: &str = "coordinator_admin_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Browser sessions last one working day; the operator logs in again afterwards.
const SESSION_TTL_SECS: i64 = 12 * 60 * 60;
/// 32 characters of `openssl rand -hex 32` output leave no room for online guessing.
const MIN_TOKEN_LEN: usize = 32;
const MAX_TOKEN_FILE_BYTES: u64 = 4096;
const KEY_CONTEXT: &[u8] = b"5day4cast-coordinator-admin-v1\0";

type AdminMac = Blake2bMac<U32>;

/// The operator token file cannot back authentication.
#[derive(Debug, thiserror::Error)]
pub enum AdminTokenError {
    #[error("admin token file {path} could not be read: {source}")]
    Unreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("admin token file {path} is empty")]
    Empty { path: String },
    #[error("admin token in {path} must be at least {MIN_TOKEN_LEN} characters")]
    TooShort { path: String },
    #[error("admin token file {path} exceeds {MAX_TOKEN_FILE_BYTES} bytes")]
    TooLarge { path: String },
}

/// How the admin listener admits requests. Built once at startup.
pub enum AdminAccess {
    Token(AdminCredentials),
    /// Development only; `AdminSettings::validate` refuses it on mainnet and off loopback.
    Unauthenticated,
}

impl AdminAccess {
    pub fn from_settings(settings: &AdminSettings) -> Result<Self, AdminTokenError> {
        if settings.dangerous_allow_unauthenticated {
            warn!(
                "Operator routes on {} accept unauthenticated requests. Never use this outside local development.",
                settings.listen_addr
            );
            return Ok(Self::Unauthenticated);
        }
        AdminCredentials::load(Path::new(&settings.token_file)).map(Self::Token)
    }
}

/// Keys derived from the operator token. Deliberately neither `Clone` nor `Debug`.
pub struct AdminCredentials {
    key: Zeroizing<[u8; 32]>,
    bearer_tag: Zeroizing<[u8; 32]>,
}

impl AdminCredentials {
    pub fn load(path: &Path) -> Result<Self, AdminTokenError> {
        let display = path.display().to_string();
        let mut contents = Zeroizing::new(Vec::new());
        File::open(path)
            .and_then(|file| {
                file.take(MAX_TOKEN_FILE_BYTES + 1)
                    .read_to_end(&mut contents)
            })
            .map_err(|source| AdminTokenError::Unreadable {
                path: display.clone(),
                source,
            })?;
        if contents.len() as u64 > MAX_TOKEN_FILE_BYTES {
            return Err(AdminTokenError::TooLarge { path: display });
        }
        let token = contents.trim_ascii();
        if token.is_empty() {
            return Err(AdminTokenError::Empty { path: display });
        }
        if token.len() < MIN_TOKEN_LEN {
            return Err(AdminTokenError::TooShort { path: display });
        }
        Ok(Self::from_token(token))
    }

    fn from_token(token: &[u8]) -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        let mut hasher = Blake2b::<U32>::new();
        hasher.update(KEY_CONTEXT);
        hasher.update(token);
        hasher.finalize_into(GenericArray::from_mut_slice(&mut key[..]));
        let mut credentials = Self {
            key,
            bearer_tag: Zeroizing::new([0u8; 32]),
        };
        let tag = credentials.mac(b"bearer", token).finalize().into_bytes();
        credentials.bearer_tag.copy_from_slice(&tag);
        credentials
    }

    fn mac(&self, purpose: &[u8], message: &[u8]) -> AdminMac {
        let mut mac =
            <AdminMac as KeyInit>::new_from_slice(&*self.key).expect("32-byte key fits BLAKE2b");
        mac.update(purpose);
        mac.update(b"\0");
        mac.update(message);
        mac
    }

    /// Constant-time: compares keyed tags, so neither content nor length leaks.
    fn accepts_bearer(&self, presented: &[u8]) -> bool {
        self.mac(b"bearer", presented)
            .verify_slice(&*self.bearer_tag)
            .is_ok()
    }

    fn issue_session(&self, now: i64) -> String {
        let expiry = (now + SESSION_TTL_SECS).to_string();
        let tag = self
            .mac(b"session", expiry.as_bytes())
            .finalize()
            .into_bytes();
        format!("{expiry}.{}", hex::encode(tag))
    }

    fn accepts_session(&self, session: &str, now: i64) -> bool {
        let Some((expiry, tag)) = session.split_once('.') else {
            return false;
        };
        let (Ok(expires_at), Ok(tag)) = (expiry.parse::<i64>(), hex::decode(tag)) else {
            return false;
        };
        expires_at > now
            && self
                .mac(b"session", expiry.as_bytes())
                .verify_slice(&tag)
                .is_ok()
    }

    fn csrf_token(&self, session: &str) -> String {
        hex::encode(
            self.mac(b"csrf", session.as_bytes())
                .finalize()
                .into_bytes(),
        )
    }

    fn accepts_csrf(&self, session: &str, presented: &str) -> bool {
        hex::decode(presented).is_ok_and(|tag| {
            self.mac(b"csrf", session.as_bytes())
                .verify_slice(&tag)
                .is_ok()
        })
    }

    /// Decide whether a request may reach an operator route. A cookie session yields
    /// the CSRF token that its pages must echo.
    fn authorize(
        &self,
        method: &Method,
        headers: &HeaderMap,
        now: i64,
    ) -> Result<AdminCsrf, AdminRejection> {
        if let Some(authorization) = headers.get(AUTHORIZATION) {
            // A presented but wrong header never falls back to the cookie.
            return authorization
                .to_str()
                .ok()
                .and_then(|value| value.strip_prefix("Bearer "))
                .filter(|token| self.accepts_bearer(token.trim().as_bytes()))
                .map(|_| AdminCsrf(None))
                .ok_or(AdminRejection::Unauthenticated);
        }
        let session = session_cookie(headers)
            .filter(|session| self.accepts_session(session, now))
            .ok_or(AdminRejection::Unauthenticated)?;
        let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
        let echoed = headers
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok());
        if !safe && !echoed.is_some_and(|token| self.accepts_csrf(session, token)) {
            return Err(AdminRejection::CsrfMismatch);
        }
        Ok(AdminCsrf(Some(self.csrf_token(session))))
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

/// CSRF token for the current browser session, rendered into admin pages so HTMX echoes
/// it. `None` for bearer and unauthenticated access, which need no CSRF token.
#[derive(Clone, Debug)]
pub struct AdminCsrf(pub Option<String>);

#[derive(Debug, PartialEq, Eq)]
enum AdminRejection {
    Unauthenticated,
    CsrfMismatch,
}

impl IntoResponse for AdminRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                [(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
                Html(
                    html! {
                        p { "Operator authentication required. " a href="/admin/login" { "Sign in" } }
                    }
                    .into_string(),
                ),
            )
                .into_response(),
            Self::CsrfMismatch => {
                (StatusCode::FORBIDDEN, "missing or invalid CSRF token").into_response()
            }
        }
    }
}

/// Admit only authenticated operators; see the module documentation.
pub async fn require_operator(
    State(access): State<Arc<AdminAccess>>,
    mut request: Request,
    next: Next,
) -> Response {
    let csrf = match access.as_ref() {
        AdminAccess::Unauthenticated => AdminCsrf(None),
        AdminAccess::Token(credentials) => {
            let now = OffsetDateTime::now_utc().unix_timestamp();
            match credentials.authorize(request.method(), request.headers(), now) {
                Ok(csrf) => csrf,
                Err(rejection) => return rejection.into_response(),
            }
        }
    };
    request.extensions_mut().insert(csrf);
    next.run(request).await
}

/// Keep operator pages out of frames and caches.
pub async fn operator_response_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct AdminLoginForm {
    token: String,
}

pub async fn admin_login_page() -> Html<String> {
    login_page(None)
}

/// Exchange the operator token for a browser session cookie.
pub async fn admin_login(
    State(access): State<Arc<AdminAccess>>,
    Form(form): Form<AdminLoginForm>,
) -> Response {
    let credentials = match access.as_ref() {
        AdminAccess::Unauthenticated => return Redirect::to("/admin").into_response(),
        AdminAccess::Token(credentials) => credentials,
    };
    if !credentials.accepts_bearer(form.token.trim().as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            login_page(Some("That token was not accepted.")),
        )
            .into_response();
    }
    let session = credentials.issue_session(OffsetDateTime::now_utc().unix_timestamp());
    let cookie = format!(
        "{SESSION_COOKIE}={session}; Path=/admin; Max-Age={SESSION_TTL_SECS}; HttpOnly; Secure; SameSite=Strict"
    );
    match HeaderValue::from_str(&cookie) {
        Ok(cookie) => ([(SET_COOKIE, cookie)], Redirect::to("/admin")).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn login_page(error: Option<&str>) -> Html<String> {
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="UTF-8";
                    meta name="viewport" content="width=device-width, initial-scale=1.0";
                    title { "5day4cast Admin - Sign in" }
                    link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bulma@1.0.2/css/bulma.min.css";
                }
                body {
                    section class="section" {
                        div class="container" style="max-width: 420px" {
                            h1 class="title" { "Operator sign in" }
                            @if let Some(error) = error {
                                div class="notification is-danger" { (error) }
                            }
                            form method="post" action="/admin/login" {
                                div class="field" {
                                    label class="label" for="token" { "Admin token" }
                                    div class="control" {
                                        input class="input" id="token" name="token" type="password"
                                              autocomplete="off" required;
                                    }
                                }
                                button class="button is-primary" type="submit" { "Sign in" }
                            }
                        }
                    }
                }
            }
        }
        .into_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TOKEN: &[u8] = b"0123456789abcdef0123456789abcdef";
    const NOW: i64 = 1_800_000_000;

    fn credentials() -> AdminCredentials {
        AdminCredentials::from_token(TOKEN)
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn token_file(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents).unwrap();
        file
    }

    #[test]
    fn bearer_token_is_accepted_only_when_it_matches_exactly() {
        let credentials = credentials();
        assert!(credentials.accepts_bearer(TOKEN));
        assert!(!credentials.accepts_bearer(b"0123456789abcdef0123456789abcdeX"));
        assert!(!credentials.accepts_bearer(&TOKEN[..31]));
        assert!(!credentials.accepts_bearer(b""));
        let other = AdminCredentials::from_token(b"ffffffffffffffffffffffffffffffff");
        assert!(!other.accepts_bearer(TOKEN));
    }

    #[test]
    fn token_file_is_trimmed_and_must_be_long_enough() {
        let file = token_file(b"  0123456789abcdef0123456789abcdef\n");
        let loaded = AdminCredentials::load(file.path()).unwrap();
        assert!(loaded.accepts_bearer(TOKEN));

        assert!(matches!(
            AdminCredentials::load(token_file(b" \n").path()),
            Err(AdminTokenError::Empty { .. })
        ));
        assert!(matches!(
            AdminCredentials::load(token_file(b"short").path()),
            Err(AdminTokenError::TooShort { .. })
        ));
        assert!(matches!(
            AdminCredentials::load(token_file(&[b'a'; 5000]).path()),
            Err(AdminTokenError::TooLarge { .. })
        ));
        assert!(matches!(
            AdminCredentials::load(Path::new("/nonexistent/admin_token")),
            Err(AdminTokenError::Unreadable { .. })
        ));
    }

    #[test]
    fn startup_refuses_a_missing_token_unless_unauthenticated_access_is_explicit() {
        let missing = AdminSettings {
            token_file: String::from("/nonexistent/admin_token"),
            ..AdminSettings::default()
        };
        assert!(AdminAccess::from_settings(&missing).is_err());
        let development = AdminSettings {
            dangerous_allow_unauthenticated: true,
            ..missing
        };
        assert!(matches!(
            AdminAccess::from_settings(&development),
            Ok(AdminAccess::Unauthenticated)
        ));
    }

    #[test]
    fn sessions_expire_and_reject_tampering_or_another_token() {
        let credentials = credentials();
        let session = credentials.issue_session(NOW);
        assert!(credentials.accepts_session(&session, NOW));
        assert!(credentials.accepts_session(&session, NOW + SESSION_TTL_SECS - 1));
        assert!(!credentials.accepts_session(&session, NOW + SESSION_TTL_SECS));

        let (expiry, tag) = session.split_once('.').unwrap();
        let extended = format!("{}.{tag}", expiry.parse::<i64>().unwrap() + 3600);
        assert!(!credentials.accepts_session(&extended, NOW));
        assert!(!credentials.accepts_session("garbage", NOW));
        assert!(!credentials.accepts_session(&format!("{expiry}.zz"), NOW));

        let rotated = AdminCredentials::from_token(b"ffffffffffffffffffffffffffffffff");
        assert!(!rotated.accepts_session(&session, NOW));
    }

    #[test]
    fn wrong_bearer_is_rejected_even_with_a_valid_cookie() {
        let credentials = credentials();
        let session = credentials.issue_session(NOW);
        let cookie = format!("{SESSION_COOKIE}={session}");
        let request = headers(&[
            ("authorization", "Bearer wrong-token"),
            ("cookie", cookie.as_str()),
        ]);
        assert_eq!(
            credentials
                .authorize(&Method::GET, &request, NOW)
                .unwrap_err(),
            AdminRejection::Unauthenticated
        );
        let basic = headers(&[("authorization", "Basic MDEyMzQ1Njc4OWFiY2RlZg==")]);
        assert_eq!(
            credentials
                .authorize(&Method::GET, &basic, NOW)
                .unwrap_err(),
            AdminRejection::Unauthenticated
        );
    }

    #[test]
    fn cookie_sessions_need_the_csrf_token_for_state_changes() {
        let credentials = credentials();
        let session = credentials.issue_session(NOW);
        let cookie = format!("theme=dark; {SESSION_COOKIE}={session}");
        let csrf = credentials.csrf_token(&session);

        let read = headers(&[("cookie", cookie.as_str())]);
        let AdminCsrf(rendered) = credentials.authorize(&Method::GET, &read, NOW).unwrap();
        assert_eq!(rendered.as_deref(), Some(csrf.as_str()));

        assert_eq!(
            credentials
                .authorize(&Method::POST, &read, NOW)
                .unwrap_err(),
            AdminRejection::CsrfMismatch
        );
        let forged = headers(&[("cookie", cookie.as_str()), (CSRF_HEADER, "00")]);
        assert_eq!(
            credentials
                .authorize(&Method::POST, &forged, NOW)
                .unwrap_err(),
            AdminRejection::CsrfMismatch
        );
        let other_session = credentials.issue_session(NOW + 1);
        let replayed = headers(&[
            ("cookie", cookie.as_str()),
            (CSRF_HEADER, credentials.csrf_token(&other_session).as_str()),
        ]);
        assert_eq!(
            credentials
                .authorize(&Method::POST, &replayed, NOW)
                .unwrap_err(),
            AdminRejection::CsrfMismatch
        );
        let echoed = headers(&[("cookie", cookie.as_str()), (CSRF_HEADER, csrf.as_str())]);
        assert!(credentials.authorize(&Method::POST, &echoed, NOW).is_ok());
    }

    #[test]
    fn bearer_requests_need_no_csrf_token() {
        let credentials = credentials();
        let bearer = format!("Bearer {}", std::str::from_utf8(TOKEN).unwrap());
        let request = headers(&[("authorization", bearer.as_str())]);
        let AdminCsrf(csrf) = credentials.authorize(&Method::POST, &request, NOW).unwrap();
        assert!(csrf.is_none());
    }
}
