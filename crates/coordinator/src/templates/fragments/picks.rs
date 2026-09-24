//! An entry's picks with the oracle's forecast and reading for each, and
//! whether the pick scored.

use maud::{html, Markup};

use crate::infra::oracle::ValueOptions;
use crate::templates::format;

/// Points for a correct over or under pick, and for par; as the oracle scores.
pub const OVER_UNDER_POINTS: u32 = 10;
pub const PAR_POINTS: u32 = 20;

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

    /// A value in this metric's unit. The oracle reads wind in knots.
    pub fn value(self, value: f64) -> String {
        match self {
            Metric::TempHigh | Metric::TempLow => format!("{:.0}°F", value.round()),
            Metric::WindSpeed if value.fract() == 0.0 => format!("{value:.0} knots"),
            Metric::WindSpeed => format!("{value:.1} knots"),
        }
    }

    /// Temperatures compare in whole degrees; wind compares exactly.
    fn rounded(self) -> bool {
        !matches!(self, Metric::WindSpeed)
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

/// Score a pick the way the oracle does: par when the reading equals the
/// forecast (whole degrees for temperatures), over or under otherwise.
pub fn outcome(metric: Metric, pick: &ValueOptions, forecast: Option<f64>, observed: Option<f64>) -> Outcome {
    let (Some(mut forecast), Some(mut observed)) = (forecast, observed) else {
        return Outcome::Pending;
    };
    if !forecast.is_finite() || !observed.is_finite() {
        return Outcome::Pending;
    }
    if metric.rounded() {
        forecast = forecast.round();
        observed = observed.round();
    }
    let hit = match pick {
        ValueOptions::Par => observed == forecast,
        ValueOptions::Over => observed > forecast,
        ValueOptions::Under => observed < forecast,
    };
    match (hit, pick) {
        (true, ValueOptions::Par) => Outcome::Hit(PAR_POINTS),
        (true, _) => Outcome::Hit(OVER_UNDER_POINTS),
        (false, _) => Outcome::Miss,
    }
}

/// One pick and the values it was judged on.
#[derive(Debug, Clone)]
pub struct PickView {
    pub station_id: String,
    pub station_name: Option<String>,
    pub metric: Metric,
    pub pick: ValueOptions,
    pub forecast: Option<f64>,
    pub observed: Option<f64>,
}

impl PickView {
    pub fn outcome(&self) -> Outcome {
        outcome(self.metric, &self.pick, self.forecast, self.observed)
    }
}

/// Whether readings can still arrive for the entry's competition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readings {
    /// Before or during the window, or while the oracle is still collecting.
    Expected,
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

/// The picks dialog's content.
pub fn picks_detail(entry_id: &str, picks: &[PickView], readings: Readings) -> Markup {
    let any_observed = picks.iter().any(|pick| pick.observed.is_some());
    let total: u32 = picks
        .iter()
        .map(|pick| match pick.outcome() {
            Outcome::Hit(points) => points,
            _ => 0,
        })
        .sum();
    let mut stations: Vec<&str> = Vec::new();
    for pick in picks {
        if !stations.contains(&pick.station_id.as_str()) {
            stations.push(&pick.station_id);
        }
    }
    html! {
        div class="picks-detail" {
            div class="entry-detail-header" {
                div {
                    h2 class="title is-5 mb-1" { "Picks" }
                    span class="entry-id" { "Entry " (format::copyable_id(entry_id)) }
                }
                @if any_observed {
                    span class="entry-detail-score" { (total) " pts" }
                }
            }
            @if picks.is_empty() {
                p class="empty-state" { "No picks recorded." }
            } @else if !any_observed {
                p class="entry-pending-msg mb-3" {
                    @match readings {
                        Readings::Expected => "Readings appear here as the oracle records them.",
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

fn pick_row(pick: &PickView) -> Markup {
    let outcome = pick.outcome();
    let (class, result) = match outcome {
        Outcome::Hit(points) => ("scored-pick is-hit", format!("✓ +{points}")),
        Outcome::Miss => ("scored-pick is-miss", "✗ 0".to_owned()),
        Outcome::Pending => ("scored-pick", String::new()),
    };
    html! {
        div class=(class) {
            span class="pick-metric" { (pick.metric.label()) }
            span class="pick-values" {
                "Forecast "
                strong { (pick.forecast.map(|value| pick.metric.value(value)).unwrap_or_else(|| "—".into())) }
                @if let Some(observed) = pick.observed {
                    " · Observed " strong { (pick.metric.value(observed)) }
                }
            }
            span class="pick-choice" { (pick_label(&pick.pick)) }
            span class="pick-result" { (result) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_matches_the_oracle() {
        use ValueOptions::*;
        // Temperatures round to whole degrees before comparing.
        assert_eq!(outcome(Metric::TempHigh, &Par, Some(55.0), Some(55.04)), Outcome::Hit(20));
        assert_eq!(outcome(Metric::TempHigh, &Over, Some(55.0), Some(55.04)), Outcome::Miss);
        assert_eq!(outcome(Metric::TempLow, &Under, Some(55.0), Some(54.4)), Outcome::Hit(10));
        assert_eq!(outcome(Metric::TempHigh, &Over, Some(69.0), Some(71.0)), Outcome::Hit(10));
        // Wind compares exactly.
        assert_eq!(outcome(Metric::WindSpeed, &Par, Some(18.0), Some(18.0)), Outcome::Hit(20));
        assert_eq!(outcome(Metric::WindSpeed, &Over, Some(18.0), Some(18.4)), Outcome::Hit(10));
        assert_eq!(outcome(Metric::WindSpeed, &Under, Some(18.0), Some(11.0)), Outcome::Hit(10));
        assert_eq!(outcome(Metric::WindSpeed, &Par, Some(18.0), None), Outcome::Pending);
    }

    fn pick(metric: Metric, choice: ValueOptions, forecast: f64, observed: Option<f64>) -> PickView {
        PickView {
            station_id: "KJFK".into(),
            station_name: Some("John F Kennedy International Airport".into()),
            metric,
            pick: choice,
            forecast: Some(forecast),
            observed,
        }
    }

    #[test]
    fn a_finished_entry_shows_readings_hits_and_misses() {
        let picks = vec![
            pick(Metric::TempHigh, ValueOptions::Under, 69.0, Some(55.04)),
            pick(Metric::WindSpeed, ValueOptions::Over, 18.0, Some(11.0)),
        ];
        let html = picks_detail("01a0-entry-1234abcd", &picks, Readings::Final).into_string();
        assert!(html.contains("John F Kennedy International Airport"));
        assert!(html.contains("Forecast <strong>69°F</strong> · Observed <strong>55°F</strong>"));
        assert!(html.contains("18 knots"));
        assert!(html.contains("✓ +10"));
        assert!(html.contains("✗ 0"));
        assert!(html.contains("10 pts"));
        assert!(!html.contains("mph"));
        assert!(!html.contains("Scores available after"));
    }

    #[test]
    fn missing_readings_say_whether_they_can_still_come() {
        let picks = vec![pick(Metric::TempLow, ValueOptions::Par, 55.0, None)];
        let waiting = picks_detail("e", &picks, Readings::Expected).into_string();
        assert!(waiting.contains("as the oracle records them"));
        let over = picks_detail("e", &picks, Readings::Final).into_string();
        assert!(over.contains("No readings were recorded"));
    }
}
