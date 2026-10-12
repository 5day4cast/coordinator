//! The pick on the run's page: which stations, from where, and why.

use maud::{html, Markup};

use super::score::{Role, Storm};
use super::Pick;

pub fn section(pick: &Pick) -> Markup {
    html! {
        section {
            h2 { "Stations" }
            p {
                "Picked for the weather from " (pick.source) ": " (pick.candidates) " candidates, "
                (pick.scored) " with a forecast"
                @if !pick.avoided.is_empty() {
                    ", leaving out " (pick.avoided.len()) " used in the lane's recent competitions"
                }
                "."
            }
            @if !pick.storms.left_out.is_empty() {
                p {
                    "Left out " (pick.storms.left_out.len()) " under or within "
                    (format!("{:.0}", pick.storms.radius_km)) " km of a storm-strength forecast: "
                    (storms(&pick.storms.forecasts)) "."
                }
            }
            div.scroll {
                table {
                    tr {
                        th { "Station" } th { "Why" } th { "From the leader" } th { "Swing" }
                        th { "Wind" } th { "Precipitation" } th { "Extremes" } th { "Score" }
                    }
                    @for picked in &pick.picked {
                        tr {
                            td {
                                (picked.station_id)
                                @if let Some(scored) = &picked.scored {
                                    @if let Some(iata) = scored.station.iata_id.as_deref().filter(|iata| !iata.is_empty()) {
                                        " (" (iata) ")"
                                    }
                                    @if !scored.station.station_name.is_empty() {
                                        br; small { (scored.station.station_name) }
                                    }
                                }
                            }
                            td { (role(picked.role)) }
                            td { @if let Some(km) = picked.km_from_leader { (format!("{km:.0} km")) } }
                            @match &picked.scored {
                                Some(scored) => {
                                    td { (scored.weather.swing_f) " °F" }
                                    td { (format!("{:.0} mph", scored.weather.wind_mph)) }
                                    td {
                                        (scored.weather.precip_chance) "%"
                                        @let amount = scored.weather.rain_in + scored.weather.snow_in + scored.weather.ice_in;
                                        @if amount > 0.0 { (format!(", {amount:.2} in")) }
                                    }
                                    td { (scored.weather.extreme_words().join(", ")) }
                                    td { (format!("{:.2}", scored.components.score)) }
                                }
                                None => { td colspan="5" { "no forecast" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The windiest storm-strength forecasts, a few by name.
fn storms(forecasts: &[Storm]) -> String {
    const NAMED: usize = 5;
    let mut words: Vec<String> = forecasts
        .iter()
        .take(NAMED)
        .map(|storm| {
            format!(
                "{} {:.0} mph, {:.2} in of rain",
                storm.station_id, storm.wind_mph, storm.rain_in
            )
        })
        .collect();
    if forecasts.len() > NAMED {
        words.push(format!("{} more", forecasts.len() - NAMED));
    }
    words.join("; ")
}

fn role(role: Role) -> &'static str {
    match role {
        Role::Leader => "leader: the most weather",
        Role::Cluster => "near the leader",
        Role::Outside => "best of the rest, too few near the leader",
        Role::Spread => "on its own score, 300 km or more from the others",
        Role::Crowded => "best of the rest, too few far apart",
        Role::Fallback => "from the lane's list",
    }
}
