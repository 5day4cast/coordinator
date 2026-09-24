use maud::{html, Markup};

/// Geographic region based on longitude
pub fn get_region(longitude: f64) -> u8 {
    if longitude < -140.0 {
        0 // Alaska/Hawaii
    } else if longitude < -115.0 {
        1 // Pacific
    } else if longitude < -100.0 {
        2 // Mountain
    } else if longitude < -85.0 {
        3 // Central
    } else {
        4 // Eastern
    }
}

/// Get CSS class for region
pub fn region_class(region: u8) -> &'static str {
    match region {
        0 => "region-alaska-hawaii",
        1 => "region-pacific",
        2 => "region-mountain",
        3 => "region-central",
        _ => "region-eastern",
    }
}

/// Get region display name
pub fn region_name(region: u8) -> &'static str {
    match region {
        0 => "Alaska & Hawaii",
        1 => "Pacific",
        2 => "Mountain",
        3 => "Central",
        _ => "Eastern",
    }
}

/// Mercator projection for latitude
fn mercator_lat(lat: f64) -> f64 {
    (std::f64::consts::PI / 4.0 + lat * std::f64::consts::PI / 360.0)
        .tan()
        .ln()
}

/// Convert lat/lon to SVG coordinates for the USA map
/// The SVG is 599.96x327.28 pixels
pub fn lat_lon_to_svg(lat: f64, lon: f64) -> Option<(f64, f64)> {
    const SVG_WIDTH: f64 = 599.96;
    const SVG_HEIGHT: f64 = 327.28;

    // Bounding box for continental US
    const NORTH: f64 = 49.3931;
    const SOUTH: f64 = 24.545874;
    const EAST: f64 = -66.95;
    const WEST: f64 = -124.75;

    // Continental US bounds check
    if !(SOUTH..=NORTH).contains(&lat) || !(WEST..=EAST).contains(&lon) {
        return None;
    }

    // Apply Mercator projection
    let mercator_top = mercator_lat(NORTH);
    let mercator_bottom = mercator_lat(SOUTH);
    let mercator_input = mercator_lat(lat);

    // Normalize and convert to SVG coordinates
    let lat_normalized = (mercator_top - mercator_input) / (mercator_top - mercator_bottom);
    let lon_normalized = (lon - WEST) / (EAST - WEST);

    let x = lon_normalized * SVG_WIDTH;
    let y = lat_normalized * SVG_HEIGHT;

    Some((x.clamp(0.0, SVG_WIDTH), y.clamp(0.0, SVG_HEIGHT)))
}

/// A station to pin on the entry form's map.
#[derive(Debug, Clone)]
pub struct StationPin {
    pub station_id: String,
    /// Short label drawn beside the pin, such as the airport code `PWM`.
    pub label: String,
    pub name: String,
    pub svg_x: f64,
    pub svg_y: f64,
}

/// A small map showing where a competition's stations are. Each pin links to
/// that station's picks further down the form; no script is involved.
pub fn station_map(pins: &[StationPin]) -> Markup {
    html! {
        figure class="station-map" {
            img src=(crate::templates::assets::USA_MAP_SVG.url) alt="" class="usa-map" width="600" height="327";
            svg class="station-pins" viewBox="0 0 599.96 327.28" role="img"
                aria-label="Where this competition's stations are" {
                @for pin in pins {
                    a href=(format!("#station-{}", pin.station_id)) {
                        title { (pin.name) }
                        circle class="station-pin" cx=(format!("{:.1}", pin.svg_x)) cy=(format!("{:.1}", pin.svg_y)) r="6" {}
                        text class="station-pin-label"
                             x=(format!("{:.1}", pin.svg_x + if pin.svg_x > 520.0 { -9.0 } else { 9.0 }))
                             y=(format!("{:.1}", pin.svg_y + 4.0))
                             text-anchor=(if pin.svg_x > 520.0 { "end" } else { "start" }) { (pin.label) }
                    }
                }
            }
        }
    }
}
