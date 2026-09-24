//! An entry's picks with the oracle's forecast and reading for each, and
//! whether the pick scored. While the competition's window is open each pick
//! shows the reading so far and how it stands, and the dialog refreshes
//! itself every minute until the window closes.

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::leaderboard::{Phase, PickProgress, PickState};
use crate::infra::oracle::ValueOptions;
use crate::templates::format::{self, MetricText};

/// How often an open window's picks and leaderboard refresh.
pub const LIVE_REFRESH: &str = "every 60s";

/// One pick as the leaderboard computed it, with its station's name as
/// players know it.
#[derive(Debug, Clone)]
pub struct PickView<'a> {
    pub pick: &'a PickProgress,
    pub station_name: Option<String>,
}

/// The badge for how a pick stands. "Final" is the oracle's attested result
/// only; a closed window without it is still awaiting that result.
fn badge(state: PickState) -> (&'static str, &'static str) {
    match state {
        PickState::Pending => ("pick-state", "Waiting for readings"),
        PickState::LockedIn => ("pick-state is-locked-in", "Locked in"),
        PickState::OnTrack => ("pick-state is-on-track", "On track"),
        PickState::OffTrack => ("pick-state is-off-track", "Off track"),
        PickState::Out => ("pick-state is-out", "Out"),
        PickState::AwaitingResult => ("pick-state is-awaiting", "Awaiting the oracle's result"),
        PickState::Final => ("pick-state is-final", "Final"),
    }
}

fn pick_label(pick: &ValueOptions) -> &'static str {
    match pick {
        ValueOptions::Over => "Over",
        ValueOptions::Par => "Par",
        ValueOptions::Under => "Under",
    }
}

/// Where an entry's picks load from.
pub fn detail_url(entry_id: &str) -> String {
    format!("/entries/{entry_id}/detail")
}

