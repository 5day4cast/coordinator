//! For tests of the site's CSS: find a rule, read a colour from it, and measure WCAG contrast.

/// The declarations of the first rule whose selector (or one line of its selector list) is
/// exactly `selector`.
pub fn rule<'a>(css: &'a str, selector: &str) -> &'a str {
    let mut at = 0;
    for line in css.split_inclusive('\n') {
        at += line.len();
        let trimmed = line.trim();
        let head = trimmed.trim_end_matches(['{', ',']).trim_end();
        if head == selector && (trimmed.ends_with('{') || trimmed.ends_with(',')) {
            let rest = &css[at - line.len()..];
            let open = rest.find('{').expect("a rule has a body") + 1;
            let close = rest[open..].find('}').expect("a rule ends");
            return &rest[open..open + close];
        }
    }
    panic!("no rule for {selector}")
}

/// The value of `property` in a rule's declarations.
pub fn value<'a>(declarations: &'a str, property: &str) -> &'a str {
    declarations
        .split(';')
        .map(|declaration| declaration.rsplit("*/").next().unwrap_or_default())
        .filter_map(|declaration| declaration.split_once(':'))
        .find(|(name, _)| name.trim() == property)
        .map(|(_, value)| value.trim())
        .unwrap_or_else(|| panic!("no {property} in {declarations}"))
}

/// `hsl(h, s%, l%)` or `hsla(h, s%, l%, a)` as `(h, s, l, a)`, with s and l from 0 to 1.
pub fn hsl(colour: &str) -> (f64, f64, f64, f64) {
    let inner = colour
        .strip_prefix("hsla(")
        .or_else(|| colour.strip_prefix("hsl("))
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("not hsl: {colour}"));
    let parts: Vec<f64> = inner
        .split(',')
        .map(|part| part.trim().trim_end_matches('%').parse().unwrap())
        .collect();
    (
        parts[0],
        parts[1] / 100.0,
        parts[2] / 100.0,
        parts.get(3).copied().unwrap_or(1.0),
    )
}

/// `#rrggbb`, `hsl(…)` or `hsla(…)` as sRGB from 0 to 1, and its alpha.
pub fn rgba(colour: &str) -> ([f64; 3], f64) {
    if let Some(hex) = colour.strip_prefix('#') {
        let channel =
            |at: usize| f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap()) / 255.0;
        return ([channel(0), channel(2), channel(4)], 1.0);
    }
    let (h, s, l, a) = hsl(colour);
    let chroma = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let channel = |n: f64| {
        let k = (n + h / 30.0) % 12.0;
        l - chroma / 2.0 * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    ([channel(0.0), channel(8.0), channel(4.0)], a)
}

/// `colour` painted over the opaque `background`.
pub fn over(colour: &str, background: [f64; 3]) -> [f64; 3] {
    let (rgb, alpha) = rgba(colour);
    [0, 1, 2].map(|i| alpha * rgb[i] + (1.0 - alpha) * background[i])
}

/// WCAG contrast ratio of two opaque colours.
pub fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let luminance = |rgb: [f64; 3]| {
        let [r, g, b] = rgb.map(|c| {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[test]
fn contrast_matches_known_pairs() {
    let white = rgba("#ffffff").0;
    assert!((contrast(white, rgba("#000000").0) - 21.0).abs() < 0.01);
    assert!((contrast(white, rgba("#767676").0) - 4.54).abs() < 0.01);
    let red = rgba("hsl(0, 100%, 50%)").0;
    assert!((red[0] - 1.0).abs() < 1e-9 && red[1].abs() < 1e-9 && red[2].abs() < 1e-9);
}
