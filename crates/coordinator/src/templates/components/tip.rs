use maud::{html, Markup};

/// A small "?" that shows `text` on hover, and on tap or keyboard focus, so phones get it too.
/// CSS alone (tip.css); the text is also its accessible name.
pub fn tip(text: &str) -> Markup {
    html! {
        span class="tip" tabindex="0" role="note" aria-label=(text) data-tip=(text) { "?" }
    }
}
