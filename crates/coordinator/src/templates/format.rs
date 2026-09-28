//! Formatting shared by the public pages: amounts, times, durations and IDs.

use maud::{html, Markup};
use time::{format_description::well_known::Rfc3339, macros::format_description, OffsetDateTime};

use crate::domain::leaderboard::Metric;

/// `18000` → `18,000`.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `18000` → `18,000 sats`.
pub fn sats(value: u64) -> String {
    format!("{} sats", thousands(value))
}

/// A duration in words, to the minute: `3 days`, `2 h 13 min`, `45 min`.
pub fn duration(duration: time::Duration) -> String {
    let minutes = duration.whole_minutes().max(0);
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "under a minute".to_owned(),
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (1, 0, _) => "1 day".to_owned(),
        (1, h, _) => format!("1 day {h} h"),
        (d, _, _) => format!("{d} days"),
    }
}

/// How a `<time>` element is shown once the browser converts it to local time.
#[derive(Clone, Copy)]
pub enum TimeStyle {
    /// `Sep 24, 11:44 AM`
    DateTime,
    /// `11:54 AM`, for the end of a window that starts the same day.
    Time,
}

/// A UTC time the browser rewrites in the reader's time zone (`localizeTimes`
/// in shared/page.js).
pub fn time(at: OffsetDateTime, style: TimeStyle) -> Markup {
    let utc = at.to_offset(time::UtcOffset::UTC);
    let fallback = match style {
        TimeStyle::DateTime => utc.format(format_description!(
            "[month repr:short] [day padding:none], [hour]:[minute] UTC"
        )),
        TimeStyle::Time => utc.format(format_description!("[hour]:[minute] UTC")),
    }
    .unwrap_or_default();
    let style = match style {
        TimeStyle::DateTime => "datetime",
        TimeStyle::Time => "time",
    };
    html! {
        time datetime=(utc.format(&Rfc3339).unwrap_or_default()) data-local=(style) { (fallback) }
    }
}

/// How a metric is named and its values shown. The metric itself is the
/// domain's (`domain::leaderboard::Metric`), as the oracle scores it.
pub trait MetricText {
    fn label(self) -> &'static str;
    /// The reading so far, part-way through the window: the highest high and
    /// wind, the lowest low.
    fn so_far(self) -> &'static str;
    /// A value in the metric's unit, as the oracle reads it.
    fn value(self, value: f64) -> String;
    /// A Par range in the metric's unit: `67.4–70.2°F`.
    fn range(self, low: f64, high: f64) -> String;
    /// Which way a station's forecasts have leaned, from its Par band on the
    /// miss: `forecasts here have run ~2°F hot lately`. `None` when the band
    /// sits within half a unit of the forecast.
    fn lean(self, lower: f64, upper: f64) -> Option<String>;
}

impl MetricText for Metric {
    fn label(self) -> &'static str {
        match self {
            Metric::TempHigh => "High temperature",
            Metric::TempLow => "Low temperature",
            Metric::WindSpeed => "Wind speed",
        }
    }

    fn so_far(self) -> &'static str {
        match self {
            Metric::TempHigh => "High so far",
            Metric::TempLow => "Low so far",
            Metric::WindSpeed => "Top wind so far",
        }
    }

    fn value(self, value: f64) -> String {
        match self {
            Metric::TempHigh | Metric::TempLow => format!("{:.0}°F", value.round()),
            Metric::WindSpeed if value.fract() == 0.0 => format!("{value:.0} knots"),
            Metric::WindSpeed => format!("{value:.1} knots"),
        }
    }

    fn range(self, low: f64, high: f64) -> String {
        match self {
            Metric::TempHigh | Metric::TempLow => format!("{low:.1}–{high:.1}°F"),
            Metric::WindSpeed => format!("{low:.1}–{high:.1} knots"),
        }
    }

    fn lean(self, lower: f64, upper: f64) -> Option<String> {
        // The band's middle is the forecast's usual miss; readings below a forecast mean it ran hot.
        let miss = (lower + upper) / 2.0;
        let size = miss.abs().round();
        if size < 1.0 {
            return None;
        }
        let (amount, way) = match (self, miss < 0.0) {
            (Metric::TempHigh | Metric::TempLow, true) => (format!("{size}°F"), "hot"),
            (Metric::TempHigh | Metric::TempLow, false) => (format!("{size}°F"), "cold"),
            (Metric::WindSpeed, true) if size == 1.0 => ("1 knot".to_owned(), "high"),
            (Metric::WindSpeed, false) if size == 1.0 => ("1 knot".to_owned(), "low"),
            (Metric::WindSpeed, true) => (format!("{size} knots"), "high"),
            (Metric::WindSpeed, false) => (format!("{size} knots"), "low"),
        };
        Some(format!("forecasts here have run ~{amount} {way} lately"))
    }
}

