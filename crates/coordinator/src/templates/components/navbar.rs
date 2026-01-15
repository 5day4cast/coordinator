use maud::{html, Markup};

/// Public UI navigation bar
///
/// Uses HTMX for navigation with `data-requires-auth` for protected routes.
/// The crypto_bridge.js intercepts these requests to add auth headers.
/// Updated for Bulma v1 - uses 4 spans in burger for proper animation.
pub fn navbar() -> Markup {
    html! {
        nav class="navbar is-light" role="navigation" aria-label="main navigation" {
            div class="container" {
                // Navbar Brand (hamburger for mobile)
                div class="navbar-brand" {
                    a role="button" class="navbar-burger" aria-label="menu"
                      aria-expanded="false" data-target="navMenu" {
                        // Bulma v1 requires 4 spans for the animated burger
                        span aria-hidden="true" {}
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
                            "Competitions"
                        }

                        a href="/entries"
                          class="navbar-item"
                          id="allEntriesNavClick"
                          hx-get="/entries"
                          hx-target="#main-content"
                          hx-push-url="true"
                          data-requires-auth="true" {
                            "Entries"
                        }

                        a href="/payouts"
                          class="navbar-item"
                          id="payoutsNavClick"
                          hx-get="/payouts"
                          hx-target="#main-content"
                          hx-push-url="true"
                          data-requires-auth="true" {
                            "Payouts"
                        }
                    }

                    // Right side - auth buttons and theme toggle
                    div class="navbar-end" {
                        // Theme toggle button
                        div class="navbar-item" {
                            button class="button is-small theme-toggle" id="themeToggle"
                                   aria-label="Toggle dark mode" title="Toggle dark/light mode" {
                                // Sun icon (shown in dark mode)
                                span class="icon theme-icon-light" {
                                    (sun_icon())
                                }
                                // Moon icon (shown in light mode)
                                span class="icon theme-icon-dark" {
                                    (moon_icon())
                                }
                            }
                        }

                        // Auth buttons (shown when logged out)
                        div class="navbar-item" id="authButtons" {
                            div class="buttons" {
                                a class="button is-primary" id="loginNavClick" {
                                    "Log in"
                                }
                                a class="button is-light" id="registerNavClick" {
                                    "Sign up"
                                }
                            }
                        }

                        // Logout button (shown when logged in, hidden by default)
                        div class="navbar-item is-hidden" id="logoutContainer" {
                            div class="buttons" {
                                a href="#" class="button is-light" id="logoutNavClick" {
                                    "Logout"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Sun icon for light mode indicator (Heroicons)
fn sun_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24"
            stroke-width="1.5" stroke="currentColor" width="20" height="20" {
            path stroke-linecap="round" stroke-linejoin="round"
                 d="M12 3v2.25m6.364.386-1.591 1.591M21 12h-2.25m-.386 6.364-1.591-1.591M12 18.75V21m-4.773-4.227-1.591 1.591M5.25 12H3m4.227-4.773L5.636 5.636M15.75 12a3.75 3.75 0 1 1-7.5 0 3.75 3.75 0 0 1 7.5 0Z" {}
        }
    }
}

/// Moon icon for dark mode indicator (Heroicons)
fn moon_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24"
            stroke-width="1.5" stroke="currentColor" width="20" height="20" {
            path stroke-linecap="round" stroke-linejoin="round"
                 d="M21.752 15.002A9.72 9.72 0 0 1 18 15.75c-5.385 0-9.75-4.365-9.75-9.75 0-1.33.266-2.597.748-3.752A9.753 9.753 0 0 0 3 11.25C3 16.635 7.365 21 12.75 21a9.753 9.753 0 0 0 9.002-5.998Z" {}
        }
    }
}
