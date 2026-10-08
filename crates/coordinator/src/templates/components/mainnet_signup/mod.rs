use crate::domain::mainnet_signup::MAX_EMAIL_BYTES;
use maud::{html, Markup};

pub fn mainnet_signup_link() -> Markup {
    html! { a href="/mainnet-signup" data-mainnet-signup-open { "Mainnet signup" } }
}

pub fn mainnet_signup_modal() -> Markup {
    html! {
        div id="mainnetSignupModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="mainnetSignupModalTitle" tabindex="-1" {
            div class="modal-background" {}
            div class="modal-card" {
                header class="modal-card-head" {
                    p id="mainnetSignupModalTitle" class="modal-card-title" { "Mainnet signup" }
                    button class="delete" aria-label="close" {}
                }
                section class="modal-card-body" data-mainnet-signup-body {}
                template id="mainnetSignupFormTemplate" { (mainnet_signup_form("", None)) }
            }
        }
    }
}

pub fn mainnet_signup_form(email: &str, error: Option<&str>) -> Markup {
    html! {
        form class="feedback-form" method="post" action="/mainnet-signup" data-mainnet-signup
             data-pow-path="/api/v1/mainnet-signup/challenge"
             hx-post="/api/v1/mainnet-signup" hx-target="this" hx-swap="outerHTML" {
            p class="mb-4" { "Get an email when 5day4cast goes live on mainnet." }
            @if let Some(error) = error { p class="notification is-danger is-light" role="alert" { (error) } }
            div class="field" {
                label class="label" { "Email address"
                    input class="input" type="email" name="email" autocomplete="email" inputmode="email"
                        maxlength=(MAX_EMAIL_BYTES) required data-initial-focus value=(email);
                }
            }
            p class="help mb-4" { "By signing up, you agree to receive an email about our mainnet launch. We'll use your email only for this announcement." }
            div class="feedback-website" aria-hidden="true" {
                label { "Website" input type="text" name="website" tabindex="-1" autocomplete="off"; }
            }
            input type="hidden" name="pow_challenge" value="";
            input type="hidden" name="pow_nonce" value="";
            button class="button is-primary" type="submit" { "Notify me" }
        }
    }
}

pub fn mainnet_signup_thanks(full_page: bool) -> Markup {
    html! {
        div role="status" {
            p { strong { "You're on the list." } " We'll email you when 5day4cast goes live on mainnet." }
            @if full_page { p class="mt-4" { a href="/competitions" { "Back to the competitions" } } }
        }
    }
}

pub fn mainnet_signup_page(content: Markup) -> Markup {
    html! { section class="feedback-page" { h1 class="title is-4" { "Mainnet signup" } (content) } }
}
