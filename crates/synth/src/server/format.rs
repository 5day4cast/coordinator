//! Values as people read them: times as "02:12 UTC · 22 min ago", durations as "3m 58s", and
//! amounts in sats with the millisats a routing fee comes in.

use maud::{html, Markup};
use time::OffsetDateTime;

/// When something happened, as "02:12 UTC · 22 min ago", with the day in front when it was not
/// today. Computed on the server, so it is as fresh as the page.
pub fn when(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    let clock = format!("{:02}:{:02} UTC", at.hour(), at.minute());
    let day = if at.date() == now.to_offset(time::UtcOffset::UTC).date() {
        clock
    } else {
        format!("{} {} {clock}", short_month(at.month()), at.day())
    };
    format!("{day} · {}", ago(now - at))
}

/// A time, readable, keeping the exact one to hover and to copy.
pub fn time(at: OffsetDateTime, now: OffsetDateTime) -> Markup {
    let exact = at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    html! { time datetime=(exact) title=(exact) { (when(at, now)) } }
}

/// A stored RFC 3339 time, as [`time`] shows it.
pub fn time_text(at: &str, now: OffsetDateTime) -> Markup {
    match parse(at) {
        Some(parsed) => time(parsed, now),
        None => html! { (at) },
    }
}

pub fn parse(at: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()
}

fn ago(elapsed: time::Duration) -> String {
    let seconds = elapsed.whole_seconds();
    if seconds < 0 {
        return format!("in {}", span(-seconds));
    }
    if seconds < 45 {
        return "just now".to_string();
    }
    format!("{} ago", span(seconds))
}

/// How long from one time to another, as [`when`] says it: "6 h", "25 min", "3 days".
pub fn span_between(from: OffsetDateTime, to: OffsetDateTime) -> String {
    span((to - from).whole_seconds().max(0))
}

/// A full id, with a button to copy it.
pub fn copyable(id: &str) -> Markup {
    html! {
        code.id { (id) }
        @if !id.is_empty() { button.copy type="button" data-copy=(id) title="Copy" { "copy" } }
    }
}

/// A span of time to the nearest unit people use for it.
fn span(seconds: i64) -> String {
    match seconds {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", (s + 30) / 60),
        s if s < 48 * 3600 => {
            let minutes = (s + 30) / 60;
            match (minutes / 60, minutes % 60) {
                (hours, 0) => format!("{hours} h"),
                (hours, minutes) => format!("{hours} h {minutes} min"),
            }
        }
        s => format!("{} days", s / 86_400),
    }
}

fn short_month(month: time::Month) -> &'static str {
    use time::Month::*;
    match month {
        January => "Jan",
        February => "Feb",
        March => "Mar",
        April => "Apr",
        May => "May",
        June => "Jun",
        July => "Jul",
        August => "Aug",
        September => "Sep",
        October => "Oct",
        November => "Nov",
        December => "Dec",
    }
}

/// How long something took: "850 ms", "12 s", "3m 58s", "1h 02m".
pub fn duration_ms(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms} ms");
    }
    let seconds = (ms + 500) / 1000;
    match seconds {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// An amount of millisats in sats, keeping the part of a sat a routing fee often is:
/// "1.001", "0.5", "12".
pub fn msat_as_sats(msat: u64) -> String {
    let (sats, rest) = (msat / 1000, msat % 1000);
    if rest == 0 {
        sats.to_string()
    } else {
        format!("{sats}.{rest:03}")
            .trim_end_matches('0')
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_time_today_reads_as_the_clock_and_how_long_ago() {
        let now = datetime!(2026-09-24 02:34:40 UTC);
        assert_eq!(
            when(datetime!(2026-09-24 02:12:28.478349406 UTC), now),
            "02:12 UTC · 22 min ago"
        );
        assert_eq!(when(now, now), "02:34 UTC · just now");
    }

    #[test]
    fn an_earlier_day_is_named() {
        let now = datetime!(2026-09-24 08:10:00 UTC);
        assert_eq!(
            when(datetime!(2026-09-23 23:03:04 UTC), now),
            "Sep 23 23:03 UTC · 9 h 7 min ago"
        );
        assert_eq!(
            when(datetime!(2026-09-20 23:03:04 UTC), now),
            "Sep 20 23:03 UTC · 3 days ago"
        );
    }

    #[test]
    fn a_time_to_come_reads_as_how_soon() {
        let now = datetime!(2026-09-24 08:10:00 UTC);
        assert_eq!(
            when(datetime!(2026-09-24 09:06:00 UTC), now),
            "09:06 UTC · in 56 min"
        );
    }

    #[test]
    fn durations_read_in_minutes_and_seconds() {
        assert_eq!(duration_ms(237_854), "3m 58s");
        assert_eq!(duration_ms(850), "850 ms");
        assert_eq!(duration_ms(12_300), "12 s");
        assert_eq!(duration_ms(3_725_000), "1h 02m");
    }

    #[test]
    fn a_routing_fee_keeps_its_millisats() {
        assert_eq!(msat_as_sats(1001), "1.001");
        assert_eq!(msat_as_sats(500), "0.5");
        assert_eq!(msat_as_sats(12_000), "12");
        assert_eq!(msat_as_sats(0), "0");
    }
}
