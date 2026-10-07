//! Messages players send from the feedback form, and the alerts the operator gets for them.
//!
//! The form is public, so each message passes these checks before it is stored:
//! - a proof of work from its own fixed-difficulty [`SignupPow`], about a second of the
//!   browser's time, solved while the visitor types;
//! - limits per browser session, per client address (generous, since a conference may share
//!   one) and for the whole site ([`FeedbackLimits`]);
//! - a hidden `website` field people never see: a bot that fills it is thanked and ignored;
//! - an exact repeat of a message stored in the last day is thanked and ignored too.
//!
//! Text is cleaned of control characters and capped ([`clean_text`]). Pages render it only
//! as escaped text.

mod limits;
mod store;

pub use limits::{FeedbackLimits, LimitRule, Limited};
pub use store::{FeedbackRow, FeedbackStatus, FeedbackStore, NewFeedback};

use crate::domain::SignupPow;

/// Characters a message may have.
pub const MAX_MESSAGE_CHARS: usize = 2000;
/// Characters kept of the optional contact.
pub const MAX_CONTACT_CHARS: usize = 200;
/// Characters kept of the user agent and the page path.
pub const MAX_META_CHARS: usize = 200;
/// Leading zero bits the feedback proof of work needs: about a second in a browser. Tests
/// solve an easier one.
pub const FEEDBACK_POW_BITS: u8 = if cfg!(test) { 8 } else { 18 };
/// A repeat of a message stored this recently is dropped.
pub const DUPLICATE_WINDOW_SECS: i64 = 24 * 3600;

/// The feedback form's state: whether it is on, where messages are kept, its proof of work
/// and its limits.
pub struct Feedback {
    pub enabled: bool,
    pub store: FeedbackStore,
    pub pow: SignupPow,
    pub limits: FeedbackLimits,
}

impl Feedback {
    pub fn new(enabled: bool, store: FeedbackStore) -> Self {
        Self {
            enabled,
            store,
            pow: SignupPow::fixed(FEEDBACK_POW_BITS),
            limits: FeedbackLimits::default(),
        }
    }
}

/// `text` without control characters (newlines and tabs in a message are kept when
/// `multiline`), trimmed, and cut to `max` characters. `None` when nothing is left.
pub fn clean_text(text: &str, max: usize, multiline: bool) -> Option<String> {
    let cleaned: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| !c.is_control() || (multiline && matches!(c, '\n' | '\t')))
        .filter(|c| !is_invisible_format(*c))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(max).collect())
}

/// Bidirectional overrides and zero-width characters, which can make text read differently
/// from what it says.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// Whether a message is longer than the form allows, before any cut.
pub fn too_long(message: &str) -> bool {
    message.chars().count() > MAX_MESSAGE_CHARS
}

/// The path of a page address, without its query or fragment: the page a message was sent
/// from. `None` unless it is a path on this site.
pub fn page_path(url: &str) -> Option<String> {
    let path = match url.find("://") {
        Some(scheme) => {
            let rest = &url[scheme + 3..];
            &rest[rest.find('/')?..]
        }
        None => url,
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    if !path.starts_with('/') || path.starts_with("//") {
        return None;
    }
    clean_text(path, MAX_META_CHARS, false).filter(|path| !path.contains(' '))
}

/// The first `max` characters of a message on one line, with an ellipsis when cut.
pub fn excerpt(message: &str, max: usize) -> String {
    let flat: String = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_invisible_characters_are_removed_and_text_is_capped() {
        assert_eq!(
            clean_text(
                "  hi\u{0007} there\r\nsecond\tline \u{202E}x\u{200B} ",
                2000,
                true
            )
            .as_deref(),
            Some("hi there\nsecond\tline x")
        );
        assert_eq!(
            clean_text("one\ntwo", 200, false).as_deref(),
            Some("onetwo")
        );
        assert_eq!(clean_text(" \u{0000}\n ", 10, true), None);
        let long = "é".repeat(2500);
        assert_eq!(
            clean_text(&long, MAX_MESSAGE_CHARS, true)
                .unwrap()
                .chars()
                .count(),
            MAX_MESSAGE_CHARS
        );
        assert!(too_long(&long));
        assert!(!too_long(&"a".repeat(MAX_MESSAGE_CHARS)));
    }

    #[test]
    fn the_page_is_a_path_on_this_site_without_its_query() {
        for (url, path) in [
            ("https://5day4cast.com/entries?x=1#top", Some("/entries")),
            ("http://127.0.0.1:9990/", Some("/")),
            ("/competitions/abc", Some("/competitions/abc")),
            ("https://5day4cast.com", None),
            ("//evil.example/x", None),
            ("javascript:alert(1)", None),
            ("", None),
        ] {
            assert_eq!(page_path(url).as_deref(), path, "{url}");
        }
    }

    #[test]
    fn excerpts_are_one_line_and_cut_with_an_ellipsis() {
        assert_eq!(excerpt("a\n b\tc", 140), "a b c");
        let cut = excerpt(&"x".repeat(200), 140);
        assert_eq!(cut.chars().count(), 140);
        assert!(cut.ends_with('…'));
    }
}