/// How long ago something happened, in words, with the exact UTC time on
/// hover: `12 min ago`. The browser leaves it as is.
pub fn ago(at: OffsetDateTime, now: OffsetDateTime) -> Markup {
    let utc = at.to_offset(time::UtcOffset::UTC);
    let words = if now - at < time::Duration::minutes(1) {
        "just now".to_owned()
    } else {
        format!("{} ago", duration(now - at))
    };
    let exact = utc
        .format(format_description!(
            "[month repr:short] [day padding:none], [hour]:[minute] UTC"
        ))
        .unwrap_or_default();
    html! {
        time datetime=(utc.format(&Rfc3339).unwrap_or_default()) title=(exact) { (words) }
    }
}

/// A competition's observation window: `Sep 24, 11:44 – 11:54`.
pub fn window(start: OffsetDateTime, end: OffsetDateTime) -> Markup {
    let same_day =
        start.to_offset(time::UtcOffset::UTC).date() == end.to_offset(time::UtcOffset::UTC).date();
    html! {
        span class="window" {
            (time(start, TimeStyle::DateTime))
            " – "
            (time(end, if same_day { TimeStyle::Time } else { TimeStyle::DateTime }))
        }
    }
}

/// The distinct end of a UUIDv7: entries made together share its time prefix.
pub fn short_id(id: &str) -> &str {
    let tail = id.len().saturating_sub(8);
    id.get(tail..).unwrap_or(id)
}

/// A shortened ID with a button that copies the whole one.
pub fn copyable_id(id: &str) -> Markup {
    html! {
        span class="copyable-id" {
            code title=(id) { "…" (short_id(id)) }
            button type="button" class="copy-button" data-copy=(id)
                   title="Copy the full ID" aria-label="Copy the full ID" { "Copy" }
        }
    }
}

/// `npub1qqqq…wxyz`, for a player without a username.
pub fn short_npub(npub: &str) -> String {
    if npub.len() <= 16 {
        return npub.to_owned();
    }
    format!("{}…{}", &npub[..9], &npub[npub.len() - 4..])
}

/// Ordinal place: 1st, 2nd, 3rd, 4th…
pub fn ordinal(place: usize) -> String {
    let suffix = match (place % 10, place % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{place}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn amounts_have_separators_and_units() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(6300), "6,300");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(sats(18000), "18,000 sats");
    }

    #[test]
    fn durations_read_as_words() {
        assert_eq!(duration(time::Duration::seconds(20)), "under a minute");
        assert_eq!(duration(time::Duration::minutes(45)), "45 min");
        assert_eq!(duration(time::Duration::minutes(120)), "2 h");
        assert_eq!(duration(time::Duration::minutes(133)), "2 h 13 min");
        assert_eq!(duration(time::Duration::hours(30)), "1 day 6 h");
        assert_eq!(duration(time::Duration::days(3)), "3 days");
        assert_eq!(duration(time::Duration::minutes(-5)), "under a minute");
    }

    #[test]
    fn times_fall_back_to_utc_until_the_browser_localizes_them() {
        let html = window(
            datetime!(2026-09-24 11:44 UTC),
            datetime!(2026-09-24 11:54 UTC),
        )
        .into_string();
        assert!(html.contains(
            r#"datetime="2026-09-24T11:44:00Z" data-local="datetime">Sep 24, 11:44 UTC"#
        ));
        assert!(html.contains(r#"data-local="time">11:54 UTC"#));
    }

    #[test]
    fn past_times_read_as_words_with_the_exact_time_on_hover() {
        let now = datetime!(2026-09-24 12:52 UTC);
        let html = ago(datetime!(2026-09-24 12:40 UTC), now).into_string();
        assert!(html.contains(r#"title="Sep 24, 12:40 UTC">12 min ago</time>"#));
        assert!(
            !html.contains("data-local"),
            "the browser must not rewrite it"
        );
        assert!(ago(now, now).into_string().contains("just now"));
    }

    #[test]
    fn ids_show_their_distinct_tail() {
        assert_eq!(short_id("01a0d0f5-52e1-7141-b4e1-8dbd7169fc2e"), "7169fc2e");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(
            short_npub("npub17mkga3fat8wls2xlj3eel9dvzf06kduhec6naek4s9lsr85tkfesmgf08f"),
            "npub17mkg…f08f"
        );
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(22), "22nd");
    }

    #[test]
    fn a_par_band_off_the_forecast_says_which_way_forecasts_have_leaned() {
        assert_eq!(
            Metric::TempHigh.lean(-3.6, -1.0).as_deref(),
            Some("forecasts here have run ~2°F hot lately")
        );
        assert_eq!(
            Metric::TempLow.lean(0.4, 4.0).as_deref(),
            Some("forecasts here have run ~2°F cold lately")
        );
        assert_eq!(
            Metric::WindSpeed.lean(0.5, 1.5).as_deref(),
            Some("forecasts here have run ~1 knot low lately")
        );
        assert_eq!(
            Metric::WindSpeed.lean(-4.0, -2.0).as_deref(),
            Some("forecasts here have run ~3 knots high lately")
        );
        assert_eq!(Metric::TempHigh.lean(-1.6, 1.2), None);
    }
}
