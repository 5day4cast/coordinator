use maud::{html, Markup, DOCTYPE};

/// Configuration for admin pages
pub struct AdminPageConfig<'a> {
    pub title: &'a str,
    pub api_base: &'a str,
    pub oracle_base: &'a str,
    pub esplora_url: &'a str,
}

/// Base layout for the admin UI
///
/// No WASM needed - pure HTMX for all interactivity
pub fn admin_base(config: &AdminPageConfig, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                base href=".";
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (config.title) }

                // Bulma CSS
                link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bulma@0.9.4/css/bulma.min.css";
                link rel="stylesheet" href="/ui/styles.css";

                // HTMX - that's all we need!
                script src="https://unpkg.com/htmx.org@1.9.10" {}

                // Custom admin styles
                style {
                    r#"
                        pre {
                            background-color: #f4f4f4;
                            padding: 10px;
                            border-radius: 5px;
                            overflow-x: auto;
                            white-space: pre-wrap;
                            font-family: monospace;
                            outline: none;
                        }
                        .invalid {
                            border: 2px solid red;
                        }
                        .is-hidden {
                            display: none;
                        }
                        .send-form {
                            max-width: 500px;
                        }
                        .notification {
                            transition: all 0.3s ease-in-out;
                        }
                        .notification.is-hidden {
                            opacity: 0;
                            transform: translateY(-10px);
                        }
                    "#
                }
            }
            body {
                // Global config
                script {
                    (format!(r#"
                        const API_BASE = "{}";
                        const ORACLE_BASE = "{}";
                        const ESPLORA_URL = "{}";
                    "#, config.api_base, config.oracle_base, config.esplora_url))
                }

                // Tab navigation
                div class="tabs is-centered" {
                    ul {
                        li class="is-active"
                           hx-get="/admin/competition"
                           hx-target="#admin-content"
                           hx-swap="innerHTML"
                           hx-push-url="true" {
                            a { "Competition" }
                        }
                        li hx-get="/admin/wallet"
                           hx-target="#admin-content"
                           hx-swap="innerHTML"
                           hx-push-url="true" {
                            a { "Wallet" }
                        }
                    }
                }

                // Main content area
                div id="admin-content" {
                    (content)
                }

                // Tab switching script (minimal JS for active state)
                script {
                    r#"
                        document.querySelectorAll('.tabs li').forEach(tab => {
                            tab.addEventListener('htmx:afterRequest', function() {
                                document.querySelectorAll('.tabs li').forEach(t => t.classList.remove('is-active'));
                                this.classList.add('is-active');
                            });
                        });
                    "#
                }
            }
        }
    }
}
