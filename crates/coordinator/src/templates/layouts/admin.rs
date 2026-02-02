use maud::{html, Markup, PreEscaped, DOCTYPE};

pub struct AdminPageConfig<'a> {
    pub title: &'a str,
    pub api_base: &'a str,
    pub oracle_base: &'a str,
    pub esplora_url: &'a str,
    pub network: &'a str,
}

pub fn admin_base(config: &AdminPageConfig, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (config.title) }

                link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bulma@1.0.2/css/bulma.min.css";
                link rel="stylesheet" href="/ui/styles.css";

                script src="https://unpkg.com/htmx.org@1.9.10" {}

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
            body data-api-base=(config.api_base)
                 data-oracle-base=(config.oracle_base)
                 data-esplora-url=(config.esplora_url)
                 data-network=(config.network) {
                script {
                    "const API_BASE = document.body.dataset.apiBase;
                     const ORACLE_BASE = document.body.dataset.oracleBase;
                     const ESPLORA_URL = document.body.dataset.esploraUrl;"
                }

                div class="tabs is-centered" {
                    ul {
                        li class="is-active" hx-get="/admin/competition" hx-target="#admin-content" hx-swap="innerHTML" hx-push-url="true" {
                            a { "Competition" }
                        }
                        li hx-get="/admin/wallet" hx-target="#admin-content" hx-swap="innerHTML" hx-push-url="true" {
                            a { "Wallet" }
                        }
                    }
                }

                div id="admin-content" {
                    (content)
                }

                script {
                    (PreEscaped(r#"
                        document.querySelectorAll('.tabs li').forEach(tab => {
                            tab.addEventListener('htmx:afterRequest', function() {
                                document.querySelectorAll('.tabs li').forEach(t => t.classList.remove('is-active'));
                                this.classList.add('is-active');
                            });
                        });
                    "#))
                }
            }
        }
    }
}
