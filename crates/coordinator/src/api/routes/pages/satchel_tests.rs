//! The Satchel test wallet is offered, and allowed by the pages' policy, only where a
//! deployment configures it.

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr::Keys;
use tower::ServiceExt;

use super::leaderboard_tests::Coordinator;
use crate::api::extractors::create_auth_event;

const SATCHEL: &str = "https://wallet.5day4cast.com";

/// A GET of `path`, as htmx sends it signed by `keys` when given, and the answer's status,
/// Content-Security-Policy and body.
async fn get(
    coordinator: &Coordinator,
    path: &str,
    keys: Option<&Keys>,
) -> (StatusCode, String, String) {
    let mut request = Request::builder().uri(path);
    if let Some(keys) = keys {
        // The default origins name the UI served on port 9990.
        let url = format!("http://localhost:9990{path}");
        let event = create_auth_event("GET", &url, None, keys).await.unwrap();
        request = request.header("HX-Request", "true").header(
            header::AUTHORIZATION,
            format!(
                "Nostr {}",
                BASE64.encode(serde_json::to_vec(&event).unwrap())
            ),
        );
    }
    let response = coordinator
        .router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let policy = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, policy, String::from_utf8(body.to_vec()).unwrap())
}

/// The directive of `policy` named `name`, in full.
fn directive<'a>(policy: &'a str, name: &str) -> &'a str {
    policy
        .split("; ")
        .find(|directive| directive.split(' ').next() == Some(name))
        .unwrap_or_default()
}

#[tokio::test]
async fn satchel_is_offered_and_allowed_only_when_configured() {
    for satchel in [None, Some(SATCHEL)] {
        let coordinator = Coordinator::start_with("http://127.0.0.1:9".into(), |settings| {
            settings.ui_settings.satchel_url = satchel.map(|origin| format!("{origin}/"));
        })
        .await;
        // Any page: the payment dialog and the account menu are in its layout.
        let (status, policy, page) = get(&coordinator, "/help", None).await;
        assert_eq!(status, StatusCode::OK);
        // The Payouts page, where the player's Lightning Address is set.
        let (status, _, payouts) = get(&coordinator, "/payouts", Some(&Keys::generate())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(payouts.contains(r#"id="payoutLightningAddress""#));

        match satchel {
            Some(origin) => {
                // The handoff form posts to Satchel, and the address lookup connects to it.
                assert_eq!(
                    directive(&policy, "form-action"),
                    format!("form-action 'self' {origin}")
                );
                assert!(
                    directive(&policy, "connect-src")
                        .split(' ')
                        .any(|source| source == origin),
                    "{policy}"
                );
                assert!(page.contains(&format!(r#"data-satchel-url="{origin}""#)));
                assert!(page.contains(r#"id="walletLinkSatchel""#));
                assert!(page.contains("Pay with Satchel"));
                assert!(page.contains(r#"id="openSatchelNavClick""#));
                assert!(page.contains(&format!(r#"href="{origin}/wallet""#)));
                assert!(payouts.contains(r#"id="satchelAddress""#));
                assert!(payouts.contains("Get a Lightning Address with Satchel"));
            }
            None => {
                assert_eq!(directive(&policy, "form-action"), "form-action 'self'");
                assert!(!policy.contains("wallet.5day4cast.com"), "{policy}");
                assert!(page.contains(r#"id="walletLinkZeus""#));
                assert!(!page.contains("Satchel") && !page.contains("data-satchel"));
                assert!(!payouts.contains("Satchel"));
            }
        }
        coordinator.stop().await;
    }
}
