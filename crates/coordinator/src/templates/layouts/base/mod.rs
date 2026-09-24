use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::templates::{
    assets::{APP_JS, HTMX_JS, STYLES_CSS},
    components::{auth_modals, navbar},
};

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
                base href="/";
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (config.title) }

                link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bulma@1.0.2/css/bulma.min.css";
                link rel="stylesheet" href=(STYLES_CSS.url);

                // The saved or system theme, applied before first paint.
                script {
                    (PreEscaped(r#"document.documentElement.dataset.theme=localStorage.getItem("fantasy-weather-theme")||(matchMedia("(prefers-color-scheme: dark)").matches?"dark":"light")"#))
                }
                script src=(HTMX_JS.url) defer {}
                script src=(APP_JS.url) defer {}
            }
            body data-api-base=(config.api_base) data-oracle-base=(config.oracle_base)
                 data-network=(config.network) data-wasm-version=(config.wasm_version) {
                // Shown while a navigation is loading (see page.css).
                div class="page-loading" aria-hidden="true" {}
                (navbar())

                section class="section pt-3" {
                    // History snapshots cover the page content only; the
                    // navbar and dialogs keep their state and listeners.
                    main class="container" id="main-content" hx-history-elt {
                        (content)
                    }
                }

                (auth_modals())
            }
        }
    }
}
