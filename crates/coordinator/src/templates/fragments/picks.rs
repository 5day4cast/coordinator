//! An entry's picks with the oracle's forecast and reading for each, and
//! whether the pick scored. While the competition's window is open each pick
//! shows the reading so far and how it stands, and the dialog refreshes
//! itself every minute until the window closes. Before the window opens the
//! picks are withheld from everyone but the player who made them, who loads
//! their own from [`own_detail_url`].

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::leaderboard::{Phase, PickProgress, PickState, Rule};
use crate::infra::oracle::ValueOptions;
use crate::templates::{
    components::{tip_end, tip_start},
    format::{self, city_name, MetricText},
    fragments::entry_form::edit_picks_url,
};

/// How often an open window's picks and leaderboard refresh.
pub const LIVE_REFRESH: &str = "every 60s";

const WITHHELD: &str = "Picks become public when entries close.";

/// One pick as the leaderboard computed it, with its station's name as
/// players know it.
#[derive(Debug, Clone)]
pub struct PickView<'a> {
    pub pick: &'a PickProgress,
    pub station_name: Option<String>,
}

/// Who is looking at an entry's picks before its window opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Viewer {
    /// Not known: the public dialog, which then asks for the viewer's own.
    Unknown,
    /// The player who made the entry.
    Owner,
    /// Anyone else, or the owner logged out.
    Other,
}

/// The badge for how a pick stands, and what that means on hover. Only settled picks get one:
/// while the window is open and a pick can still flip, the reading so far and its points say
/// how it stands. "Final" is the oracle's attested result only; a closed window without it is
/// still awaiting that result.
fn badge(state: PickState) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match state {
        PickState::Pending | PickState::OnTrack | PickState::OffTrack => return None,
        PickState::LockedIn => (
            "pick-state is-locked-in",
            "Locked in",
            "Right, and the rest of the window can't change that",
        ),
        PickState::Out => (
            "pick-state is-out",
            "Out",
            "Wrong, and the rest of the window can't change that",
        ),
        PickState::AwaitingResult => (
            "pick-state is-awaiting",
            "Awaiting oracle",
            "The window has closed; the oracle's own reading decides",
        ),
        PickState::Final => (
            "pick-state is-final",
            "Final",
            "The oracle's attested result",
        ),
    })
}

