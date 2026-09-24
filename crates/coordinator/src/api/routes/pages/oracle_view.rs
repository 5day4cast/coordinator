//! The oracle's stations as the public pages show them: by id, with the
//! names players know, pinned on the map, and with their forecasts.

use std::collections::HashMap;

use crate::domain::leaderboard::{CompetitionWeather, Metric};
use crate::infra::oracle_weather::Station;
use crate::templates::{
    fragments::entry_form::StationForecast,
    shared_map::{lat_lon_to_svg, StationPin},
};

/// The oracle's stations by id.
pub type StationsById = HashMap<String, Station>;

pub fn stations_by_id(stations: &[Station]) -> StationsById {
    stations
        .iter()
        .map(|station| (station.station_id.clone(), station.clone()))
        .collect()
}

/// Each of `locations` with the forecasts the oracle compares picks with.
pub fn station_forecasts(
    locations: &[String],
    weather: &CompetitionWeather,
    stations: &StationsById,
) -> Vec<StationForecast> {
    locations
        .iter()
        .map(|station_id| StationForecast {
            station_id: station_id.clone(),
            station_name: station_name(stations, station_id),
            forecasts: Metric::ALL
                .iter()
                .map(|metric| (*metric, weather.forecast(station_id, *metric)))
                .collect(),
        })
        .collect()
}

/// Map pins for `locations` the oracle knows.
pub fn station_pins(locations: &[String], stations: &StationsById) -> Vec<StationPin> {
    locations
        .iter()
        .filter_map(|station_id| {
            let station = stations.get(station_id)?;
            let (svg_x, svg_y) = lat_lon_to_svg(station.latitude, station.longitude)?;
            let label = if station.iata_id.trim().is_empty() {
                station_id.trim_start_matches('K').to_owned()
            } else {
                station.iata_id.trim().to_owned()
            };
            Some(StationPin {
                station_id: station_id.clone(),
                label,
                name: station_name(stations, station_id).unwrap_or_else(|| station_id.clone()),
                svg_x,
                svg_y,
            })
        })
        .collect()
}

/// A station's name as players know it: `Portland International, ME`.
pub fn station_name(stations: &StationsById, station_id: &str) -> Option<String> {
    let station = stations.get(station_id)?;
    let name = station
        .station_name
        .split_whitespace()
        .map(|word| match word {
            "Intl" => "International",
            "Rgnl" => "Regional",
            "Muni" => "Municipal",
            "Fld" => "Field",
            "Arpt" => "Airport",
            word => word,
        })
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        return None;
    }
    Some(match station.state.trim() {
        "" => name,
        state => format!("{name}, {state}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_names_are_spelled_out() {
        let station = Station {
            station_id: "KPWM".into(),
            station_name: "Portland Intl".into(),
            state: "ME".into(),
            iata_id: "PWM".into(),
            elevation_m: None,
            latitude: 43.6,
            longitude: -70.3,
        };
        let stations = stations_by_id(&[station]);
        assert_eq!(
            station_name(&stations, "KPWM").as_deref(),
            Some("Portland International, ME")
        );
        assert_eq!(station_name(&stations, "KXXX"), None);
    }
}
