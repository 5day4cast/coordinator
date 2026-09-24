//! An entry's picks with the oracle's forecast and reading for each, and
//! whether the pick scored. While the competition's window is open each pick
//! shows the reading so far and how it stands, and the dialog refreshes
//! itself every minute until the window closes.

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::infra::oracle::ValueOptions;
use crate::templates::format;

/// Points for a correct over or under pick, and for par; as the oracle scores.
pub const OVER_UNDER_POINTS: u32 =
    crate::domain::leaderboard::progress::OVER_OR_UNDER_POINTS as u32;
pub const PAR_POINTS: u32 = crate::domain::leaderboard::progress::PAR_POINTS as u32;

/// How often an open window's picks and leaderboard refresh.
pub const LIVE_REFRESH: &str = "every 60s";

/// A metric players can pick, with how it is compared and shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    TempHigh,
    TempLow,
    WindSpeed,
}

impl Metric {
    pub const ALL: [Metric; 3] = [Metric::TempHigh, Metric::TempLow, Metric::WindSpeed];

    /// The oracle's name for the metric.
    pub fn id(self) -> &'static str {
        match self {
            Metric::TempHigh => "temp_high",
            Metric::TempLow => "temp_low",
            Metric::WindSpeed => "wind_speed",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Metric::TempHigh => "High temperature",
            Metric::TempLow => "Low temperature",
            Metric::WindSpeed => "Wind speed",
        }
    }

    /// What the reading so far is, part-way through the window: the highest
    /// high and wind, the lowest low.
    pub fn so_far(self) -> &'static str {
        match self {
            Metric::TempHigh => "High so far",
            Metric::TempLow => "Low so far",
            Metric::WindSpeed => "Top wind so far",
        }
    }

    /// A value in this metric's unit. The oracle reads wind in knots.
    pub fn value(self, value: f64) -> String {
        match self {
            Metric::TempHigh | Metric::TempLow => format!("{:.0}°F", value.round()),
            Metric::WindSpeed if value.fract() == 0.0 => format!("{value:.0} knots"),
            Metric::WindSpeed => format!("{value:.1} knots"),
        }
    }
}

/// How a pick fared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No reading yet (or none was recorded).
    Pending,
    Hit(u32),
    Miss,
}

/// How a pick stands while its window is open. The leaderboard seam derives
/// it from each metric's direction: a high or a wind reading can only rise
/// during the window, a low can only fall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickState {
    /// No reading or forecast yet.
    Pending,
    /// Right, and nothing left in the window can change that.
    LockedIn,
    /// Right if the window ended now, but it can still change.
    OnTrack,
    /// Wrong if the window ended now, but it can still come right.
    OffTrack,
    /// Wrong, and it can no longer come right.
    Out,
    /// The window has closed; the result is the final one.
    Final,
}

impl PickState {
    fn badge(self) -> (&'static str, &'static str) {
        match self {
            PickState::Pending => ("pick-state", "Waiting for readings"),
            PickState::LockedIn => ("pick-state is-locked-in", "Locked in"),
            PickState::OnTrack => ("pick-state is-on-track", "On track"),
            PickState::OffTrack => ("pick-state is-off-track", "Off track"),
            PickState::Out => ("pick-state is-out", "Out"),
            PickState::Final => ("pick-state is-final", "Final"),
        }
    }

    fn row_class(self) -> &'static str {
        match self {
            PickState::LockedIn => "scored-pick is-live is-locked-in",
            PickState::Out => "scored-pick is-live is-out",
            _ => "scored-pick is-live",
        }
    }
}

/// A pick part-way through its window: the reading so far and how the pick
/// stands on it.
#[derive(Debug, Clone, PartialEq)]
pub struct PickProgress {
    /// Running highest (high, wind) or lowest (low) reading so far.
    pub observed_so_far: Option<f64>,
    /// Latest report's position within the window, of `hours_total`; gaps may exist.
    pub hours_covered: u32,
    pub hours_total: u32,
    pub state: PickState,
}

/// One pick and the values it was judged on.
#[derive(Debug, Clone)]
pub struct PickView {
    pub station_id: String,
    pub station_name: Option<String>,
    pub metric: Metric,
    pub pick: ValueOptions,
    pub forecast: Option<f64>,
    /// Authoritative per-pick points and hit state from the leaderboard service.
    pub score: u32,
    pub hit: bool,
    /// The final reading, once the window has closed.
    pub observed: Option<f64>,
    /// Set while the window is open.
    pub progress: Option<PickProgress>,
}

impl PickView {
    pub fn outcome(&self) -> Outcome {
        if self.forecast.is_none() || self.observed.is_none() {
            Outcome::Pending
        } else if self.hit {
            Outcome::Hit(self.score)
        } else {
            Outcome::Miss
        }
    }

    /// Whether the pick is being judged on readings so far.
    fn live(&self) -> Option<&PickProgress> {
        self.progress
            .as_ref()
            .filter(|progress| progress.state != PickState::Final)
    }

