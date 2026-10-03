//! The Ark wallet card: what ark-swapd's wallet can fund, what waits to be boarded, the last
//! refill and the next check, with refills paused and resumed from it as a scenario is; and the
//! page with every refill.

use axum::{
    extract::{Form, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use maud::{html, Markup};
use serde::Deserialize;
use time::OffsetDateTime;

use super::format;
use super::live;
use super::routes::{from_htmx, html_by_hx_request, Dashboard};
use crate::ark_refill::{self, Refill};

/// Where the refill history is.
pub(super) const PATH: &str = "/ark-refill";

/// How many refills the history page lists.
const HISTORY: i64 = 100;

pub(super) fn router(state: Dashboard) -> Router {
    Router::new()
        .route(PATH, get(history))
        .route("/api/ark-refill", post(set_paused))
        .with_state(state)
}

/// The dashboard's card.
pub(super) async fn card(state: &Dashboard, now: OffsetDateTime) -> Markup {
    let Some(refiller) = &state.ark_refiller else {
        return html! {
            section id="ark-wallet" {
                h2 { "Ark wallet" }
                p.note { "Refilling ark-swapd's Ark wallet is not enabled (ark_refill)." }
            }
        };
    };
    let db = state.runner.db();
    let observation = refiller.last().await;
    let last = ark_refill::list(db, 1).await;
    let paused = ark_refill::paused(db).await;
    let config = refiller.config();
    html! {
        section id="ark-wallet" {
            h2 { "Ark wallet" }
            @match &observation.wallet {
                Some(wallet) => p {
                    "Payable " strong { (format::sats(wallet.payable())) } " sats · boarding "
                    strong { (format::sats(wallet.boarding_sat)) } " sats. Refilled below "
                    (format::sats(config.low_water_sats)) " up to " (format::sats(config.target_sats)) " sats."
                },
                None if observation.checked_at.is_some() => p.error { "ark-swapd did not report its wallet." },
                None => p { "Not checked yet." },
            }
            @if let Some(error) = &observation.error { p.error { "Last check failed: " (error) } }
            @if let Some(decision) = &observation.decision { p.note { "Last check: " (decision) "." } }
            @match &last {
                Ok(refills) => {
                    @match refills.first() {
                        Some(refill) => p {
                            "Last refill: " (format::sats(sats(refill))) " sats, "
                            span class=(format!("badge {}", refill.status)) { (refill.status) } " "
                            (format::time(refill.created_time(), now))
                        },
                        None => p { "No refill sent yet." },
                    }
                },
                Err(_) => p.error { "Refills could not be read." },
            }
            @if let Some(next) = observation.next_check_at {
                p.note { "Next check " (format::time(next, now)) }
            }
            @match paused {
                Ok(paused) => form method="post" action="/api/ark-refill"
                    hx-post="/api/ark-refill" hx-target="#ark-refill-result" {
                    input type="hidden" name="paused" value=(if paused { "false" } else { "true" });
                    span.badge { (if paused { "Paused" } else { "On" }) }
                    " " button type="submit" { (if paused { "Resume refills" } else { "Pause refills" }) }
                },
                Err(_) => p.error { "The refill control could not be read, so refills are paused." },
            }
            p id="ark-refill-result" role="status" {}
            p { a href=(PATH) { "Refill history →" } }
        }
    }
}

fn sats(refill: &Refill) -> u64 {
    u64::try_from(refill.amount_sats).unwrap_or(0)
}

/// The history page's live part: the card, and every refill.
pub(super) async fn history_live(state: &Dashboard) -> Markup {
    let now = OffsetDateTime::now_utc();
    let refills = ark_refill::list(state.runner.db(), HISTORY).await;
    let card = card(state, now).await;
    html! {
        (card)
        section.history {
            h2 { "Refills" }
            @match &refills {
                Err(_) => p.error { "Refills could not be read." },
                Ok(refills) if refills.is_empty() => p { "No refill sent yet." },
                Ok(refills) => div.scroll { table.stack {
                    thead { tr {
                        th { "When" } th.num { "Amount" } th.num { "Payable before" }
                        th { "Status" } th { "Transaction / error" }
                    } }
                    tbody {
                        @for refill in refills {
                            tr {
                                td data-label="When" { (format::time(refill.created_time(), now)) }
                                td.num data-label="Amount" { (format::sats(sats(refill))) " sats" }
                                td.num data-label="Payable before" { (format::sats_signed(refill.payable_before_sats)) " sats" }
                                td data-label="Status" { span class=(format!("badge {}", refill.status)) { (refill.status) } }
                                td data-label="Transaction / error" {
                                    @if let Some(txid) = &refill.txid { (format::copyable_short(txid)) }
                                    @if let Some(error) = &refill.error_message { @if refill.txid.is_some() { br; } span.error { (error) } }
                                    @if refill.txid.is_none() && refill.error_message.is_none() { "-" }
                                }
                            }
                        }
                    }
                } },
            }
            p.note { "Refills go on-chain to ark-swapd's boarding address, labelled " code { (ark_refill::LABEL) } " in the payer's wallet." }
        }
    }
}

async fn history(State(state): State<Dashboard>, headers: HeaderMap) -> Response {
    let live = history_live(&state).await;
    if from_htmx(&headers) {
        return html_by_hx_request(live);
    }
    let header = html! {
        p { a href="/" { "← Dashboard" } }
        h1 { "Ark wallet refills" }
    };
    html_by_hx_request(live::page(
        "Synth - Ark wallet refills",
        live::ARK_REFILL,
        header,
        live,
    ))
}

#[derive(Deserialize)]
struct Control {
    paused: bool,
}

async fn set_paused(
    State(state): State<Dashboard>,
    headers: HeaderMap,
    Form(control): Form<Control>,
) -> Response {
    if let Err(error) = ark_refill::set_paused(state.runner.db(), control.paused).await {
        log::error!("Cannot save the refill control: {error:#}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the refill control",
        )
            .into_response();
    }
    log::info!(
        "Ark wallet refills {} from the dashboard",
        if control.paused { "paused" } else { "resumed" }
    );
    state
        .runner
        .events()
        .send(crate::events::Event::ArkRefillChecked);
    if from_htmx(&headers) {
        return Html(
            html! { "Refills " (if control.paused { "paused" } else { "resumed" }) "." }
                .into_string(),
        )
        .into_response();
    }
    Redirect::to("/#ark-wallet").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SynthDb;
    use axum::{body::Body, http::header, http::Request};
    use tower::ServiceExt;

    /// A refiller that reaches nothing, with the files it reads in `directory`.
    fn refiller(directory: &std::path::Path, db: &SynthDb) -> crate::ark_refill::ArkRefiller {
        let token = directory.join("ark-swap.token");
        std::fs::write(&token, "token").unwrap();
        let macaroon = directory.join("refill.macaroon");
        std::fs::write(&macaroon, [1u8]).unwrap();
        let config: crate::ark_refill::ArkRefillConfig = toml::from_str(&format!(
            r#"
enabled = true
[ark_swap]
url = "http://127.0.0.1:1"
token_file = "{}"
[lnd]
rest_url = "http://127.0.0.1:1"
macaroon_file = "{}"
"#,
            token.display(),
            macaroon.display()
        ))
        .unwrap();
        crate::ark_refill::ArkRefiller::new(config, db.clone(), crate::events::Events::new())
            .unwrap()
    }

    #[tokio::test]
    async fn refills_are_paused_from_the_card_by_an_operator_only() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        let mut dashboard = Dashboard::for_tests(db.clone());
        let now = OffsetDateTime::now_utc();
        let off = card(&dashboard, now).await.into_string();
        assert!(off.contains("not enabled"), "{off}");

        dashboard.ark_refiller = Some(refiller(directory.path(), &db));
        let token_path = directory.path().join("operator-token");
        let token = "test-operator-token-with-at-least-32-characters";
        std::fs::write(&token_path, token).unwrap();
        let config = crate::config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            allowed_origins: vec!["https://synth.example".into()],
            operator_token_file: Some(token_path),
        };
        let app = router(dashboard.clone()).layer(axum::middleware::from_fn_with_state(
            super::super::operator::OperatorAccess::new(&config).unwrap(),
            super::super::operator::authorize,
        ));
        let request = |authorized: bool| {
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/ark-refill")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if authorized {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            request.body(Body::from("paused=true")).unwrap()
        };
        let card_on = card(&dashboard, now).await.into_string();
        assert!(card_on.contains("Pause refills"), "{card_on}");
        assert!(card_on.contains("Not checked yet"), "{card_on}");
        assert_eq!(
            app.clone().oneshot(request(false)).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert!(!ark_refill::paused(&db).await.unwrap());
        let response = app.clone().oneshot(request(true)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/#ark-wallet");
        assert!(ark_refill::paused(&db).await.unwrap());
        let paused = card(&dashboard, now).await.into_string();
        assert!(paused.contains("Paused"), "{paused}");
        assert!(paused.contains("Resume refills"), "{paused}");

        let page = app
            .oneshot(Request::get(PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let body = axum::body::to_bytes(page.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("No refill sent yet"), "{body}");
        assert!(body.contains(ark_refill::LABEL), "{body}");
    }
}