/// The picks dialog's content. While the window is open it reloads itself
/// every minute; the reload after the window closes, or after the
/// competition is cancelled, carries no trigger, so the polling stops there.
/// `updated_at` is when the readings so far last changed, if known.
pub fn picks_detail(
    entry_id: &str,
    picks: &[PickView],
    phase: Phase,
    updated_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Markup {
    let live = phase == Phase::Live;
    let any_observed = picks.iter().any(|view| view.pick.observed.is_some());
    let total: u64 = picks.iter().map(|view| view.pick.points).sum();
    let mut stations: Vec<&str> = Vec::new();
    for view in picks {
        if !stations.contains(&view.pick.station_id.as_str()) {
            stations.push(&view.pick.station_id);
        }
    }
    let url = detail_url(entry_id);
    html! {
        div class="picks-detail"
            hx-get=[live.then_some(&url)] hx-trigger=[live.then_some(LIVE_REFRESH)]
            hx-target=[live.then_some("this")] hx-swap=[live.then_some("outerHTML")] {
            div class="entry-detail-header" {
                div {
                    h2 class="title is-5 mb-1" { "Picks" }
                    span class="entry-id" { "Entry " (format::copyable_id(entry_id)) }
                }
                @if any_observed {
                    div class="entry-detail-score" {
                        (total) " pts"
                        @match phase {
                            Phase::Live => { span class="fact-note" { "so far" } }
                            Phase::AwaitingResult => { span class="fact-note" { "awaiting the oracle's result" } }
                            Phase::Scored => { span class="fact-note" { "Final" } }
                            _ => {}
                        }
                    }
                }
            }
            @match phase {
                Phase::Live => {
                    p class="provisional-note" {
                        "Provisional: scored as if the window ended now"
                        @if let Some(at) = updated_at {
                            " · updated " (format::ago(at, now))
                        }
                        "."
                    }
                }
                Phase::AwaitingResult => {
                    p class="provisional-note" {
                        "Window closed: awaiting the oracle's result. The points are what the "
                        "readings so far score; the oracle's own reading decides."
                    }
                }
                Phase::Unfilled => {
                    p class="notice" { "Not enough entries arrived before the window started, so nothing is scored and entry fees are refunded." }
                }
                Phase::Expired => {
                    p class="notice" { "The oracle never published a result, so every entry was refunded." }
                }
                Phase::Cancelled | Phase::Failed => {
                    p class="notice" { "This competition did not run, so nothing is scored." }
                }
                Phase::Upcoming | Phase::Scored => {}
            }
            @if picks.is_empty() {
                p class="empty-state" {
                    @if phase == Phase::Upcoming { "Picks become public when entries close." }
                    @else { "No picks recorded." }
                }
            } @else if !any_observed {
                p class="entry-pending-msg mb-3" {
                    @match phase {
                        Phase::Live | Phase::AwaitingResult => { "Readings appear here as the oracle records them." }
                        Phase::Scored => { "No readings were recorded for these stations in the window, so no pick scored." }
                        _ => { "Readings appear here once the window opens." }
                    }
                }
            }
            @for station in &stations {
                @let station_picks: Vec<&PickView> = picks.iter().filter(|view| view.pick.station_id == *station).collect();
                section class="picks-station" {
                    h3 class="picks-station-name" {
                        @if let Some(name) = station_picks.first().and_then(|view| view.station_name.as_deref()) {
                            (name) " "
                        }
                        span class="station-code" { (station) }
                    }
                    @for view in station_picks {
                        @if live { (live_pick_row(view.pick)) } @else { (pick_row(view.pick)) }
                    }
                }
            }
        }
    }
}

fn forecast_value(pick: &PickProgress) -> Markup {
    html! {
        "Forecast "
        strong { (pick.forecast.map(|value| pick.metric.value(value)).unwrap_or_else(|| "—".into())) }
    }
}

/// A pick once the window has closed: the reading, whether it scored, and
/// whether that is the oracle's final result yet.
fn pick_row(pick: &PickProgress) -> Markup {
    let scored = pick.forecast.is_some() && pick.observed.is_some();
    let class = match (scored, pick.hit) {
        (true, true) => "scored-pick is-hit",
        (true, false) => "scored-pick is-miss",
        (false, _) => "scored-pick",
    };
    let (badge_class, badge_text) = badge(pick.state);
    html! {
        div class=(class) {
            span class="pick-metric" { (pick.metric.label()) }
            span class="pick-values" {
                (forecast_value(pick))
                @if let Some(observed) = pick.observed {
                    " · Observed " strong { (pick.metric.value(observed)) }
                }
            }
            span class="pick-choice" { (pick_label(&pick.pick)) }
            span class="pick-result" {
                @if scored {
                    @if pick.hit { "✓ +" (pick.points) } @else { "✗ 0" }
                    span class=(badge_class) { (badge_text) }
                }
            }
        }
    }
}

/// `Forecast 69°F · High so far 71°F · 9 of 24 h`, and how the pick stands.
fn live_pick_row(pick: &PickProgress) -> Markup {
    let (badge_class, badge_text) = badge(pick.state);
    let row_class = match pick.state {
        PickState::LockedIn => "scored-pick is-live is-locked-in",
        PickState::Out => "scored-pick is-live is-out",
        _ => "scored-pick is-live",
    };
    let hours_total = pick.hours_total.ceil() as u32;
    let hours_covered = (pick.hours_covered.floor() as u32).min(hours_total);
    html! {
        div class=(row_class) {
            span class="pick-metric" { (pick.metric.label()) }
            span class="pick-values" {
                (forecast_value(pick))
                @match pick.observed {
                    Some(value) => { " · " (pick.metric.so_far()) " " strong { (pick.metric.value(value)) } }
                    None => { " · no reading yet" }
                }
                @if hours_total > 0 {
                    " · reports through " (hours_covered) " of " (hours_total) " h"
                }
            }
            span class="pick-choice" { (pick_label(&pick.pick)) }
            span class="pick-result" {
                @if pick.observed.is_some() {
                    span class=(badge_class) { (badge_text) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::leaderboard::{progress::points, Metric};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-24 12:52 UTC);

    fn pick(
        metric: Metric,
        choice: ValueOptions,
        forecast: f64,
        observed: Option<f64>,
        state: PickState,
    ) -> PickProgress {
        let points = points(&choice, metric, Some(forecast), observed);
        PickProgress {
            station_id: "KJFK".into(),
            metric,
            pick: choice,
            forecast: Some(forecast),
            observed,
            state,
            points,
            hit: points > 0,
            hours_covered: 9.4,
            hours_total: 24.0,
        }
    }

    fn views(picks: &[PickProgress]) -> Vec<PickView<'_>> {
        picks
            .iter()
            .map(|pick| PickView {
                pick,
                station_name: Some("John F Kennedy International Airport".into()),
            })
            .collect()
    }

    #[test]
    fn a_scored_entry_shows_readings_hits_and_misses_as_final() {
        let picks = [
            pick(
                Metric::TempHigh,
                ValueOptions::Under,
                69.0,
                Some(55.04),
                PickState::Final,
            ),
            pick(
                Metric::WindSpeed,
                ValueOptions::Over,
                18.0,
                Some(11.0),
                PickState::Final,
            ),
        ];
        let html = picks_detail(
            "01a0-entry-1234abcd",
            &views(&picks),
            Phase::Scored,
            None,
            NOW,
        )
        .into_string();
        assert!(html.contains("John F Kennedy International Airport"));
        assert!(html.contains("Forecast <strong>69°F</strong> · Observed <strong>55°F</strong>"));
        assert!(html.contains("18 knots"));
        assert!(html.contains("✓ +10"));
        assert!(html.contains("✗ 0"));
        assert!(html.contains("10 pts"));
        assert!(html.contains("Final"));
        assert!(!html.contains("mph"));
        assert!(
            !html.contains("hx-trigger"),
            "a scored entry does not refresh"
        );
    }

    /// Final is the oracle's attested result only.
    #[test]
    fn a_closed_window_awaits_the_oracle_and_says_so() {
        let picks = [pick(
            Metric::TempHigh,
            ValueOptions::Over,
            69.0,
            Some(71.0),
            PickState::AwaitingResult,
        )];
        let html =
            picks_detail("e1", &views(&picks), Phase::AwaitingResult, None, NOW).into_string();
        assert!(html.contains("Observed <strong>71°F</strong>"));
        assert!(html.contains("✓ +10"));
        assert!(html.contains("Window closed: awaiting the oracle"));
        assert!(!html.contains(">Final<"));
        assert!(!html.contains("hx-trigger"));
    }

    #[test]
    fn missing_readings_say_whether_they_can_still_come() {
        let picks = [pick(
            Metric::TempLow,
            ValueOptions::Par,
            55.0,
            None,
            PickState::Pending,
        )];
        let waiting =
            picks_detail("e", &views(&picks), Phase::AwaitingResult, None, NOW).into_string();
        assert!(waiting.contains("as the oracle records them"));
        let over = picks_detail("e", &views(&picks), Phase::Scored, None, NOW).into_string();
        assert!(over.contains("No readings were recorded"));
        let before = picks_detail("e", &[], Phase::Upcoming, None, NOW).into_string();
        assert!(before.contains("Picks become public when entries close."));
    }

    #[test]
    fn an_open_window_shows_readings_so_far_and_how_each_pick_stands() {
        let picks = [
            pick(
                Metric::TempHigh,
                ValueOptions::Over,
                69.0,
                Some(71.0),
                PickState::LockedIn,
            ),
            pick(
                Metric::TempLow,
                ValueOptions::Par,
                55.0,
                Some(55.0),
                PickState::OnTrack,
            ),
            pick(
                Metric::WindSpeed,
                ValueOptions::Under,
                12.0,
                Some(14.0),
                PickState::Out,
            ),
            pick(
                Metric::TempHigh,
                ValueOptions::Par,
                60.0,
                Some(58.0),
                PickState::OffTrack,
            ),
        ];
        let updated = Some(NOW - time::Duration::minutes(12));
        let html = picks_detail("e1", &views(&picks), Phase::Live, updated, NOW).into_string();
        assert!(html.contains(
            "Forecast <strong>69°F</strong> · High so far <strong>71°F</strong> · reports through 9 of 24 h"
        ));
        assert!(html.contains("Low so far <strong>55°F</strong>"));
        assert!(html.contains("Top wind so far <strong>14 knots</strong>"));
        for badge in ["Locked in", "On track", "Out", "Off track"] {
            assert!(html.contains(badge), "{badge}");
        }
        // Locked in (10) and on track (par, 20), as if the window ended now.
        assert!(html.contains("30 pts"));
        assert!(html.contains("Provisional: scored as if the window ended now · updated"));
        assert!(html.contains("12 min ago"));
        assert!(html.contains(r#"hx-get="/entries/e1/detail""#));
        assert!(html.contains(r#"hx-trigger="every 60s""#));
        assert!(html.contains(r#"hx-swap="outerHTML""#));
    }

    #[test]
    fn a_pick_without_a_reading_yet_has_no_badge() {
        let picks = [pick(
            Metric::TempLow,
            ValueOptions::Par,
            55.0,
            None,
            PickState::Pending,
        )];
        let html = picks_detail("e1", &views(&picks), Phase::Live, None, NOW).into_string();
        assert!(html.contains("no reading yet"));
        assert!(!html.contains("Waiting for readings"));
        assert!(html.contains("as the oracle records them"));
    }

    /// A competition cancelled mid-window stops refreshing.
    #[test]
    fn only_an_open_window_refreshes() {
        let picks = [pick(
            Metric::TempHigh,
            ValueOptions::Over,
            69.0,
            Some(71.0),
            PickState::OnTrack,
        )];
        for phase in [
            Phase::Cancelled,
            Phase::Failed,
            Phase::Unfilled,
            Phase::Expired,
        ] {
            let html = picks_detail("e1", &views(&picks), phase, None, NOW).into_string();
            assert!(!html.contains("hx-trigger"), "{phase:?}");
        }
    }
}
