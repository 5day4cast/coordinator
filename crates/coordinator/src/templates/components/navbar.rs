use maud::{html, Markup};

/// Public UI navigation bar
///
/// Uses HTMX for navigation with `data-requires-auth` for protected routes.
/// The crypto_bridge.js intercepts these requests to add auth headers.
pub fn navbar() -> Markup {
    html! {
        div class="container" {
            nav class="navbar is-light" role="navigation" aria-label="main navigation" {
                div class="container" {
                    // Navbar Brand (hamburger for mobile)
                    div class="navbar-brand" {
                        a role="button" class="navbar-burger" aria-label="menu"
                          aria-expanded="false" data-target="navMenu" {
                            span aria-hidden="true" {}
                            span aria-hidden="true" {}
                            span aria-hidden="true" {}
                        }
                    }

                    // Navbar Menu
                    div id="navMenu" class="navbar-menu" {
                        // Left side - navigation items
                        div class="navbar-start" {
                            a href="/competitions"
                              class="navbar-item"
                              id="allCompetitionsNavClick"
                              hx-get="/competitions"
                              hx-target="#main-content"
                              hx-push-url="true" {
                                span { "Competitions" }
                            }

                            a href="/entries"
                              class="navbar-item"
                              id="allEntriesNavClick"
                              hx-get="/entries"
                              hx-target="#main-content"
                              hx-push-url="true"
                              data-requires-auth="true" {
                                span { "Entries" }
                            }

                            a href="/payouts"
                              class="navbar-item"
                              id="payoutsNavClick"
                              hx-get="/payouts"
                              hx-target="#main-content"
                              hx-push-url="true"
                              data-requires-auth="true" {
                                span { "Payouts" }
                            }
                        }

                        // Right side - auth buttons
                        div class="navbar-end" {
                            // Auth buttons (shown when logged out)
                            div class="navbar-item" id="authButtons" {
                                div class="buttons" {
                                    a class="button is-primary" id="loginNavClick" {
                                        span { "Log in" }
                                    }
                                    a class="button is-light" id="registerNavClick" {
                                        span { "Sign up" }
                                    }
                                }
                            }

                            // Logout button (shown when logged in, hidden by default)
                            div class="navbar-item is-hidden" id="logoutContainer" {
                                div class="buttons" {
                                    a href="#" class="button is-light" id="logoutNavClick" {
                                        span { "Logout" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
