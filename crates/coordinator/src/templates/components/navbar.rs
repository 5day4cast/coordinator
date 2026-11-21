use maud::{html, Markup};

/// The wordmark: "5day4cast" with its 4 in gold.
pub fn wordmark() -> Markup {
    html! {
        span class="wordmark" { "5day" b { "4cast" } }
    }
}

/// The thin strip that says a test network's sats are play money; mainnet has none.
pub fn network_banner(network: &str) -> Markup {
    let name = match network {
        "bitcoin" | "mainnet" => return html! {},
        "signet" => "Mutinynet",
        "regtest" => "Regtest",
        "testnet" | "testnet4" => "Testnet",
        other => other,
    };
    html! {
        div class="network-banner" role="note" {
            strong { (name) " play money" }
            span { "Test sats with no real value." }
            span class="network-banner-more" { "Never send mainnet bitcoin here." }
        }
    }
}

/// The site header, under the test network's banner so both stay in view. Its links load
/// pages with htmx; `htmx_auth.js` signs requests for account pages. On phones the same
/// links become the tab bar along the bottom (base.css). `satchel` is the Satchel wallet's
/// origin, when configured: the signed-in player's menu opens it.
pub fn navbar(network: &str, satchel: Option<&str>) -> Markup {
    html! {
        header class="site-header" {
            (network_banner(network))
            div class="site-header-inner" {
                a class="site-brand" href="/" aria-label="5day4cast lobby" { (wordmark()) }

                nav class="site-nav" aria-label="Main" {
                    (nav_link("/competitions", "allCompetitionsNavClick", "Lobby", lobby_icon()))
                    (nav_link("/entries", "allEntriesNavClick", "My entries", ticket_icon()))
                    (nav_link("/payouts", "payoutsNavClick", "Wallet", wallet_icon()))
                    (nav_link("/help", "helpNavClick", "How to play", help_icon()))
                }

                div class="site-account" {
                    // Shown when logged out.
                    div class="buttons" id="authButtons" {
                        button type="button" class="button is-small is-ghost-line" id="loginNavClick"
                               data-open-modal="loginModal" {
                            "Log in"
                        }
                        button type="button" class="button is-small is-primary" id="registerNavClick"
                               data-open-modal="registerModal" {
                            "Sign up"
                        }
                    }

                    // Shown when logged in.
                    div class="buttons is-hidden" id="logoutContainer" {
                        // Signs the player in to Satchel in its own tab (shared/satchel.js).
                        @if let Some(satchel) = satchel {
                            a class="button is-small is-ghost-line" id="openSatchelNavClick"
                              href=(format!("{satchel}/wallet")) target="_blank" rel="noopener"
                              data-satchel-next="/wallet" {
                                "Open Satchel"
                            }
                        }
                        button type="button" class="button is-small is-ghost-line" id="logoutNavClick" {
                            "Log out"
                        }
                    }
                }
            }
        }
    }
}

fn nav_link(href: &str, id: &str, label: &str, icon: Markup) -> Markup {
    html! {
        a href=(href) class="site-nav-link" id=(id)
          hx-get=(href) hx-target="#main-content" hx-push-url="true" {
            (icon)
            span { (label) }
        }
    }
}

fn icon(body: Markup) -> Markup {
    html! {
        svg class="nav-icon" viewBox="0 0 24 24" width="22" height="22" fill="none"
            stroke="currentColor" stroke-width="1.7" stroke-linecap="round"
            stroke-linejoin="round" aria-hidden="true" {
            (body)
        }
    }
}

fn lobby_icon() -> Markup {
    icon(html! {
        rect x="3.5" y="3.5" width="7" height="7" rx="1.5" {}
        rect x="13.5" y="3.5" width="7" height="7" rx="1.5" {}
        rect x="3.5" y="13.5" width="7" height="7" rx="1.5" {}
        rect x="13.5" y="13.5" width="7" height="7" rx="1.5" {}
    })
}

fn ticket_icon() -> Markup {
    icon(html! {
        path d="M3 7.5h18v3a1.75 1.75 0 0 0 0 3.5v3H3v-3a1.75 1.75 0 0 0 0-3.5z" {}
        path d="M9 7.5v9.5" stroke-dasharray="1.5 2" {}
    })
}

fn wallet_icon() -> Markup {
    icon(html! {
        rect x="3" y="6" width="18" height="13" rx="2" {}
        path d="M3 9.5h18" {}
        circle cx="16.5" cy="14" r="1.2" {}
    })
}

fn help_icon() -> Markup {
    icon(html! {
        circle cx="12" cy="12" r="8.5" {}
        path d="M9.6 9.4a2.5 2.5 0 1 1 3.4 2.3c-.6.3-1 .8-1 1.5v.6" {}
        circle cx="12" cy="16.9" r="0.4" {}
    })
}

/// GitHub icon (Octicons)
pub fn github_icon() -> Markup {
    html! {
        svg class="github-icon" height="16" width="16" viewBox="0 0 16 16" version="1.1" aria-hidden="true" {
            path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_test_networks_have_the_play_money_banner() {
        assert_eq!(network_banner("bitcoin").into_string(), "");
        let mutinynet = network_banner("signet").into_string();
        assert!(mutinynet.contains("Mutinynet play money"));
        assert!(network_banner("regtest")
            .into_string()
            .contains("Regtest play money"));
    }
}
