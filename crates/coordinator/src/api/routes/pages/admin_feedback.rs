//! The operator's Feedback pages. Changes go through htmx, which sends the session's CSRF
//! token (`api::admin_auth`).

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Extension,
};
use axum_extra::extract::Form;
use log::error;
use serde::Deserialize;
use uuid::Uuid;

use super::admin::render_admin_fragment;
use crate::{
    api::admin_auth::AdminCsrf,
    domain::feedback::{clean_text, FeedbackStatus},
    startup::AppState,
    templates::admin::feedback::{
        feedback_detail, feedback_list, feedback_status_form, unread_badge,
    },
};

/// Messages the list shows.
const LIST_LIMIT: i64 = 200;

#[derive(Debug, Default, Deserialize)]
pub struct FeedbackFilter {
    #[serde(default)]
    status: Option<String>,
}

pub async fn admin_feedback_list(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Query(filter): Query<FeedbackFilter>,
    headers: HeaderMap,
) -> Html<String> {
    // New messages first, unless another status or all are asked for.
    let selected = match filter.status.as_deref() {
        None => Some(FeedbackStatus::New),
        Some("all") => None,
        Some(status) => status.parse().ok(),
    };
    let rows = state
        .feedback
        .store
        .list(selected, LIST_LIMIT)
        .await
        .inspect_err(|e| error!("Feedback list failed: {e}"))
        .ok();
    let content = feedback_list(rows.as_deref(), selected, state.feedback.enabled);
    render_admin_fragment(&headers, &state, &csrf, "Feedback", content)
}

/// The unread count the navigation loads beside Feedback.
pub async fn admin_feedback_unread(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(unread_badge(state.feedback.store.count_new().await.ok()).into_string())
}

pub async fn admin_feedback_detail(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let store = &state.feedback.store;
    match store.get(id).await {
        Ok(Some(mut row)) => {
            // Opening a new message marks it seen.
            if row.status == FeedbackStatus::New {
                match store.mark_seen(id).await {
                    Ok(()) => row.status = FeedbackStatus::Seen,
                    Err(e) => error!("Feedback {id} not marked seen: {e}"),
                }
            }
            render_admin_fragment(
                &headers,
                &state,
                &csrf,
                "Feedback",
                feedback_detail(&row, false),
            )
            .into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Feedback {id} unavailable: {e}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FeedbackUpdate {
    status: String,
    #[serde(default)]
    note: String,
}

/// Set a message's status and note; answers the form again.
pub async fn admin_feedback_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Form(update): Form<FeedbackUpdate>,
) -> Response {
    let Ok(status) = update.status.parse::<FeedbackStatus>() else {
        return (StatusCode::BAD_REQUEST, "Unknown status").into_response();
    };
    let note = clean_text(&update.note, 2000, true).unwrap_or_default();
    let store = &state.feedback.store;
    match store.update(id, status, Some(note)).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Feedback {id} not updated: {e}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    match store.get(id).await {
        Ok(Some(row)) => Html(feedback_status_form(&row, true).into_string()).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Feedback {id} unavailable: {e}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}
