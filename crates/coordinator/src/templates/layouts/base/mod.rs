use maud::{html, Markup, DOCTYPE};

use crate::api::{request_context, telemetry};
use crate::templates::{
    assets::{APP_JS, BULMA_CSS, HTMX_JS, LOGIN_WORKER_JS, STYLES_CSS, THEME_JS},
    components::{
        auth_modals,
        feedback::{feedback_link, feedback_modal},
        mainnet_signup::{mainnet_signup_link, mainnet_signup_modal},
        navbar, RecoveryHelp,
    },
};

/// htmx 4 settings for the public pages, stated in full so they hold whatever
/// htmx's defaults become:
/// - requests only to this site (`mode`), each given up after 10 s; the
///   server answers within 400 ms even when the oracle is slow (fragments
///   say "still loading" and ask again instead);
/// - attributes apply to the element they are on, never to its children;
/// - error responses (4xx/5xx) are not swapped into the page unless the
///   element asks for its status with `hx-status:<code>`;
/// - history stores nothing: Back fetches the page from the server again
///   (htmx 4 keeps no snapshots, so account pages never sit in storage);
/// - no inline indicator styles (base.css has them);
/// - only this site's own htmx extensions may register: the NIP-98 signer
///   and the Trusted Types policy (shared/htmx_auth.js, htmx_security.js).
pub const HTMX_CONFIG: &str = r#"{"mode":"same-origin","defaultTimeout":10000,"implicitInheritance":false,"noSwap":[204,304,"4xx","5xx"],"history":true,"includeIndicatorCSS":false,"extensions":"fw-auth, fw-security"}"#;

/// Where players reach the people running the site.
pub const CONTACT_EMAIL: &str = "5day4cast@protonmail.com";

pub struct PageConfig<'a> {
    pub title: &'a str,
    pub api_base: &'a str,
    pub oracle_base: &'a str,
    pub network: &'a str,
    /// Hash of the WASM package on disk; versions its URLs so browsers can cache it.
    pub wasm_version: &'a str,
    /// What the sign-up dialog says about the recovery key.
    pub recovery: RecoveryHelp,
    /// Origin of the Satchel test-network wallet, when configured: the payment dialog and
    /// account menu link to it, and `shared/satchel.js` signs the player in there.
    pub satchel_url: Option<&'a str>,
    /// The footer links to the feedback form, when it is on.
    pub feedback: bool,
    pub mainnet_signup: bool,
}

