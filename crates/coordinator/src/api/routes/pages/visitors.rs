//! The operator's Visitors page. The page renders at once; the results load as a fragment,
//! read from the visitor logs with one of the fixed queries in `infra::visitor_logs`.

use std::{net::IpAddr, sync::Arc};

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Html,
    Extension,
};
use log::info;
use nostr::PublicKey;
use serde::Deserialize;

use super::admin::render_admin_fragment;
use crate::{
    api::admin_auth::AdminCsrf,
    domain::Error,
    infra::visitor_logs::{Search, Window},
    startup::AppState,
    templates::admin::visitors::{
        visitors_error, visitors_invalid, visitors_page, visitors_results, VisitorQuery,
    },
};

#[derive(Debug, Default, Deserialize)]
pub struct VisitorParams {
    #[serde(default)]
    ip: String,
    #[serde(default)]
    rid: String,
    #[serde(default)]
    sid: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    window: String,
    #[serde(default)]
    synth: String,
}

impl VisitorParams {
    fn query(self) -> VisitorQuery {
        VisitorQuery {
            window: Window::parse(&self.window).unwrap_or_default(),
            synthetic: !self.synth.is_empty(),
            ip: self.ip,
            rid: self.rid,
            sid: self.sid,
            user: self.user,
        }
    }
}

pub async fn visitors_page_handler(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Query(params): Query<VisitorParams>,
    headers: HeaderMap,
) -> Html<String> {
    let content = visitors_page(&state.visitor_logs, &params.query());
    render_admin_fragment(&headers, &state, &csrf, "Visitors", content)
}

/// The results fragment the page loads.
pub async fn visitors_results_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<VisitorParams>,
) -> Html<String> {
    let query = params.query();
    let search = match search(&state, &query).await {
        Ok(search) => search,
        Err(reason) => return Html(visitors_invalid(&reason).into_string()),
    };
    let logs = &state.visitor_logs;
    let content = match logs.search(&search, query.window).await {
        Ok(report) => {
            info!(
                "Visitor search over {} read {} lines in {} ms",
                query.window.as_str(),
                report.lines.len(),
                report.took.as_millis()
            );
            visitors_results(&report, logs, &search, &query)
        }
        Err(error) => visitors_error(
            &error,
            logs.explore_search(&search, query.window).as_deref(),
        ),
    };
    Html(content.into_string())
}

/// The one search the form asks for: a request id, else a session id, else an account, else
/// an address. Each value is checked before it goes into a query.
async fn search(state: &AppState, query: &VisitorQuery) -> Result<Search, String> {
    let rid = query.rid.trim();
    if !rid.is_empty() {
        return Search::rid(rid).ok_or_else(|| "That is not a request id.".to_owned());
    }
    let sid = query.sid.trim();
    if !sid.is_empty() {
        return Search::sid(sid).ok_or_else(|| "That is not a session id.".to_owned());
    }
    let user = query.user.trim();
    if !user.is_empty() {
        return user_search(state, user).await;
    }
    let ip = query.ip.trim();
    if !ip.is_empty() {
        return ip
            .parse::<IpAddr>()
            .map(|ip| Search::Ip(ip.to_canonical()))
            .map_err(|_| "That is not an IP address.".to_owned());
    }
    Err("Enter an address, a request id, a session id, or an account.".to_owned())
}

/// A hex key (or its first 16 characters), an npub, or a username looked up to its key.
async fn user_search(state: &AppState, user: &str) -> Result<Search, String> {
    if let Some(search) = Search::user_hex(user) {
        return Ok(search);
    }
    if user.starts_with("npub1") {
        return PublicKey::parse(user)
            .ok()
            .and_then(|key| Search::user_hex(&key.to_hex()))
            .ok_or_else(|| "That npub is not valid.".to_owned());
    }
    let valid_username = user.len() <= 64
        && user
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !valid_username {
        return Err("That is not a pubkey, npub or username.".to_owned());
    }
    match state.users_info.get_pubkey_by_username(user).await {
        Ok(stored) => PublicKey::parse(&stored)
            .ok()
            .and_then(|key| Search::user_hex(&key.to_hex()))
            .ok_or_else(|| "That account's key could not be read.".to_owned()),
        Err(Error::NotFound(_)) => Err("No account has that username.".to_owned()),
        Err(error) => {
            log::error!("Username lookup for the Visitors page failed: {error}");
            Err("Accounts could not be read just now.".to_owned())
        }
    }
}
