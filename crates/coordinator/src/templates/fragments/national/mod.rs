//! The national forecast on the home page, in the style of the Weather Channel's travel cities
//! screen: the whole country with a high and a low beside each city, and the same cities as a
//! table. The cities come in groups that take turns (national.css), the next contest's first.

use maud::{html, Markup};
use time::{macros::format_description, OffsetDateTime};

use crate::domain::leaderboard::Metric;
use crate::templates::{format::MetricText, shared_map::lat_lon_to_svg};

/// Where the national forecast reloads from, so the lobby's own refresh never restarts it.
pub const NATIONAL_URL: &str = "/national-forecast";

/// Cities shown at once.
const PER_GROUP: usize = 8;
/// Groups that take turns; national.css times up to this many.
const MAX_GROUPS: usize = 4;

/// One city's forecast, placed on the map.
#[derive(Debug, Clone, PartialEq)]
pub struct NationalCity {
    pub station_id: String,
    /// `Chicago`, without its state.
    pub name: String,
    pub svg_x: f64,
    pub svg_y: f64,
    pub high: f64,
    pub low: f64,
    /// One of the next contest's cities.
    pub contest: bool,
}

impl NationalCity {
    /// `None` for a station off the map of the lower 48.
    pub fn new(
        station_id: &str,
        name: &str,
        latitude: f64,
        longitude: f64,
        high: f64,
        low: f64,
    ) -> Option<Self> {
        let (svg_x, svg_y) = lat_lon_to_svg(latitude, longitude)?;
        Some(Self {
            station_id: station_id.to_owned(),
            name: name.to_owned(),
            svg_x,
            svg_y,
            high,
            low,
            contest: false,
        })
    }
}

/// Up to [`MAX_GROUPS`] groups of [`PER_GROUP`] cities, each spread across the country: the map
/// is cut into cells, the cities are taken a cell at a time west to east, and dealt out to the
/// groups in turn. The next contest's cities lead.
pub fn national_groups(
    mut cities: Vec<NationalCity>,
    contest: &[String],
) -> Vec<Vec<NationalCity>> {
    let cell = |city: &NationalCity| {
        let column = (city.svg_x / 100.0).floor() as i64;
        let row = (city.svg_y / 110.0).floor() as i64;
        (column, row)
    };
    cities.sort_by(|a, b| cell(a).cmp(&cell(b)).then(a.station_id.cmp(&b.station_id)));
    let mut cells: Vec<Vec<NationalCity>> = Vec::new();
    for mut city in cities {
        city.contest = contest.contains(&city.station_id);
        match cells.last_mut() {
            Some(last) if cell(&last[0]) == cell(&city) => last.push(city),
            _ => cells.push(vec![city]),
        }
    }
    let mut leading: Vec<NationalCity> = Vec::new();
    let mut spread: Vec<NationalCity> = Vec::new();
    let mut round = 0;
    while cells.iter().any(|cities| round < cities.len()) {
        for cities in &cells {
            if let Some(city) = cities.get(round) {
                if city.contest {
                    leading.push(city.clone());
                } else {
                    spread.push(city.clone());
                }
            }
        }
        round += 1;
    }
    // Dealt out a city to each group in turn, so every group spans west to east.
    let shown = (leading.len() + spread.len()).min(PER_GROUP * MAX_GROUPS);
    let count = shown.div_ceil(PER_GROUP);
    let mut groups: Vec<Vec<NationalCity>> = vec![Vec::new(); count];
    // The contest's cities together in the first group, the rest dealt out after them.
    let lead = leading.len().min(PER_GROUP);
    let mut cities = leading.into_iter().chain(spread).take(shown);
    groups[0].extend(cities.by_ref().take(lead));
    let mut turn = 1;
    for city in cities {
        while groups[turn % count].len() == PER_GROUP {
            turn += 1;
        }
        groups[turn % count].push(city);
        turn += 1;
    }
    groups
}

