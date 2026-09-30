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

    /// The bubble's colours in each theme, from tip.css: `(background, text)`.
    fn bubble_colours(theme: &str) -> (&'static str, &'static str) {
        const CSS: &str = include_str!("tip.css");
        let block = CSS
            .split(&format!(r#"[data-theme="{theme}"]"#))
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap_or_else(|| panic!("tip.css sets no colours for the {theme} theme"));
        let value = |name: &str| {
            block
                .split(&format!("{name}:"))
                .nth(1)
                .and_then(|rest| rest.split(';').next())
                .map(str::trim)
                .unwrap_or_else(|| panic!("no {name} in the {theme} theme"))
        };
        (value("--tip-bg"), value("--tip-text"))
    }

    /// WCAG relative luminance of `#rrggbb`.
    fn luminance(hex: &str) -> f64 {
        let channel = |at: usize| {
            let c = f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap()) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
    }

    /// Bulma's dark theme keeps --bulma-scheme-invert dark, which once put dark text on a dark
    /// bubble; each theme sets its own, readable at WCAG AA.
    #[test]
    fn the_bubble_reads_at_4_5_to_1_in_both_themes() {
        for theme in ["light", "dark"] {
            let (background, text) = bubble_colours(theme);
            let (a, b) = (luminance(background), luminance(text));
            let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(
                ratio >= 4.5,
                "{theme}: {text} on {background} is {ratio:.1}:1"
            );
        }
        let css = include_str!("tip.css");
        assert!(
            !css.contains("var(--bulma-scheme-invert)"),
            "the bubble uses its own colours"
        );
    }
}