pub fn base(config: &PageConfig, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                meta name="htmx-config" content=(HTMX_CONFIG);
                // The page's request id joins the browser's telemetry to the server's log.
                @if let Some(context) = request_context::current() {
                    meta name="request-id" content=(context.rid);
                }
                @if telemetry::enabled() {
                    meta name="telemetry" content="on";
                }
                title { (config.title) }

                link rel="stylesheet" href=(BULMA_CSS.url);
                link rel="stylesheet" href=(STYLES_CSS.url);

                // The saved or system theme, applied before first paint.
                script src=(THEME_JS.url) {}
                script src=(HTMX_JS.url) defer {}
                script src=(APP_JS.url) defer {}
            }
            body data-api-base=(config.api_base) data-oracle-base=(config.oracle_base)
                 data-network=(config.network) data-wasm-version=(config.wasm_version)
                 data-login-worker=(LOGIN_WORKER_JS.url) data-satchel-url=[config.satchel_url] {
                // Shown while a navigation is loading (see base.css).
                div class="page-loading" aria-hidden="true" {}
                (navbar(config.satchel_url))

                section class="section pt-3" {
                    // Back swaps in only the page content, fetched again
                    // (see HTMX_CONFIG); the navbar and dialogs keep their
                    // state and listeners.
                    main class="container" id="main-content" hx-history-elt {
                        (content)
                    }
                }

                footer class="site-footer" {
                    a href="/help" hx-get="/help" hx-target="#main-content" hx-push-url="true" { "How it works" }
                    " · Contact "
                    a href=(format!("mailto:{CONTACT_EMAIL}")) { (CONTACT_EMAIL) }
                    @if config.feedback {
                        " · "
                        (feedback_link())
                    }
                    @if config.mainnet_signup { " · " (mainnet_signup_link()) }
                }

                (auth_modals(config.recovery, config.satchel_url))
                @if config.feedback {
                    (feedback_modal())
                }
                @if config.mainnet_signup { (mainnet_signup_modal()) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> String {
        page_with(None)
    }

    fn page_with(satchel_url: Option<&str>) -> String {
        let config = PageConfig {
            title: "Fantasy Weather",
            api_base: "https://5day4cast.com",
            oracle_base: "https://4casttruth.win",
            network: "signet",
            wasm_version: "abc123",
            recovery: RecoveryHelp::FileAndRelays,
            satchel_url,
            feedback: false,
            mainnet_signup: true,
        };
        base(&config, html! { p { "content" } }).into_string()
    }

    /// Satchel's buttons, beside Zeus in the payment dialog and in the account menu, are on the
    /// page only when it is configured.
    #[test]
    fn satchel_is_offered_only_when_configured() {
        let html = page();
        assert!(html.contains(r#"id="walletLinkZeus""#));
        assert!(!html.contains("Satchel"));
        assert!(!html.contains("data-satchel"));

        let html = page_with(Some("https://wallet.5day4cast.com"));
        assert!(html.contains(r#"data-satchel-url="https://wallet.5day4cast.com""#));
        let zeus = html.find(r#"id="walletLinkZeus""#).unwrap();
        let pay = html.find(r#"id="walletLinkSatchel""#).unwrap();
        assert!(pay > zeus && html[zeus..pay].contains("Pay with Zeus"));
        assert!(html[pay..].contains("Pay with Satchel"));
        let menu = html.find(r#"id="logoutContainer""#).unwrap();
        let open = html.find(r#"id="openSatchelNavClick""#).unwrap();
        assert!(open > menu);
        assert!(html.contains(r#"href="https://wallet.5day4cast.com/wallet""#));
        assert!(html.contains(r#"data-satchel-next="/wallet""#));
        assert!(html[open..].contains("Open Satchel"));
    }

    #[test]
    fn pages_name_their_request_and_telemetry_only_when_on() {
        let html = page();
        assert!(!html.contains(r#"name="request-id""#));
        assert!(!html.contains(r#"name="telemetry""#));

        let context = crate::api::request_context::RequestContext {
            rid: "0192f1e0-aaaa-7bbb-8ccc-123456789abc".into(),
            prid: None,
            sid: None,
            ip: std::net::Ipv4Addr::LOCALHOST.into(),
            user: Default::default(),
        };
        let html = request_context::REQUEST_CONTEXT.sync_scope(context, page);
        assert!(html.contains(
            r#"<meta name="request-id" content="0192f1e0-aaaa-7bbb-8ccc-123456789abc">"#
        ));
    }

    #[test]
    fn the_footer_links_to_feedback_beside_contact_only_when_it_is_on() {
        let html = page();
        assert!(!html.contains("data-feedback-open"));
        assert!(!html.contains(r#"id="feedbackModal""#));

        let config = PageConfig {
            title: "Fantasy Weather",
            api_base: "https://5day4cast.com",
            oracle_base: "https://4casttruth.win",
            network: "signet",
            wasm_version: "abc123",
            recovery: RecoveryHelp::FileAndRelays,
            satchel_url: None,
            feedback: true,
            mainnet_signup: true,
        };
        let html = base(&config, html! { p { "content" } }).into_string();
        let footer = &html[html.find(r#"<footer class="site-footer">"#).unwrap()..];
        let contact = footer.find("mailto:").unwrap();
        let link = footer
            .find(r#"<a href="/feedback" data-feedback-open"#)
            .unwrap();
        assert!(link > contact && footer[..footer.find("</footer>").unwrap()].contains("Feedback"));
        assert!(html.contains(r#"id="feedbackModal""#));
        // Still no inline style.
        assert!(!html.contains(" style="));
    }

    #[test]
    fn every_page_has_the_contact_email_in_its_footer() {
        let html = page();
        let footer = html.find(r#"<footer class="site-footer">"#).unwrap();
        assert!(html[footer..].contains(r#"href="mailto:5day4cast@protonmail.com""#));
    }

    #[test]
    fn the_page_has_no_inline_script_or_style() {
        let html = page();
        for script in html.split("<script").skip(1) {
            assert!(
                script.starts_with(" src=\"/assets/"),
                "inline script: <script{script}"
            );
        }
        assert!(!html.contains("<style"));
        assert!(!html.contains(" style="));
        assert!(!html.contains(" onclick="));
        assert!(!html.contains("<base"), "base-uri 'none' forbids it");
    }

    #[test]
    fn htmx_sends_only_to_this_site_and_swaps_no_errors() {
        let config: serde_json::Value = serde_json::from_str(HTMX_CONFIG).unwrap();
        assert_eq!(config["mode"], "same-origin");
        assert_eq!(config["implicitInheritance"], false);
        assert_eq!(config["defaultTimeout"], 10_000);
        assert_eq!(
            config["noSwap"],
            serde_json::json!([204, 304, "4xx", "5xx"])
        );
        assert_eq!(config["includeIndicatorCSS"], false);
        assert!(page().contains(
            r#"<meta name="htmx-config" content="{&quot;mode&quot;:&quot;same-origin&quot;"#
        ));
    }

    /// On a touch screen Copy takes a tap at least 44 px tall and wide, centred on it: an inset
    /// around the button left 42. It is 46 px, since 44 between whole pixels covered only 43.
    #[test]
    fn copy_takes_a_44_px_tap_on_a_touch_screen() {
        use crate::templates::css_check::{rule, value};
        let base = include_str!("base.css");
        let touch = &base[base.find("@media (pointer: coarse)").unwrap()..];
        let tap = rule(touch, ".copy-button::before");
        assert_eq!(value(tap, "height"), "46px");
        assert_eq!(value(tap, "width"), "max(calc(100% + 8px), 46px)");
    }

    /// Phone tables shrink small buttons to 32 px (styles.css); on a touch screen Picks keeps
    /// 44 px with a selector that outranks that one.
    #[test]
    fn picks_stays_44_px_tall_in_a_phone_table() {
        use crate::templates::css_check::{rule, value};
        let site = include_str!("../../static/styles.css");
        let base = include_str!("base.css");
        assert_eq!(
            value(
                rule(site, ".table:not(.is-card-mobile) .button.is-small"),
                "min-height"
            ),
            "32px"
        );
        let touch = &base[base.find("@media (pointer: coarse)").unwrap()..];
        assert_eq!(
            value(
                rule(
                    touch,
                    ".table:not(.is-card-mobile) .button.is-small.picks-button"
                ),
                "min-height"
            ),
            "44px"
        );
    }
}
