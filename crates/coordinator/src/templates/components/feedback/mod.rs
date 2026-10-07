//! The feedback form: a dialog opened from the footer, and the `/feedback` page that works
//! without JavaScript. The browser adds a proof of work while the visitor types
//! (feedback.js); the server adds the page, request and session ids and the signed-in key, so
//! the form carries none of them.

use maud::{html, Markup};

use crate::domain::feedback::{MAX_CONTACT_CHARS, MAX_MESSAGE_CHARS};

/// What the form shows again after a refused send.
#[derive(Clone, Debug, Default)]
pub struct FeedbackDraft<'a> {
    pub message: &'a str,
    pub contact: &'a str,
    /// One sentence on why it was not sent.
    pub error: Option<&'a str>,
}

/// The footer link. Without JavaScript it opens the `/feedback` page.
pub fn feedback_link() -> Markup {
    html! {
        a href="/feedback" data-feedback-open { "Feedback" }
    }
}

/// The dialog. Each opening starts from a fresh copy of the form in its template.
pub fn feedback_modal() -> Markup {
    html! {
        div id="feedbackModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="feedbackModalTitle" tabindex="-1" {
            div class="modal-background" {}
            div class="modal-card" {
                header class="modal-card-head" {
                    p id="feedbackModalTitle" class="modal-card-title" { "Feedback" }
                    button class="delete" aria-label="close" {}
                }
                section class="modal-card-body" data-feedback-body {}
                template id="feedbackFormTemplate" {
                    (feedback_form(&FeedbackDraft::default()))
                }
            }
        }
    }
}

/// The form, empty or as it was sent with the reason it was refused. Sent by htmx it is
/// replaced by the answer; without JavaScript it posts to `/feedback`.
pub fn feedback_form(draft: &FeedbackDraft) -> Markup {
    let count = draft.message.encode_utf16().count();
    html! {
        form class="feedback-form" method="post" action="/feedback" data-feedback
             hx-post="/api/v1/feedback" hx-target="this" hx-swap="outerHTML" {
            p { "Tell us what worked, what didn't, or what you'd like to see." }
            @if let Some(error) = draft.error {
                p class="notification is-warning" role="alert" { (error) }
            }
            div class="field" {
                label class="label" {
                    "Message"
                    textarea class="textarea" name="message" rows="5" required
                             maxlength=(MAX_MESSAGE_CHARS) data-initial-focus { (draft.message) }
                }
                p class="help" aria-live="polite" {
                    span data-feedback-count { (count) } " / " (MAX_MESSAGE_CHARS)
                }
            }
            div class="field" {
                label class="label" {
                    "Email or npub, if you'd like a reply"
                    input class="input" type="text" name="contact" autocomplete="email"
                          maxlength=(MAX_CONTACT_CHARS) value=(draft.contact);
                }
            }
            // People never see this field; bots that fill it are thanked and ignored.
            div class="feedback-website" aria-hidden="true" {
                label { "Website" input type="text" name="website" tabindex="-1" autocomplete="off"; }
            }
            input type="hidden" name="pow_challenge" value="";
            input type="hidden" name="pow_nonce" value="";
            div class="field" {
                button class="button is-primary" type="submit" { "Send" }
            }
        }
    }
}

/// What a sent message gets, stored or not.
pub fn feedback_thanks(page: bool) -> Markup {
    html! {
        div class="feedback-thanks" role="status" {
            p { strong { "Thank you." } " Your message reached the 5day4cast team." }
            @if page {
                p { a href="/competitions" { "Back to the competitions" } }
            }
        }
    }
}

/// The `/feedback` page's content.
pub fn feedback_page(form: Markup) -> Markup {
    html! {
        section class="feedback-page" {
            h1 class="title is-4" { "Feedback" }
            (form)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_has_a_honeypot_and_no_hidden_identity_fields() {
        let html = feedback_form(&FeedbackDraft::default()).into_string();
        assert!(html.contains(r#"name="message""#));
        assert!(html.contains(r#"maxlength="2000""#));
        assert!(html.contains("Email or npub, if you'd like a reply"));
        assert!(html.contains(r#"name="website""#));
        assert!(html.contains(r#"action="/feedback""#));
        assert!(html.contains(r#"hx-post="/api/v1/feedback""#));
        for field in ["page", "rid", "sid", "pubkey", "user_agent"] {
            assert!(!html.contains(&format!(r#"name="{field}""#)), "{field}");
        }
    }

    #[test]
    fn a_refused_draft_is_shown_again_as_text() {
        let html = feedback_form(&FeedbackDraft {
            message: "<script>alert(1)</script>",
            contact: "\"><b>",
            error: Some("Try again."),
        })
        .into_string();
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("\"><b>"));
        assert!(html.contains("Try again."));
        assert!(html.contains(">25</span> / 2000"));
    }

    #[test]
    fn the_thanks_say_the_message_reached_the_team() {
        assert!(feedback_thanks(false)
            .into_string()
            .contains("Your message reached the 5day4cast team."));
    }
}
