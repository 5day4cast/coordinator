//! Values as people read them: times as "02:12 UTC · 22 min ago", durations as "3m 58s", and
//! amounts in sats grouped by thousands ("1,999,038"), with the millisats a routing fee comes in.
//! The exports keep plain numbers; only pages group them.

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

/// A long id shortened to its ends, "tark1qqq…qqqqqqqq", or `None` if it is short enough to show
/// whole.
fn shorten(id: &str) -> Option<String> {
    const ENDS: usize = 8;
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 2 * ENDS + 3 {
        return None;
    }
    let head: String = chars[..ENDS].iter().collect();
    let tail: String = chars[chars.len() - ENDS..].iter().collect();
    Some(format!("{head}…{tail}"))
}

/// A long id shortened to its ends, with the full id to hover and a button to copy it whole. For
/// ids in tables, where the whole one would push the columns beside it away.
pub fn copyable_short(id: &str) -> Markup {
    match shorten(id) {
        Some(short) => html! {
            code.id title=(id) { (short) }
            button.copy type="button" data-copy=(id) title="Copy" { "copy" }
        },
        None => copyable(id),
    }
}

/// A command with each long argument (a hash, an address) shortened to its ends; the copy button
/// copies the whole command.
pub fn copyable_command(command: &str) -> Markup {
    let shown = command
        .split(' ')
        .map(|word| shorten(word).unwrap_or_else(|| word.to_string()))
        .collect::<Vec<_>>()
        .join(" ");
    html! {
        code.id title=(command) { (shown) }
        @if !command.is_empty() { button.copy type="button" data-copy=(command) title="Copy" { "copy" } }
    }
}

/// A link's text: its host and the last part of its path, "5day4cast.com/…/leaderboard", or the
/// whole path when it has two parts, "4casttruth.win/events/01a0da04…ebc77891". Long parts are
/// shortened; the link itself stays whole.
pub fn link_text(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    match parts.as_slice() {
        [] => rest.to_string(),
        [host] => host.to_string(),
        [host, middle @ .., last] => {
            let last = shorten(last).unwrap_or_else(|| last.to_string());
            match middle {
                [] => format!("{host}/{last}"),
                [one] => {
                    let one = shorten(one).unwrap_or_else(|| one.to_string());
                    format!("{host}/{one}/{last}")
                }
                _ => format!("{host}/…/{last}"),
            }
        }
    }
}

/// An amount of sats as people read it: "40,700", "1,999,038".
pub fn sats(sats: u64) -> String {
    group(&sats.to_string())
}

/// A signed amount of sats, as [`sats`] writes it: "-1,100".
pub fn sats_signed(sats: i64) -> String {
    match sats {
        negative if negative < 0 => format!("-{}", group(&negative.unsigned_abs().to_string())),
        positive => group(&positive.to_string()),
    }
}

/// A number written in digits, its whole part grouped by thousands: "1234.5" is "1,234.5".
/// Anything else is left as it is.
pub fn group(number: &str) -> String {
    let (whole, rest) = number.split_at(number.find('.').unwrap_or(number.len()));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return number.to_string();
    }
    let mut grouped = String::with_capacity(number.len() + whole.len() / 3);
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped.push_str(rest);
    grouped
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

    #[test]
    fn sats_are_grouped_by_thousands() {
        assert_eq!(sats(0), "0");
        assert_eq!(sats(999), "999");
        assert_eq!(sats(1_100), "1,100");
        assert_eq!(sats(40_700), "40,700");
        assert_eq!(sats(768_803), "768,803");
        assert_eq!(sats(1_999_038), "1,999,038");
        assert_eq!(sats_signed(-1_100), "-1,100");
        assert_eq!(sats_signed(-5), "-5");
        assert_eq!(sats_signed(3_000), "3,000");
        assert_eq!(group("1234.001"), "1,234.001");
        assert_eq!(group("0.5"), "0.5");
        assert_eq!(group("-"), "-");
    }

    #[test]
    fn a_long_id_is_shortened_but_copied_whole() {
        let address = format!("tark1{}", "q".repeat(60));
        let shown = copyable_short(&address).into_string();
        assert!(shown.contains("tark1qqq…qqqqqqqq"), "{shown}");
        assert!(
            shown.contains(&format!("data-copy=\"{address}\"")),
            "{shown}"
        );
        assert!(shown.contains(&format!("title=\"{address}\"")), "{shown}");
        assert_eq!(
            copyable_short("alice").into_string(),
            copyable("alice").into_string()
        );
    }

    #[test]
    fn a_command_shows_its_long_arguments_shortened_and_copies_whole() {
        let hash = "ab".repeat(32);
        let command = format!("lncli lookupinvoice {hash}");
        let shown = copyable_command(&command).into_string();
        assert!(
            shown.contains(">lncli lookupinvoice abababab…abababab<"),
            "{shown}"
        );
        assert!(
            shown.contains(&format!("data-copy=\"{command}\"")),
            "{shown}"
        );
    }

    #[test]
    fn a_link_reads_as_its_host_and_last_part() {
        let id = "01a0da04-c7a4-7fc1-8d0f-0dfeebc77891";
        assert_eq!(
            link_text(&format!(
                "https://5day4cast.com/competitions/{id}/leaderboard"
            )),
            "5day4cast.com/…/leaderboard"
        );
        assert_eq!(
            link_text(&format!("https://4casttruth.win/events/{id}")),
            "4casttruth.win/events/01a0da04…ebc77891"
        );
        assert_eq!(
            link_text("https://5day4cast.com/api/v1/entries?event_id=abc"),
            "5day4cast.com/…/entries"
        );
        assert_eq!(link_text("https://mutinynet.com/"), "mutinynet.com");
    }
}
