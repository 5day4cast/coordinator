//! The public feedback form: `GET /feedback`, `GET /api/v1/feedback/challenge`, and
//! `POST /api/v1/feedback` (htmx) or `POST /feedback` (a browser without JavaScript).
//! See `domain::feedback` for the checks a message passes.

use std::{net::IpAddr, sync::Arc};

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use axum_extra::extract::Form;
use log::{error, info};
use serde::Deserialize;
use time::OffsetDateTime;

use super::public::{is_fragment, page, Caching};
use crate::{
    api::request_context::{self, log_value},
    domain::{
        feedback::{
            clean_text, page_path, too_long, NewFeedback, DUPLICATE_WINDOW_SECS, MAX_CONTACT_CHARS,
            MAX_MESSAGE_CHARS, MAX_META_CHARS,
        },
        PowProof,
    },
    startup::AppState,
    templates::components::feedback::{
        feedback_form, feedback_page, feedback_thanks, FeedbackDraft,
    },
};

const TITLE: &str = "Feedback - Fantasy Weather";
/// Largest form body read.
pub const FEEDBACK_MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Debug, Default, Deserialize)]
pub struct FeedbackForm {
    #[serde(default)]
    message: String,
    #[serde(default)]
    contact: String,
    /// The honeypot.
    #[serde(default)]
    website: String,
    #[serde(default)]
    pow_challenge: String,
    #[serde(default)]
    pow_nonce: String,
}

/// Why a message was not taken. Each is one sentence for the form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    Empty,
    TooLong,
    StaleProof,
    Busy,
    Unavailable,
}

impl Refusal {
    fn message(self) -> &'static str {
        match self {
            Self::Empty => "Write a message first.",
            Self::TooLong => "Messages can be up to 2000 characters; please shorten it.",
            Self::StaleProof => "This form went stale. Press Send again.",
            Self::Busy => "We have had a lot of messages just now. Please try again later.",
            Self::Unavailable => "Your message could not be saved just now. Please try again.",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::Empty | Self::TooLong | Self::StaleProof => StatusCode::BAD_REQUEST,
            Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

fn unix(now: OffsetDateTime) -> u64 {
    u64::try_from(now.unix_timestamp()).unwrap_or_default()
}

/// `GET /feedback`: the form on a page of its own.
pub async fn feedback_page_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !state.feedback.enabled {
        return super::not_found(&headers, &state, "Page");
    }
    page(
        &headers,
        &state,
        TITLE,
        feedback_page(feedback_form(&FeedbackDraft::default())),
        Caching::Public,
    )
}

/// `GET /api/v1/feedback/challenge`: a proof-of-work challenge for one message.
pub async fn feedback_challenge(State(state): State<Arc<AppState>>) -> Response {
    let feedback = &state.feedback;
    if !feedback.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let pow = &feedback.pow;
    let mut response =
        Json(pow.issue(pow.difficulty(0), unix(OffsetDateTime::now_utc()))).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `POST /api/v1/feedback` and `POST /feedback`.
pub async fn post_feedback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<FeedbackForm>,
) -> Response {
    if !state.feedback.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let fragment = is_fragment(&headers);
    let answer = |refusal: Option<Refusal>| -> Response {
        let content = match refusal {
            None => feedback_thanks(!fragment),
            Some(refusal) => feedback_form(&FeedbackDraft {
                message: &form.message,
                contact: &form.contact,
                error: Some(refusal.message()),
            }),
        };
        if fragment {
            // htmx swaps only successful answers, so refusals are 200 too.
            let mut response = content.into_string().into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, Caching::Private.header());
            return response;
        }
        let mut response = page(
            &headers,
            &state,
            TITLE,
            feedback_page(content),
            Caching::Private,
        );
        if let Some(refusal) = refusal {
            *response.status_mut() = refusal.status();
        }
        response
    };
    match accept(&state, &headers, &form).await {
        Ok(()) => answer(None),
        Err(refusal) => answer(Some(refusal)),
    }
}

/// Check and store a message. Dropped messages (the honeypot, a repeat) are `Ok` too.
async fn accept(state: &AppState, headers: &HeaderMap, form: &FeedbackForm) -> Result<(), Refusal> {
    let feedback = &state.feedback;
    let now = OffsetDateTime::now_utc();
    let context = request_context::current();
    let ip = context
        .as_ref()
        .map(|context| context.ip)
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let sid = context.as_ref().and_then(|context| context.sid.clone());

    if !form.website.trim().is_empty() {
        info!("Feedback dropped: the hidden field was filled");
        return Ok(());
    }
    let full = clean_text(&form.message, usize::MAX, true).ok_or(Refusal::Empty)?;
    if too_long(&full) {
        return Err(Refusal::TooLong);
    }
    let message = clean_text(&full, MAX_MESSAGE_CHARS, true).ok_or(Refusal::Empty)?;
    let contact = clean_text(&form.contact, MAX_CONTACT_CHARS, false);

    let with_work = !form.pow_challenge.is_empty() || !form.pow_nonce.is_empty();
    if with_work {
        let proof = PowProof {
            pow_challenge: Some(form.pow_challenge.clone()),
            pow_nonce: Some(form.pow_nonce.clone()),
        };
        let pow = &feedback.pow;
        pow.verify(&proof, pow.difficulty(0), unix(now))
            .map_err(|rejection| {
                info!("Feedback proof of work refused: {}", rejection.label());
                Refusal::StaleProof
            })?;
    }
    feedback
        .limits
        .admit(sid.as_deref(), ip, with_work, unix(now))
        .map_err(|limited| {
            info!("Feedback refused by the {limited} limit");
            Refusal::Busy
        })?;
    match feedback
        .store
        .is_duplicate(
            &message,
            now - time::Duration::seconds(DUPLICATE_WINDOW_SECS),
        )
        .await
    {
        Ok(true) => {
            info!("Feedback dropped: a repeat of a recent message");
            return Ok(());
        }
        Ok(false) => {}
        Err(e) => {
            error!("Feedback duplicate check failed: {e}");
            return Err(Refusal::Unavailable);
        }
    }

    let page = headers
        .get("HX-Current-URL")
        .or_else(|| headers.get(header::REFERER))
        .and_then(|value| value.to_str().ok())
        .and_then(page_path);
    let rid = context.as_ref().map(|context| context.rid.clone());
    let pubkey = context
        .as_ref()
        .and_then(|context| context.user.lock().ok().and_then(|user| user.clone()));
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .and_then(|agent| clean_text(agent, MAX_META_CHARS, false));
    let id = feedback
        .store
        .insert(
            NewFeedback {
                message,
                contact,
                page: page.clone(),
                rid: rid.clone(),
                sid: sid.clone(),
                pubkey,
                ip: Some(ip.to_string()),
                user_agent,
            },
            now,
        )
        .await
        .map_err(|e| {
            error!("Feedback could not be stored: {e}");
            Refusal::Unavailable
        })?;
    // Never the message or the contact.
    info!(
        target: "feedback",
        "feedback id={id} rid={} sid={} ip={ip} page={}",
        log_value(rid.as_deref().unwrap_or("-")),
        log_value(sid.as_deref().unwrap_or("-")),
        log_value(page.as_deref().unwrap_or("-")),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_say_what_to_do() {
        for refusal in [
            Refusal::Empty,
            Refusal::TooLong,
            Refusal::StaleProof,
            Refusal::Busy,
            Refusal::Unavailable,
        ] {
            assert!(refusal.message().ends_with('.'));
            assert!(refusal.status().is_client_error() || refusal.status().is_server_error());
        }
    }
}
