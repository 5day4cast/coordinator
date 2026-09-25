//! What a fragment shows when the oracle's weather it needs is not there.
//!
//! The server waits for a fetch in flight only briefly
//! (`domain::leaderboard::FIRST_READ_WAIT`), so every request answers within
//! the site's 400 ms budget however slow the oracle is. A placeholder then
//! asks again by itself every 2 s, at most `MAX_ASKS` times, and after that
//! leaves it to the player's Retry. A failed fetch and a server error each say
//! what happened instead of looking like a slow load.

use maud::{html, Markup};

/// Why a fragment has nothing to show yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// The oracle's weather is on its way.
    Loading,
    /// The last fetch from the oracle failed; the next is at most 30 s away.
    Unavailable,
    /// The server failed, a database error say. Sent with status 500, which
    /// elements that load these fragments swap in (`hx-status:500`).
    Failed,
}

/// How long a placeholder waits before each request of its own.
const ASK_AGAIN: &str = "load delay:2s";

/// How many times a placeholder asks by itself: 30 s of asking, longer than
/// the oracle's slowest answers.
pub const MAX_ASKS: u8 = 15;

/// The placeholder `#id` for `url`'s content, about `what` ("observations
/// and scores"). `asked`: how many times the placeholder has already asked by
/// itself (the `again` in its URL; 0 for the first render and for Retry).
pub fn placeholder(id: &str, url: &str, what: &str, pending: Pending, asked: u8) -> Markup {
    let ask = pending == Pending::Loading && asked < MAX_ASKS;
    let separator = if url.contains('?') { '&' } else { '?' };
    let next = format!("{url}{separator}again={}", asked + 1);
    html! {
        div id=(id) hx-get=[ask.then_some(&next)] hx-trigger=[ask.then_some(ASK_AGAIN)]
            hx-swap=[ask.then_some("outerHTML")] "hx-status:500"=[ask.then_some("swap:outerHTML")] {
            p class="notice" role="status" {
                @match pending {
                    Pending::Loading => {
                        @if ask { span class="spinner" aria-hidden="true" {} " " }
                        "Still loading " (what) " from the oracle…"
                    }
                    Pending::Unavailable => {
                        "The oracle's " (what) " are unavailable right now; this site asks again within 30 seconds."
                    }
                    Pending::Failed => { "Something went wrong loading the " (what) "." }
                }
                @if !ask {
                    " "
                    (retry(id, url))
                }
            }
        }
    }
}

/// A button that loads `url` into `#id` again.
pub fn retry(id: &str, url: &str) -> Markup {
    html! {
        button type="button" class="button is-small" hx-get=(url) hx-target=(format!("#{id}"))
            hx-swap="outerHTML" "hx-status:500"="swap:outerHTML" { "Retry" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "/competitions/c1/leaderboard/rows";
    const WHAT: &str = "observations and scores";

    #[test]
    fn a_slow_load_says_still_loading_and_asks_again_for_a_while() {
        let first = placeholder("scores", URL, WHAT, Pending::Loading, 0).into_string();
        assert!(first.contains("Still loading observations and scores"));
        assert!(first.contains(r#"hx-get="/competitions/c1/leaderboard/rows?again=1""#));
        assert!(first.contains(r#"hx-trigger="load delay:2s""#));
        assert!(!first.contains("Retry"));

        let later = placeholder("scores", URL, WHAT, Pending::Loading, 3).into_string();
        assert!(later.contains(r#"hx-get="/competitions/c1/leaderboard/rows?again=4""#));

        let last = placeholder("scores", URL, WHAT, Pending::Loading, MAX_ASKS).into_string();
        assert!(last.contains("Still loading"));
        assert!(!last.contains("hx-trigger"), "it stops asking by itself");
        assert!(last.contains("Retry"));
        assert!(
            last.contains(r#"hx-get="/competitions/c1/leaderboard/rows""#),
            "Retry starts over"
        );
    }

    #[test]
    fn a_failed_fetch_and_a_server_error_say_so() {
        let unavailable =
            placeholder("scores", URL, "observations", Pending::Unavailable, 0).into_string();
        assert!(unavailable.contains("unavailable right now"));
        assert!(!unavailable.contains("hx-trigger"));
        let failed = placeholder("scores", URL, "observations", Pending::Failed, 0).into_string();
        assert!(failed.contains("Something went wrong"));
        assert!(!failed.contains("unavailable"));
        assert!(failed.contains(r#"hx-status:500="swap:outerHTML""#));
    }
}
