//! Human-readable station names for public pages.

use crate::templates::admin::dashboard::Station;
use std::collections::HashMap;

/// A station's name as players know it: `Portland International, ME`.
pub fn station_name(stations: &HashMap<String, Station>, station_id: &str) -> Option<String> {
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
        let stations = HashMap::from([("KPWM".to_owned(), station)]);
        assert_eq!(
            station_name(&stations, "KPWM").as_deref(),
            Some("Portland International, ME")
        );
        assert_eq!(station_name(&stations, "KXXX"), None);
    }
}