pub(crate) fn state_badge(state: PickState) -> Markup {
    html! {
        @if let Some((class, text, title)) = badge(state) {
            span class=(class) tabindex="0" data-tip=(title) { (text) }
        }
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

/// Where a player's own picks load from before the window opens: signed when
/// the player is logged in, so the server can tell the entry's owner.
pub fn own_detail_url(entry_id: &str) -> String {
    format!("/entries/{entry_id}/detail/mine")
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
    detail(
        entry_id,
        picks,
        phase,
        updated_at,
        now,
        Viewer::Unknown,
        None,
    )
}

/// The dialog's content for the entry's owner before the window opens, with
/// the forecasts so far: what [`picks_detail`] withholds until then. `lock` is why the picks
/// can no longer change; without one the owner gets Edit picks.
pub fn own_picks_detail(
    entry_id: &str,
    picks: &[PickView],
    now: OffsetDateTime,
    lock: Option<&str>,
) -> Markup {
    detail(
        entry_id,
        picks,
        Phase::Upcoming,
        None,
        now,
        Viewer::Owner,
        Some(lock),
    )
}

/// What anyone but the owner gets from [`own_detail_url`].
pub fn withheld_picks_detail(entry_id: &str, now: OffsetDateTime) -> Markup {
    detail(
        entry_id,
        &[],
        Phase::Upcoming,
        None,
        now,
        Viewer::Other,
        None,
    )
}

fn detail(
    entry_id: &str,
    picks: &[PickView],
    phase: Phase,
    updated_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
    viewer: Viewer,
    // The owner's: why their picks are locked, or none when they may edit them.
    edit: Option<Option<&str>>,
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
                    h2 class="title is-5 mb-1" {
                        @if viewer == Viewer::Owner { "Your picks" } @else { "Picks" }
                    }
                    span class="entry-id" { "Entry " (format::copyable_id(entry_id)) }
                }
                @if let Some(None) = edit {
                    button type="button" class="button is-small is-link is-light"
                      hx-get=(edit_picks_url(entry_id)) hx-target="#entryValues" hx-swap="innerHTML" {
                        "Edit picks"
                    }
                }
                @if any_observed {
                    div class="entry-detail-score" {
                        (total) " pts"
                        @match phase {
                            Phase::Live => { span class="fact-note" { "so far" } }
                            Phase::AwaitingResult => { span class="fact-note" { "pending" } }
                            Phase::Scored => { span class="fact-note" { "final" } }
                            _ => {}
                        }
                    }
                }
            }
            @if let Some(Some(reason)) = edit {
                p class="fact-note picks-locked" { (reason) "." }
            }
            @match phase {
                Phase::Live => {
                    p class="provisional-note" {
                        @if let Some(at) = updated_at { "Updated " (format::ago(at, now)) }
                        @if let Some((covered, total)) = coverage(picks) {
                            @if updated_at.is_some() { " · " }
                            "reports in for " (covered) " of " (total) " h"
                        }
                        (tip_end("Scored as if the window ended now; picks can still change until it closes."))
                    }
                }
                Phase::AwaitingResult => {
                    p class="provisional-note" {
                        "Window closed"
                        (tip_start("The oracle's own reading decides the final scores."))
                    }
                }
                Phase::Unfilled => {
                    p class="notice" { "Not enough entries arrived before the window started, so nothing is scored and entry fees are refunded." }
                }
                Phase::Expired => {
                    p class="notice" { "The oracle never signed a result in time, so the pot is shared back among the entries." }
                }
                Phase::Cancelled | Phase::Failed => {
                    p class="notice" { "This competition did not run, so nothing is scored." }
                }
                Phase::Upcoming | Phase::Scored => {}
            }
            @if picks.is_empty() {
                @match (phase, viewer) {
                    // Asks for the viewer's own picks; the request is signed when they are logged in.
                    (Phase::Upcoming, Viewer::Unknown) => {
                        p class="empty-state" hx-get=(own_detail_url(entry_id)) hx-trigger="load"
                          hx-target="#entryValues" hx-swap="innerHTML" { (WITHHELD) }
                    }
                    (Phase::Upcoming, Viewer::Other) => { p class="empty-state" { (WITHHELD) } }
                    _ => { p class="empty-state" { "No picks recorded." } }
                }
            } @else if !any_observed && matches!(phase, Phase::Live | Phase::AwaitingResult | Phase::Scored) {
                p class="entry-pending-msg mb-3" {
                    @if phase == Phase::Scored {
                        "No readings were recorded for these stations in the window, so no pick scored."
                    } @else {
                        "No readings yet."
                    }
                }
            }
            @if !picks.is_empty() { (picks_header(phase)) }
            @for station in &stations {
                @let station_picks: Vec<&PickView> = picks.iter().filter(|view| view.pick.station_id == *station).collect();
                section class="picks-station" {
                    @let name = station_picks.first().and_then(|view| view.station_name.as_deref());
                    // The city leads; the station's own name is its tooltip, and its code
                    // stays small beside it. Without the oracle's stations, the code alone.
                    h3 class="picks-station-name" title=[name.map(|name| format!("Weather station: {name} ({station})"))] {
                        @if let Some(name) = name {
                            (city_name(name)) " "
                        }
                        span class="station-code" { (station) }
                    }
                    @for view in station_picks {
                        @if live { (live_pick_row(view.pick)) } @else { (pick_row(view.pick, phase != Phase::Expired)) }
                    }
                }
            }
        }
    }
}

/// What each column of the pick rows is, once above them: the reading picked on, the pick and
/// what it needs, the reading and the points. Before the window there is neither of the last two.
fn picks_header(phase: Phase) -> Markup {
    let observed = match phase {
        Phase::Upcoming => None,
        Phase::Live => Some("So far"),
        _ => Some("Observed"),
    };
    html! {
        div class="scored-pick picks-header" aria-hidden="true" {
            span { "Reading" }
            span { "Pick" }
            @if let Some(observed) = observed {
                span class="pick-reading" { (observed) }
                span class="pick-result" { "Points" }
            }
        }
    }
}

