use maud::{html, Markup, DOCTYPE};

use crate::templates::{
    assets::{APP_JS, HTMX_JS, STYLES_CSS, THEME_JS},
    components::{auth_modals, navbar},
};

/// Bulma, from its CDN, pinned to the exact file by its hash.
pub const BULMA_CSS: &str = "https://cdn.jsdelivr.net/npm/bulma@1.0.2/css/bulma.min.css";
const BULMA_INTEGRITY: &str =
    "sha384-tl5h4XuWmVzPeVWU0x8bx0j/5iMwCBduLEgZ+2lH4Wjda+4+q3mpCww74dgAB3OX";

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

pub struct PageConfig<'a> {
    pub title: &'a str,
    pub api_base: &'a str,
    pub oracle_base: &'a str,
    pub network: &'a str,
    /// Hash of the WASM package on disk; versions its URLs so browsers can cache it.
    pub wasm_version: &'a str,
}

pub fn base(config: &PageConfig, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                meta name="htmx-config" content=(HTMX_CONFIG);
                title { (config.title) }

                link rel="stylesheet" href=(BULMA_CSS) integrity=(BULMA_INTEGRITY) crossorigin="anonymous";
                link rel="stylesheet" href=(STYLES_CSS.url);

                // The saved or system theme, applied before first paint.
                script src=(THEME_JS.url) {}
                script src=(HTMX_JS.url) defer {}
                script src=(APP_JS.url) defer {}
            }
            body data-api-base=(config.api_base) data-oracle-base=(config.oracle_base)
                 data-network=(config.network) data-wasm-version=(config.wasm_version) {
                // Shown while a navigation is loading (see base.css).
                div class="page-loading" aria-hidden="true" {}
                (navbar())

                section class="section pt-3" {
                    // Back swaps in only the page content, fetched again
                    // (see HTMX_CONFIG); the navbar and dialogs keep their
                    // state and listeners.
                    main class="container" id="main-content" hx-history-elt {
                        (content)
                    }
                }

                (auth_modals())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> String {
        let config = PageConfig {
            title: "Fantasy Weather",
            api_base: "https://5day4cast.com",
            oracle_base: "https://4casttruth.win",
            network: "signet",
            wasm_version: "abc123",
        };
        base(&config, html! { p { "content" } }).into_string()
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
}
