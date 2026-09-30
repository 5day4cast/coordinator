use maud::{html, Markup};

/// A small "?" that shows `text` on hover, and on tap or keyboard focus, so phones get it too.
/// CSS alone (tip.css); the text is also its accessible name.
pub fn tip(text: &str) -> Markup {
    tip_with_class("tip", text)
}

/// A [`tip`] near the start of a line, whose bubble grows rightwards so a phone shows all of it.
pub fn tip_start(text: &str) -> Markup {
    tip_with_class("tip tip-start", text)
}

fn tip_with_class(class: &str, text: &str) -> Markup {
    html! {
        span class=(class) tabindex="0" role="note" aria-label=(text) data-tip=(text) { "?" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tip_near_the_start_of_a_line_is_marked_to_grow_rightwards() {
        assert!(tip("Why")
            .into_string()
            .starts_with(r#"<span class="tip" "#));
        assert!(tip_start("Why")
            .into_string()
            .starts_with(r#"<span class="tip tip-start" "#));
    }
}