    fn observed(&self) -> Option<f64> {
        match self.live() {
            Some(progress) => progress.observed_so_far,
            None => self.observed,
        }
    }

    /// Points now: final ones, or as if the window ended now.
    fn points_now(&self) -> u32 {
        self.score
    }
}

/// Where the entry's competition is, for the readings the dialog can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readings {
    /// The window hasn't opened.
    NotStarted,
    /// The window is open: readings so far, scored as if it ended now.
    Live {
        /// When the readings so far last changed, if known.
        updated_at: Option<OffsetDateTime>,
    },
    /// The window has closed; the oracle may still record its last readings.
    Closed,
    /// The competition is over; a missing reading will not come.
    Final,
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
/// every minute; the reload after the window closes carries no trigger, so
/// the polling stops there.
pub fn picks_detail(
    entry_id: &str,
    picks: &[PickView],
    readings: Readings,
    now: OffsetDateTime,
) -> Markup {
    let live = matches!(readings, Readings::Live { .. });
    let any_observed = picks.iter().any(|pick| pick.observed().is_some());
    let total: u32 = picks.iter().map(PickView::points_now).sum();
    let mut stations: Vec<&str> = Vec::new();
    for pick in picks {
        if !stations.contains(&pick.station_id.as_str()) {
            stations.push(&pick.station_id);
        }
    }
    let url = detail_url(entry_id);
    html! {
        div class="picks-detail"
            hx-get=[live.then_some(&url)] hx-trigger=[live.then_some(LIVE_REFRESH)]
            hx-target=[live.then_some("this")] hx-swap=[live.then_some("outerHTML")]
            hx-disinherit=[live.then_some("*")] {
            div class="entry-detail-header" {
                div {
                    h2 class="title is-5 mb-1" { "Picks" }
                    span class="entry-id" { "Entry " (format::copyable_id(entry_id)) }
                }
                @if any_observed {
                    div class="entry-detail-score" {
                        (total) " pts"
                        @match readings {
                            Readings::Live { .. } => { span class="fact-note" { "so far" } }
                            Readings::Closed | Readings::Final => {
                                span class="fact-note" { (PickState::Final.badge().1) }
                            }
                            Readings::NotStarted => {}
                        }
                    }
                }
            }
            @if let Readings::Live { updated_at } = readings {
                p class="provisional-note" {
                    "Provisional: scored as if the window ended now"
                    @if let Some(at) = updated_at {
                        " · updated " (format::ago(at, now))
                    }
                    "."
                }
            }
            @if picks.is_empty() {
                p class="empty-state" {
                    @if matches!(readings, Readings::NotStarted) { "Picks become public when entries close." }
                    @else { "No picks recorded." }
                }
            } @else if !any_observed {
                p class="entry-pending-msg mb-3" {
                    @match readings {
                        Readings::NotStarted => "Readings appear here once the window opens.",
                        Readings::Live { .. } | Readings::Closed => "Readings appear here as the oracle records them.",
                        Readings::Final => "No readings were recorded for these stations in the window, so no pick scored.",
                    }
                }
            }
            @for station in &stations {
                @let station_picks: Vec<&PickView> = picks.iter().filter(|pick| pick.station_id == *station).collect();
                section class="picks-station" {
                    h3 class="picks-station-name" {
                        @if let Some(name) = station_picks.first().and_then(|pick| pick.station_name.as_deref()) {
                            (name) " "
                        }
                        span class="station-code" { (station) }
                    }
                    @for pick in station_picks {
                        (pick_row(pick))
                    }
                }
            }
        }
    }
}

fn forecast_value(pick: &PickView) -> Markup {
    html! {
        "Forecast "
        strong { (pick.forecast.map(|value| pick.metric.value(value)).unwrap_or_else(|| "—".into())) }
    }
}

fn pick_row(pick: &PickView) -> Markup {
    if let Some(progress) = pick.live() {
        return live_pick_row(pick, progress);
    }
    let (class, result) = match pick.outcome() {
        Outcome::Hit(points) => ("scored-pick is-hit", format!("✓ +{points}")),
        Outcome::Miss => ("scored-pick is-miss", "✗ 0".to_owned()),
        Outcome::Pending => ("scored-pick", String::new()),
    };
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
                (result)
                @if pick.observed.is_some() { span class="pick-state is-final" { "Final" } }
            }
        }
    }
}

