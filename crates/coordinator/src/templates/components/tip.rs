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

/// A [`tip`] at the end of a line, whose bubble grows leftwards so it stays inside a dialog.
pub fn tip_end(text: &str) -> Markup {
    tip_with_class("tip tip-end", text)
}

fn tip_with_class(class: &str, text: &str) -> Markup {
    html! {
        span class=(class) tabindex="0" role="note" aria-label=(text) data-tip=(text) { "?" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::css_check::{contrast, rgba, rule, value};

    #[test]
    fn a_tip_near_the_start_of_a_line_is_marked_to_grow_rightwards() {
        assert!(tip("Why")
            .into_string()
            .starts_with(r#"<span class="tip" "#));
        assert!(tip_start("Why")
            .into_string()
            .starts_with(r#"<span class="tip tip-start" "#));
        assert!(tip_end("Why")
            .into_string()
            .starts_with(r#"<span class="tip tip-end" "#));
    }

    const CSS: &str = include_str!("tip.css");

    /// The bubble's colours in each theme, from tip.css: `(background, text)`.
    fn bubble_colours(theme: &str) -> (&'static str, &'static str) {
        let block = rule(CSS, &format!(r#"[data-theme="{theme}"]"#));
        (value(block, "--tip-bg"), value(block, "--tip-text"))
    }

    /// Bulma's dark theme keeps --bulma-scheme-invert dark, which once put dark text on a dark
    /// bubble; each theme sets its own, readable at WCAG AA.
    #[test]
    fn the_bubble_reads_at_4_5_to_1_in_both_themes() {
        for theme in ["light", "dark"] {
            let (background, text) = bubble_colours(theme);
            let ratio = contrast(rgba(background).0, rgba(text).0);
            assert!(
                ratio >= 4.5,
                "{theme}: {text} on {background} is {ratio:.1}:1"
            );
        }
        assert!(
            !CSS.contains("var(--bulma-scheme-invert)"),
            "the bubble uses its own colours"
        );
    }

    /// A hidden bubble is not laid out, so wherever it would sit it never widens a page or a
    /// dialog; `visibility: hidden` once made a phone page 438 px wide.
    #[test]
    fn a_hidden_bubble_takes_no_room() {
        assert_eq!(value(rule(CSS, "[data-tip]::after"), "display"), "none");
        assert!(!CSS.contains("visibility"));
    }
}