/// The national forecast for `day`. Nothing while there are no cities to show.
pub fn national_forecast(groups: &[Vec<NationalCity>], day: OffsetDateTime) -> Markup {
    if groups.is_empty() {
        return html! {};
    }
    let date = day
        .format(format_description!(
            "[weekday repr:short] [month repr:short] [day padding:none]"
        ))
        .unwrap_or_default();
    html! {
        section id="nationalForecast" class=(format!("national national-cycle-{}", groups.len()))
            hx-get=(NATIONAL_URL) hx-trigger="every 10m" hx-swap="outerHTML" {
            div class="national-bar" {
                span class="national-title" { "National forecast" }
                span class="national-date pixel" { (date) }
                span class="national-key" {
                    span class=(Metric::TempHigh.color()) { "HI" } " / " span class=(Metric::TempLow.color()) { "LO" }
                }
            }
            div class="national-body" {
                svg class="national-map" viewBox="0 0 599.96 327.28" role="img"
                    aria-label="Forecast highs and lows across the country" {
                    rect class="fm-sea" width="599.96" height="327.28" {}
                    g class="fm-grid" aria-hidden="true" {
                        @for x in (1..24).map(|step| f64::from(step) * 25.0) {
                            line x1=(x) y1="0" x2=(x) y2="327.28" {}
                        }
                        @for y in (1..14).map(|step| f64::from(step) * 25.0) {
                            line x1="0" y1=(y) x2="599.96" y2=(y) {}
                        }
                    }
                    use class="fm-land" href=(format!("{}#Layer_1", crate::templates::assets::USA_MAP_SVG.url)) {}
                    @for (index, group) in groups.iter().enumerate() {
                        g class=(format!("national-group national-group-{index}")) {
                            @for city in group { (map_city(city)) }
                        }
                    }
                }
                div class="national-table" {
                    div class="national-head" aria-hidden="true" {
                        span { "City" } span { "Hi" } span { "Lo" }
                    }
                    div class="national-rows" {
                        @for (index, group) in groups.iter().enumerate() {
                            ul class=(format!("national-group national-group-{index}")) {
                                @for city in group {
                                    li class=[city.contest.then_some("is-contest")] {
                                        span class="national-city" { (city.name) }
                                        span class=(format!("pixel {}", Metric::TempHigh.color())) { (format!("{:.0}", city.high)) }
                                        span class=(format!("pixel {}", Metric::TempLow.color())) { (format!("{:.0}", city.low)) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A city's dot, name and `77/55`, its name to the right unless that runs off the map.
fn map_city(city: &NationalCity) -> Markup {
    let left = city.svg_x > 470.0;
    let x = if left {
        city.svg_x - 7.0
    } else {
        city.svg_x + 7.0
    };
    let anchor = if left { "end" } else { "start" };
    html! {
        g class=(if city.contest { "national-city-mark is-contest" } else { "national-city-mark" }) {
            circle cx=(format!("{:.1}", city.svg_x)) cy=(format!("{:.1}", city.svg_y)) r="3.2" {}
            text class="national-name" x=(format!("{x:.1}")) y=(format!("{:.1}", city.svg_y - 3.0)) text-anchor=(anchor) {
                (city.name)
            }
            text class="national-temps pixel" x=(format!("{x:.1}")) y=(format!("{:.1}", city.svg_y + 9.0)) text-anchor=(anchor) {
                tspan class=(Metric::TempHigh.color()) { (format!("{:.0}", city.high)) }
                "/"
                tspan class=(Metric::TempLow.color()) { (format!("{:.0}", city.low)) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_spread_across_the_country_with_the_contest_first() {
        // Nine stations in the west and nine in the east, one of them in the contest.
        let mut cities: Vec<NationalCity> = (0..9)
            .flat_map(|index| {
                let index = f64::from(index);
                [
                    NationalCity::new(
                        &format!("KW{index}"),
                        "West",
                        40.0,
                        -122.0 + index * 0.1,
                        70.0,
                        50.0,
                    ),
                    NationalCity::new(
                        &format!("KE{index}"),
                        "East",
                        40.0,
                        -75.0 + index * 0.1,
                        60.0,
                        40.0,
                    ),
                ]
            })
            .flatten()
            .collect();
        cities.reverse();
        let groups = national_groups(cities, &["KE8".to_owned(), "KW3".to_owned()]);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups.iter().map(Vec::len).sum::<usize>(), 18);
        // The contest's cities lead the first group together.
        let mut first: Vec<_> = groups[0][..2]
            .iter()
            .map(|city| city.station_id.as_str())
            .collect();
        first.sort();
        assert_eq!(first, ["KE8", "KW3"]);
        assert!(groups[0][..2].iter().all(|city| city.contest));
        // Every full group holds both coasts.
        for group in &groups[..2] {
            assert!(group.iter().any(|city| city.name == "West"));
            assert!(group.iter().any(|city| city.name == "East"));
        }
        assert!(national_forecast(&[], OffsetDateTime::UNIX_EPOCH)
            .into_string()
            .is_empty());
    }
}