/// `Forecast 69°F · High so far 71°F · 9 of 24 h`, and how the pick stands.
fn live_pick_row(pick: &PickView, progress: &PickProgress) -> Markup {
    let (badge_class, badge) = progress.state.badge();
    html! {
        div class=(progress.state.row_class()) {
            span class="pick-metric" { (pick.metric.label()) }
            span class="pick-values" {
                (forecast_value(pick))
                @match progress.observed_so_far {
                    Some(value) => { " · " (pick.metric.so_far()) " " strong { (pick.metric.value(value)) } }
                    None => { " · no reading yet" }
                }
                @if progress.hours_total > 0 {
                    " · reports through " (progress.hours_covered.min(progress.hours_total)) " of " (progress.hours_total) " h"
                }
            }
            span class="pick-choice" { (pick_label(&pick.pick)) }
            span class="pick-result" {
                @if progress.observed_so_far.is_some() {
                    span class=(badge_class) { (badge) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-24 12:52 UTC);

    fn pick(
        metric: Metric,
        choice: ValueOptions,
        forecast: f64,
        observed: Option<f64>,
    ) -> PickView {
        let metric_for_score = crate::domain::leaderboard::Metric::from_id(metric.id()).unwrap();
        let score = crate::domain::leaderboard::progress::points(
            &choice,
            metric_for_score,
            Some(forecast),
            observed,
        ) as u32;
        PickView {
            station_id: "KJFK".into(),
            station_name: Some("John F Kennedy International Airport".into()),
            metric,
            pick: choice,
            forecast: Some(forecast),
            score,
            hit: score > 0,
            observed,
            progress: None,
        }
    }

    fn live(mut view: PickView, so_far: Option<f64>, state: PickState) -> PickView {
        view.score = crate::domain::leaderboard::progress::points(
            &view.pick,
            crate::domain::leaderboard::Metric::from_id(view.metric.id()).unwrap(),
            view.forecast,
            so_far,
        ) as u32;
        view.hit = view.score > 0;
        view.progress = Some(PickProgress {
            observed_so_far: so_far,
            hours_covered: 9,
            hours_total: 24,
            state,
        });
        view
    }

    #[test]
    fn a_finished_entry_shows_readings_hits_and_misses() {
        let picks = vec![
            pick(Metric::TempHigh, ValueOptions::Under, 69.0, Some(55.04)),
            pick(Metric::WindSpeed, ValueOptions::Over, 18.0, Some(11.0)),
        ];
        let html = picks_detail("01a0-entry-1234abcd", &picks, Readings::Final, NOW).into_string();
        assert!(html.contains("John F Kennedy International Airport"));
        assert!(html.contains("Forecast <strong>69°F</strong> · Observed <strong>55°F</strong>"));
        assert!(html.contains("18 knots"));
        assert!(html.contains("✓ +10"));
        assert!(html.contains("✗ 0"));
        assert!(html.contains("10 pts"));
        assert!(html.contains("Final"));
        assert!(!html.contains("mph"));
        assert!(!html.contains("Scores available after"));
        assert!(
            !html.contains("hx-trigger"),
            "a finished entry does not refresh"
        );
    }

    #[test]
    fn missing_readings_say_whether_they_can_still_come() {
        let picks = vec![pick(Metric::TempLow, ValueOptions::Par, 55.0, None)];
        let before = picks_detail("e", &picks, Readings::NotStarted, NOW).into_string();
        assert!(before.contains("once the window opens"));
        let waiting = picks_detail("e", &picks, Readings::Closed, NOW).into_string();
        assert!(waiting.contains("as the oracle records them"));
        let over = picks_detail("e", &picks, Readings::Final, NOW).into_string();
        assert!(over.contains("No readings were recorded"));
    }

    #[test]
    fn an_open_window_shows_readings_so_far_and_how_each_pick_stands() {
        let picks = vec![
            live(
                pick(Metric::TempHigh, ValueOptions::Over, 69.0, None),
                Some(71.0),
                PickState::LockedIn,
            ),
            live(
                pick(Metric::TempLow, ValueOptions::Par, 55.0, None),
                Some(55.0),
                PickState::OnTrack,
            ),
            live(
                pick(Metric::WindSpeed, ValueOptions::Under, 12.0, None),
                Some(14.0),
                PickState::Out,
            ),
            live(
                pick(Metric::TempHigh, ValueOptions::Par, 60.0, None),
                Some(58.0),
                PickState::OffTrack,
            ),
        ];
        let updated = Readings::Live {
            updated_at: Some(NOW - time::Duration::minutes(12)),
        };
        let html = picks_detail("e1", &picks, updated, NOW).into_string();
        assert!(html.contains(
            "Forecast <strong>69°F</strong> · High so far <strong>71°F</strong> · 9 of 24 h"
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
        let picks = vec![live(
            pick(Metric::TempLow, ValueOptions::Par, 55.0, None),
            None,
            PickState::OffTrack,
        )];
        let html =
            picks_detail("e1", &picks, Readings::Live { updated_at: None }, NOW).into_string();
        assert!(html.contains("no reading yet"));
        assert!(!html.contains("Off track"));
        assert!(html.contains("as the oracle records them"));
    }

    #[test]
    fn a_closed_window_shows_final_results() {
        let picks = vec![live(
            pick(Metric::TempHigh, ValueOptions::Over, 69.0, Some(71.0)),
            Some(71.0),
            PickState::Final,
        )];
        let html = picks_detail("e1", &picks, Readings::Closed, NOW).into_string();
        assert!(html.contains("Observed <strong>71°F</strong>"));
        assert!(html.contains("✓ +10"));
        assert!(html.contains("Final"));
        assert!(!html.contains("hx-trigger"));
    }
}
