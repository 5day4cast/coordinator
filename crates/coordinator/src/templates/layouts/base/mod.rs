use maud::{html, Markup, DOCTYPE};

use crate::templates::components::{auth_modals, navbar};

pub struct PageConfig<'a> {
    pub title: &'a str,
    pub api_base: &'a str,
    pub oracle_base: &'a str,
    pub network: &'a str,
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
                link rel="stylesheet" href="/ui/styles.css";

                script src="https://unpkg.com/htmx.org@1.9.10" {}
                script src="/ui/bolt11.min.js" {}
                script type="module" src="https://unpkg.com/bitcoin-qr@1.4.1/dist/bitcoin-qr/bitcoin-qr.esm.js" {}
            }
            body data-api-base=(config.api_base) data-oracle-base=(config.oracle_base) data-network=(config.network) {
                (navbar())

                section class="section pt-3" {
                    div class="container" {
                        div id="main-content" {
                            (content)
                        }
                    }
                }

                (auth_modals())

                // Inline burger setup so it works even if WASM/app bundle fails to load
                script {
                    (maud::PreEscaped(r#"
                    document.addEventListener('DOMContentLoaded', function() {
                        var burgers = document.querySelectorAll('.navbar-burger');
                        burgers.forEach(function(el) {
                            el.addEventListener('click', function() {
                                var target = document.getElementById(el.dataset.target);
                                if (!target) return;
                                el.classList.toggle('is-active');
                                target.classList.toggle('is-active');
                                el.setAttribute('aria-expanded', el.classList.contains('is-active'));
                            });
                        });
                        document.addEventListener('click', function(e) {
                            if (!e.target.closest('.navbar')) {
                                var menu = document.querySelector('.navbar-menu.is-active');
                                var burger = document.querySelector('.navbar-burger.is-active');
                                if (menu) menu.classList.remove('is-active');
                                if (burger) {
                                    burger.classList.remove('is-active');
                                    burger.setAttribute('aria-expanded', 'false');
                                }
                            }
                        });
                    });
                    "#))
                }

                // loader.js handles WASM init and loads the bundled app
                script type="module" src="/ui/loader.js" {}
            }
        }
    }
}
