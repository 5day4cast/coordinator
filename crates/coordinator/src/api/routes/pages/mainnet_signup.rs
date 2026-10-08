use super::{
    admin::render_admin_fragment,
    public::{is_fragment, page, Caching},
};
use crate::{
    api::{admin_auth::AdminCsrf, request_context},
    domain::{mainnet_signup::normalize_email, PowProof},
    startup::AppState,
    templates::{
        admin::mainnet_signup::mainnet_signups,
        components::mainnet_signup::{
            mainnet_signup_form, mainnet_signup_page, mainnet_signup_thanks,
        },
    },
};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    Extension, Json,
};
use axum_extra::extract::Form;
use serde::Deserialize;
use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};
use time::OffsetDateTime;
const TITLE: &str = "Mainnet signup - Fantasy Weather";

#[derive(Default, Deserialize)]
pub struct MainnetSignupForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    website: String,
    #[serde(default)]
    pow_challenge: String,
    #[serde(default)]
    pow_nonce: String,
}

pub async fn mainnet_signup_page_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !state.mainnet_signup.enabled {
        return super::not_found(&headers, &state, "Page");
    }
    page(
        &headers,
        &state,
        TITLE,
        mainnet_signup_page(mainnet_signup_form("", None)),
        Caching::Public,
    )
}

pub async fn mainnet_signup_challenge(State(state): State<Arc<AppState>>) -> Response {
    if !state.mainnet_signup.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let pow = &state.mainnet_signup.pow;
    let mut response = Json(pow.issue(
        pow.difficulty(0),
        OffsetDateTime::now_utc().unix_timestamp() as u64,
    ))
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub async fn post_mainnet_signup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<MainnetSignupForm>,
) -> Response {
    if !state.mainnet_signup.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let result = accept(&state, &form).await;
    let fragment = is_fragment(&headers);
    let content = match &result {
        Ok(()) => mainnet_signup_thanks(!fragment),
        Err((_, message)) => mainnet_signup_form(&form.email, Some(message)),
    };
    if fragment {
        // As with Feedback, htmx swaps validation errors along with the form.
        let mut response = Html(content.into_string()).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, Caching::Private.header());
        response
    } else {
        let mut response = page(
            &headers,
            &state,
            TITLE,
            mainnet_signup_page(content),
            Caching::Private,
        );
        if let Err((status, _)) = result {
            *response.status_mut() = status;
        }
        response
    }
}

async fn accept(
    state: &AppState,
    form: &MainnetSignupForm,
) -> Result<(), (StatusCode, &'static str)> {
    if !form.website.trim().is_empty() {
        return Ok(());
    }
    let email = normalize_email(&form.email)
        .ok_or((StatusCode::BAD_REQUEST, "Enter a valid email address."))?;
    let signup = &state.mainnet_signup;
    let now = OffsetDateTime::now_utc();
    let unix = now.unix_timestamp() as u64;
    let with_work = !form.pow_challenge.is_empty() || !form.pow_nonce.is_empty();
    if with_work {
        signup
            .pow
            .verify(
                &PowProof {
                    pow_challenge: Some(form.pow_challenge.clone()),
                    pow_nonce: Some(form.pow_nonce.clone()),
                },
                signup.pow.difficulty(0),
                unix,
            )
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "This form went stale. Press Notify me again.",
                )
            })?;
    }
    let context = request_context::current();
    let ip = context
        .as_ref()
        .map(|context| context.ip)
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let sid = context.as_ref().and_then(|context| context.sid.as_deref());
    signup.limits.admit(sid, ip, with_work, unix).map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "We've had a lot of signups just now. Please try again later.",
        )
    })?;
    signup.store.insert(email, now).await.map_err(|_| {
        // Do not log form fields or database errors that could contain an email address.
        log::error!("Mainnet signup could not be stored");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Your signup could not be saved just now. Please try again.",
        )
    })
}

#[derive(Default, Deserialize)]
pub struct MainnetSignupPage {
    #[serde(default)]
    page: u32,
}
const PAGE_SIZE: i64 = 100;

pub async fn admin_mainnet_signups(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Query(query): Query<MainnetSignupPage>,
    headers: HeaderMap,
) -> Response {
    let store = &state.mainnet_signup.store;
    let result = async {
        let total = store.count().await?;
        let rows = store
            .list(PAGE_SIZE, i64::from(query.page) * PAGE_SIZE)
            .await?;
        Ok::<_, crate::domain::Error>((total, rows))
    }
    .await;
    let mut response = match result {
        Ok((total, rows)) => render_admin_fragment(
            &headers,
            &state,
            &csrf,
            "Mainnet signups",
            mainnet_signups(
                &rows,
                total,
                query.page,
                PAGE_SIZE,
                state.mainnet_signup.enabled,
            ),
        )
        .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Mainnet signups are unavailable. Please try again.",
        )
            .into_response(),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, Caching::Private.header());
    response
}

/// CSV cells are quoted and spreadsheet formulas are neutralized on export only.
fn csv_cell(value: &str) -> String {
    let prefix = if value.starts_with(['=', '+', '-', '@']) {
        "'"
    } else {
        ""
    };
    format!("\"{prefix}{}\"", value.replace('"', "\"\""))
}

pub async fn admin_mainnet_signups_csv(State(state): State<Arc<AppState>>) -> Response {
    let rows = match state.mainnet_signup.store.list(-1, 0).await {
        Ok(rows) => rows,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Mainnet signups are unavailable. Please try again.",
            )
                .into_response()
        }
    };
    let mut csv = String::from("email,created_at\r\n");
    for row in rows {
        csv.push_str(&format!(
            "{},{}\r\n",
            csv_cell(&row.email),
            csv_cell(&row.created_at)
        ));
    }
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=mainnet-signups.csv",
            ),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        csv,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_quotes_fields_and_neutralizes_spreadsheet_formulas() {
        assert_eq!(
            csv_cell("player+launch@example.com"),
            "\"player+launch@example.com\""
        );
        assert_eq!(csv_cell("=1+1@example.com"), "\"'=1+1@example.com\"");
        assert_eq!(csv_cell("a\"b"), "\"a\"\"b\"");
    }
}
