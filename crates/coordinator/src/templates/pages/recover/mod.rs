//! The recovery page: a player's nsec, and optionally their recovery file, are all it needs to
//! find their entries and claim them without the coordinator. The page is a shell around
//! `static/recover.js` and the WASM module; `standalone.html` is the same page for hosting
//! anywhere once the coordinator is gone (`scripts/build-recover-page.sh`).

use maud::{html, Markup, DOCTYPE};

use crate::templates::assets::{BULMA_CSS, RECOVER_JS, STYLES_CSS, THEME_JS};

pub struct RecoverConfig<'a> {
    pub network: &'a str,
    pub oracle_base: &'a str,
    /// Hash of the WASM package on disk; versions its URLs so browsers can cache it.
    pub wasm_version: &'a str,
}

pub fn recover_page(config: &RecoverConfig) -> Markup {
    let query = if config.wasm_version.is_empty() {
        String::new()
    } else {
        format!("?v={}", config.wasm_version)
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "Recover your entries - Fantasy Weather" }
                link rel="stylesheet" href=(BULMA_CSS.url);
                link rel="stylesheet" href=(STYLES_CSS.url);
                script src=(THEME_JS.url) {}
                script src=(RECOVER_JS.url) defer {}
            }
            body data-network=(config.network) data-oracle=(config.oracle_base)
                 data-wasm-glue=(format!("/ui/pkg/coordinator_wasm.js{query}"))
                 data-wasm-module=(format!("/ui/pkg/coordinator_wasm_bg.wasm{query}"))
                 data-info-url="/api/v1/recovery/info" data-telemetry="off" {
                section class="section" {
                    main class="container recover-page" {
                        a class="back-link" href="/competitions" { "← All competitions" }
                        (recover_form())
                    }
                }
            }
        }
    }
}

/// The form and results area. `standalone.html` repeats this markup; a test keeps them alike.
fn recover_form() -> Markup {
    html! {
        h1 class="title is-4" { "Recover your entries" }
        p class="content" {
            "Your nsec is all this needs: it finds your entries on Nostr relays, follows their money "
            "on chain, and builds the transactions that claim it. The recovery file from your "
            "account page saves the relay search. Your nsec stays in this page; only relay, chain "
            "and oracle requests leave it."
        }
        form id="recover-form" {
            div class="field" {
                label class="label" for="recover-nsec" { "nsec" }
                input class="input" id="recover-nsec" type="password" autocomplete="off"
                      spellcheck="false" required;
            }
            div class="field" {
                label class="label" for="recover-kit" { "Recovery file (optional)" }
                input id="recover-kit" type="file" accept=".json,application/json";
            }
            details class="recover-settings" {
                summary { "Settings" }
                div class="field" {
                    label class="label" for="recover-network" { "Network" }
                    div class="select" {
                        select id="recover-network" {
                            option value="bitcoin" { "Bitcoin" }
                            option value="signet" { "Mutinynet (signet)" }
                        }
                    }
                }
                div class="field" {
                    label class="label" for="recover-coordinator" { "Coordinator recovery key (hex)" }
                    input class="input" id="recover-coordinator" autocomplete="off" spellcheck="false";
                }
                div class="field" {
                    label class="label" for="recover-relays" { "Relays, one per line" }
                    textarea class="textarea" id="recover-relays" rows="3" spellcheck="false" {}
                }
                div class="field" {
                    label class="label" for="recover-esplora" { "Esplora API" }
                    input class="input" id="recover-esplora" placeholder="https://mempool.space/api"
                          autocomplete="off" spellcheck="false";
                }
                div class="field" {
                    label class="label" for="recover-oracle" { "Oracle" }
                    input class="input" id="recover-oracle" autocomplete="off" spellcheck="false";
                }
            }
            button class="button is-primary" id="recover-start" type="submit" { "Find my entries" }
        }
        p id="recover-status" class="mt-3" aria-live="polite" {}
        div id="recover-entries" {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> String {
        recover_page(&RecoverConfig {
            network: "signet",
            oracle_base: "https://oracle.example",
            wasm_version: "abc",
        })
        .into_string()
    }

    fn ids(html: &str) -> Vec<&str> {
        let mut ids: Vec<&str> = html
            .split(" id=\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .collect();
        ids.sort();
        ids
    }

    /// The standalone copy has the same form, so the script finds the same elements.
    #[test]
    fn standalone_page_has_the_same_form() {
        let standalone = include_str!("standalone.html");
        assert_eq!(ids(&page()), ids(standalone));
        for attribute in ["data-wasm-glue=", "data-wasm-module=", "data-network="] {
            assert!(standalone.contains(attribute));
        }
        assert!(standalone.contains("src=\"recover.js\""));
    }

    #[test]
    fn loads_only_its_own_script_and_the_versioned_module() {
        let html = page();
        assert!(html.contains("data-wasm-glue=\"/ui/pkg/coordinator_wasm.js?v=abc\""));
        assert!(html.contains(RECOVER_JS.url));
        for script in html.split("<script").skip(1) {
            assert!(
                script.starts_with(" src=\"/assets/"),
                "inline script: <script{script}"
            );
        }
        assert!(!html.contains(" style="));
        assert!(!html.contains("checkbox"));
    }
}
