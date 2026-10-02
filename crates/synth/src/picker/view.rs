//! The pick on the run's page: which stations, from where, and why.

use maud::{html, Markup};

use super::score::Role;
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
