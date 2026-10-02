use maud::{html, Markup, DOCTYPE};
use serde_json::json;

use crate::{
    api::admin_auth::CSRF_HEADER,
    templates::assets::{ADMIN_THEME_JS, BULMA_CSS, HTMX_JS, STYLES_CSS},
};

pub struct AdminPageConfig<'a> {
    pub title: &'a str,
    pub api_base: &'a str,
    pub oracle_base: &'a str,
    pub explorer_url: &'a str,
    pub network: &'a str,
    /// Echoed by HTMX on every request; required for cookie-session state changes.
    pub csrf_token: Option<&'a str>,
}

pub fn admin_base(config: &AdminPageConfig, content: Markup) -> Markup {
    let csrf_headers = config
        .csrf_token
        .map(|token| json!({ CSRF_HEADER: token }).to_string());
    html! {
        (DOCTYPE)
        html lang="en" data-theme="dark" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (config.title) }

                script src=(ADMIN_THEME_JS.url) {}
                link rel="stylesheet" href=(BULMA_CSS.url);
                link rel="stylesheet" href=(STYLES_CSS.url);

                script src=(HTMX_JS.url) defer {}

                style {
                    r#"
                        pre { background-color: #f4f4f4; padding: 10px; border-radius: 5px; overflow-x: auto; white-space: pre-wrap; font-family: monospace; outline: none; }
                        .invalid { border: 2px solid red; }
                        .is-hidden { display: none; }
                        .send-form { max-width: 500px; }
                        .notification { transition: all 0.3s ease-in-out; }
                        .notification.is-hidden { opacity: 0; transform: translateY(-10px); }
                    "#
                }
            }
            body.admin-shell data-api-base=(config.api_base)
                 data-oracle-base=(config.oracle_base)
                 data-explorer-url=(config.explorer_url)
                 data-network=(config.network)
                 // Every htmx request on the page carries the CSRF token.
                 hx-headers:inherited=[csrf_headers] {
                nav.admin-nav aria-label="Administration" {
                    strong { "5day4cast / Admin" }
                    a href="/admin/operations" aria-current=[config.title.starts_with("Competition").then_some("page")] { "Operations" }
                    a href="/admin/funds" aria-current=[config.title.starts_with("Funds").then_some("page")] { "Find customer funds" }
                    a href="/admin/competition" aria-current=[config.title.starts_with("Weather").then_some("page")] { "Discover games" }
                    a href="/admin/wallet" aria-current=[config.title.starts_with("Node").then_some("page")] { "Node & wallets" }
                    label.admin-appearance hidden { "Appearance"
                        select id="admin-theme" {
                            option value="light" { "Light" }
                            option value="dark" selected { "Dark" }
                        }
                    }
                }

                div id="admin-content" {
                    (content)
                }
            }
        }
    }
}