/// How much of the window the stations have reported, the least-covered pick's: `(9, 24)`.
fn coverage(picks: &[PickView]) -> Option<(u32, u32)> {
    picks
        .iter()
        .map(|view| view.pick)
        .filter(|pick| pick.hours_total > 0.0)
        .map(|pick| {
            let total = pick.hours_total.ceil() as u32;
            ((pick.hours_covered.floor() as u32).min(total), total)
        })
        .min()
}

/// What the pick needs the reading to be: `< 69.4°F`, `69.4–72.0°F`, `> 72.0°F`; with a fixed
/// Par, the forecast itself: `< 69°F`, `69°F`, `> 69°F`.
fn pick_target(pick: &PickProgress) -> Option<String> {
    let forecast = pick.forecast?;
    let metric = pick.metric;
    Some(match (pick.rule, &pick.pick) {
        (Some(Rule::Line { lower, .. }), ValueOptions::Under) => {
            format!("< {}", metric.bound(forecast + lower))
        }
        (Some(Rule::Line { lower, upper }), ValueOptions::Par) => {
            metric.range(forecast + lower, forecast + upper)
        }
        (Some(Rule::Line { upper, .. }), ValueOptions::Over) => {
            format!("> {}", metric.bound(forecast + upper))
        }
        (_, ValueOptions::Under) => format!("< {}", metric.value(forecast)),
        (_, ValueOptions::Par) => metric.value(forecast),
        (_, ValueOptions::Over) => format!("> {}", metric.value(forecast)),
    })
}

/// `High · Under < 69.4°F`: the reading, the pick and what it needs.
fn pick_cells(pick: &PickProgress) -> Markup {
    html! {
        span class="pick-metric" { (pick.metric.short()) }
        span class="pick-choice" {
            (pick_label(&pick.pick))
            @if let Some(target) = pick_target(pick) { " " span class="pick-target" { (target) } }
        }
        span class="pick-reading" {
            @if let Some(observed) = pick.observed { (pick.metric.value(observed)) }
        }
    }
}

/// A pick outside the open window: before it, just the pick; after it, the reading and
/// whether it scored. `badge` is false once the contract has expired: its picks' state still
/// says they await the oracle, and nothing is awaited any more.
fn pick_row(pick: &PickProgress, badge: bool) -> Markup {
    let scored = pick.forecast.is_some() && pick.observed.is_some();
    let class = match (scored, pick.hit) {
        (true, true) => "scored-pick is-hit",
        (true, false) => "scored-pick is-miss",
        (false, _) => "scored-pick",
    };
    html! {
        div class=(class) {
            (pick_cells(pick))
            span class="pick-result" {
                @if scored {
                    @if pick.hit { "✓ +" (pick.points) } @else { "✗ 0" }
                    @if badge { (state_badge(pick.state)) }
                }
            }
        }
    }
}

