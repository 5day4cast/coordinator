use maud::{html, Markup, DOCTYPE};

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

                script src=(HTMX_JS.url) defer {}
                script type="module" src="https://unpkg.com/bitcoin-qr@1.4.1/dist/bitcoin-qr/bitcoin-qr.esm.js" {}
                script src=(APP_JS.url) defer {}
            }
            body data-api-base=(config.api_base) data-oracle-base=(config.oracle_base)
                 data-network=(config.network) data-wasm-version=(config.wasm_version) {
                (navbar())

                section class="section pt-3" {
                    div class="container" {
                        div id="main-content" {
                            (content)
                        }
                    }
                }

                (auth_modals())
            }
        }
    }
}