/// A pick while the window is open: the reading so far, the points it earns if the window
/// ended now, and how it stands.
fn live_pick_row(pick: &PickProgress) -> Markup {
    let row_class = match pick.state {
        PickState::LockedIn => "scored-pick is-live is-locked-in",
        PickState::Out => "scored-pick is-live is-out",
        _ => "scored-pick is-live",
    };
    html! {
        div class=(row_class) {
            (pick_cells(pick))
            span class="pick-result" {
                @if pick.observed.is_some() {
                    span class="pick-points" { @if pick.hit { "+" (pick.points) } @else { "0" } }
                    (state_badge(pick.state))
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
        let points = points(&choice, metric, Some(Rule::Fixed), Some(forecast), observed);
        PickProgress {
            station_id: "KJFK".into(),
            metric,
            pick: choice,
            rule: Some(Rule::Fixed),
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
                station_name: Some("New York/JFK International, NY".into()),
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
        // The city leads, with the station's full name on hover and its code beside it.
        assert!(html.contains(r#"title="Weather station: New York/JFK International, NY (KJFK)">New York, NY <span class="station-code">KJFK</span>"#));
        assert!(html.contains(r#"Under <span class="pick-target">&lt; 69°F</span>"#));
        assert!(html.contains(r#"<span class="pick-reading">55°F</span>"#));
        assert!(html.contains("&gt; 18 knots"));
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
        assert!(html.contains(r#"<span class="pick-reading">71°F</span>"#));
        assert!(html.contains("✓ +10"));
        assert!(html.contains("Window closed"));
        assert!(
            html.contains("oracle&#39;s own reading decides")
                || html.contains("oracle's own reading decides")
        );
        assert!(!html.contains(">Final<"));
        assert!(!html.contains("hx-trigger"));
    }

    /// Once the contract has expired unsigned nothing is awaited any more, and the pot's
    /// shares may not have been paid yet: the dialog claims neither.
    #[test]
    fn an_expired_competition_awaits_nothing_and_does_not_say_refunds_are_done() {
        let picks = [pick(
            Metric::TempHigh,
            ValueOptions::Over,
            69.0,
            Some(71.0),
            PickState::AwaitingResult,
        )];
        let html = picks_detail("e1", &views(&picks), Phase::Expired, None, NOW).into_string();
        assert!(html.contains("so the pot is shared back among the entries."));
        assert!(!html.contains("was refunded"));
        assert!(!html.contains("Awaiting oracle"));
        assert!(html.contains(r#"<span class="pick-reading">71°F</span>"#));
    }

    /// The status fits its column: short, with the reason in its tip.
    #[test]
    fn the_awaiting_status_is_short_and_explains_itself_on_tap() {
        let picks = [pick(
            Metric::TempHigh,
            ValueOptions::Over,
            83.0,
            Some(83.0),
            PickState::AwaitingResult,
        )];
        let html =
            picks_detail("e1", &views(&picks), Phase::AwaitingResult, None, NOW).into_string();
        assert!(html.contains(r#"<span class="pick-state is-awaiting" tabindex="0" data-tip="The window has closed; the oracle"#));
        assert!(html.contains(">Awaiting oracle</span>"));
        assert!(
            !html.contains("Awaiting the oracle&#39;s result")
                && !html.contains("Awaiting the oracle's result")
        );
        // The picks dialog's "Window closed" tip still grows rightwards from the line's start.
        assert!(html.contains(r#"Window closed<span class="tip tip-start""#));
    }

    /// Which value is the pick and which the reading: named once, above every station's rows.
    #[test]
    fn one_header_names_the_columns() {
        let picks = [
            pick(
                Metric::TempHigh,
                ValueOptions::Over,
                83.0,
                Some(83.0),
                PickState::Final,
            ),
            PickProgress {
                station_id: "KMIA".into(),
                ..pick(
                    Metric::TempLow,
                    ValueOptions::Under,
                    75.4,
                    Some(72.0),
                    PickState::Final,
                )
            },
        ];
        let header = r#"<div class="scored-pick picks-header" aria-hidden="true"><span>Reading</span><span>Pick</span><span class="pick-reading">Observed</span><span class="pick-result">Points</span></div>"#;
        let html = picks_detail("e1", &views(&picks), Phase::Scored, None, NOW).into_string();
        assert_eq!(html.matches(header).count(), 1, "{html}");
        assert!(
            html.find(header) < html.find("picks-station"),
            "above the rows"
        );
        assert_eq!(html.matches("picks-station-name").count(), 2);

        let live = picks_detail("e1", &views(&picks), Phase::Live, None, NOW).into_string();
        assert!(live.contains(r#"<span class="pick-reading">So far</span>"#));
        // Before the window there is only the pick to name.
        let own = own_picks_detail("e1", &views(&picks), NOW, None).into_string();
        assert!(own.contains("<span>Reading</span><span>Pick</span></div>"));
        assert!(!own.contains(">Observed<") && !own.contains(">Points<"));
        // No picks, no header.
        let none = picks_detail("e1", &[], Phase::Scored, None, NOW).into_string();
        assert!(!none.contains("picks-header"));
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
        assert!(waiting.contains("No readings yet."));
        let over = picks_detail("e", &views(&picks), Phase::Scored, None, NOW).into_string();
        assert!(over.contains("No readings were recorded"));
        let before = picks_detail("e", &[], Phase::Upcoming, None, NOW).into_string();
        assert!(before.contains("Picks become public when entries close."));
        assert!(before.contains(r#"hx-get="/entries/e/detail/mine" hx-trigger="load""#));
    }

    /// Before the window opens the owner sees their own picks with the forecasts;
    /// anyone else gets the same message as the public dialog, without asking again.
    #[test]
    fn the_owner_sees_their_own_picks_before_the_window_opens() {
        let picks = [pick(
            Metric::TempHigh,
            ValueOptions::Over,
            69.0,
            None,
            PickState::Pending,
        )];
        let own = own_picks_detail(
            "e",
            &views(&picks),
            NOW,
            Some("Picks in this competition are locked once entered"),
        )
        .into_string();
        assert!(own.contains("Your picks"));
        assert!(own.contains(r#"Over <span class="pick-target">&gt; 69°F</span>"#));
        assert!(
            !own.contains("No readings"),
            "nothing is due before the window"
        );
        assert!(!own.contains("hx-"));
        assert!(own.contains("Picks in this competition are locked once entered."));
        assert!(!own.contains("Edit picks"));
        // While the competition takes picks, the owner can edit them in the same dialog.
        let editable = own_picks_detail("e", &views(&picks), NOW, None).into_string();
        assert!(editable.contains("Edit picks"));
        assert!(editable.contains(r##"hx-get="/entries/e/edit" hx-target="#entryValues""##));
        assert!(!editable.contains("locked"));
        let withheld = withheld_picks_detail("e", NOW).into_string();
        assert!(withheld.contains("Picks become public when entries close."));
        assert!(!withheld.contains("hx-"));
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
        // One line per pick: the metric, the pick and what it needs, the reading so far.
        assert!(html.contains(
            r#"<span class="pick-metric">High</span><span class="pick-choice">Over <span class="pick-target">&gt; 69°F</span></span><span class="pick-reading">71°F</span>"#
        ), "{html}");
        assert!(html.contains(r#"<span class="pick-reading">14 knots</span>"#));
        assert!(!html.contains("Forecast") && !html.contains("lately"));
        // Coverage is said once for the dialog, not per pick.
        assert_eq!(html.matches("reports in for 9 of 24 h").count(), 1);
        // Settled picks say so; a pick that can still flip shows only its reading and points.
        for badge in [">Locked in<", ">Out<"] {
            assert!(html.contains(badge), "{badge}");
        }
        for gone in ["On track", "Off track", "is-on-track", "is-off-track"] {
            assert!(!html.contains(gone), "{gone}");
        }
        assert!(
            html.contains(r#"data-tip="Wrong, and the rest of the window can&#39;t change that""#)
                || html
                    .contains(r#"data-tip="Wrong, and the rest of the window can't change that""#)
        );
        assert!(
            !html.contains("say how a pick stands"),
            "the legend is on the help page"
        );
        // Locked in (10) and a right Par so far (20), as if the window ended now, pick by pick.
        assert!(html.contains("30 pts"));
        assert!(html.contains(r#"<span class="pick-points">+10</span>"#));
        assert!(html.contains(r#"<span class="pick-points">+20</span>"#));
        assert_eq!(
            html.matches(r#"<span class="pick-points">0</span>"#)
                .count(),
            2
        );
        assert!(html.contains("Updated <time"));
        assert!(html.contains("12 min ago"));
        assert!(!html.contains("Provisional"));
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
        assert_eq!(html.matches("No readings yet.").count(), 1);
        assert!(!html.contains("Waiting for readings"));
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

    /// "✓ +10" sits on a green-tinted row in the dialog; it reads at WCAG AA in both themes.
    #[test]
    fn a_hit_s_points_read_at_4_5_to_1_in_both_themes() {
        use crate::templates::css_check::{contrast, over, rgba, rule, value};
        const PICKS: &str = include_str!("picks.css");
        const SITE: &str = include_str!("../static/styles.css");
        let tint = value(rule(PICKS, ".scored-pick.is-hit"), "background");
        for (theme, text) in [
            ("light", ".is-hit .pick-result"),
            ("dark", r#"[data-theme="dark"] .is-hit .pick-result"#),
        ] {
            let dialog = value(
                rule(SITE, &format!(r#"[data-theme="{theme}"]"#)),
                "--app-modal-bg",
            );
            let row = over(tint, rgba(dialog).0);
            let colour = value(rule(PICKS, text), "color");
            let ratio = contrast(rgba(colour).0, row);
            assert!(ratio >= 4.5, "{theme}: {colour} is {ratio:.2}:1");
        }
    }
}
