use maud::{html, Markup};

/// Station marker data for map rendering
#[derive(Debug, Clone)]
pub struct StationMarker {
    pub station_id: String,
    pub station_name: String,
    pub state: String,
    pub latitude: f64,
    pub longitude: f64,
    pub svg_x: f64,
    pub svg_y: f64,
    pub region: u8,
    /// Weather data for popup display
    pub weather: Option<StationWeather>,
}

/// Weather data attached to a station marker for popup display
#[derive(Debug, Clone)]
pub struct StationWeather {
    pub forecast_high: Option<f64>,
    pub forecast_low: Option<f64>,
    pub actual_high: Option<f64>,
    pub actual_low: Option<f64>,
    pub wind_speed: Option<f64>,
}

impl StationMarker {
    /// Create a StationMarker from station data, computing SVG coordinates.
    /// Returns None if the station is outside the continental US bounds.
    pub fn new(
        station_id: String,
        station_name: String,
        state: String,
        latitude: f64,
        longitude: f64,
    ) -> Option<Self> {
        let (svg_x, svg_y) = lat_lon_to_svg(latitude, longitude)?;
        let region = get_region(longitude);
        Some(Self {
            station_id,
            station_name,
            state,
            latitude,
            longitude,
            svg_x,
            svg_y,
            region,
            weather: None,
        })
    }
}

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

/// Render a read-only station map with markers
/// Used on the entry form to show station locations.
/// Clicking a marker scrolls to the corresponding station pick section.
pub fn station_map(markers: &[StationMarker]) -> Markup {
    html! {
        div class="location-map-container" {
            div class="map-wrapper" id="entries-map-wrapper" {
                div class="map-zoomable" id="entries-map-zoomable" {
                    img src="/ui/usa-map.svg" alt="USA Map" class="usa-map";

                    svg class="station-markers" viewBox="0 0 599.96 327.28" preserveAspectRatio="none" {
                        @for marker in markers {
                            circle
                                class={"station-marker " (region_class(marker.region))}
                                cx=(format!("{:.1}", marker.svg_x))
                                cy=(format!("{:.1}", marker.svg_y))
                                r="5"
                                data-station-id=(marker.station_id)
                                data-station-name=(marker.station_name)
                                data-state=(marker.state)
                                data-forecast-high=(marker.weather.as_ref().and_then(|w| w.forecast_high).map(|v| format!("{:.0}", v)).unwrap_or_default())
                                data-forecast-low=(marker.weather.as_ref().and_then(|w| w.forecast_low).map(|v| format!("{:.0}", v)).unwrap_or_default())
                                data-actual-high=(marker.weather.as_ref().and_then(|w| w.actual_high).map(|v| format!("{:.0}", v)).unwrap_or_default())
                                data-actual-low=(marker.weather.as_ref().and_then(|w| w.actual_low).map(|v| format!("{:.0}", v)).unwrap_or_default())
                                data-wind=(marker.weather.as_ref().and_then(|w| w.wind_speed).map(|v| format!("{:.0}", v)).unwrap_or_default())
                                onclick=(format!("showStationWeather(this, '{}')", marker.station_id))
                                style="cursor: pointer;" {}
                        }
                    }
                }

                // Station popup with weather data (shown on click)
                div id="entries-station-popup" class="station-popup" style="display: none;" {
                    div class="popup-header" {
                        strong class="popup-station-id" {}
                        button class="delete is-small popup-close" onclick="hideStationPopup()" {}
                    }
                    div class="popup-name" {}

                    // Weather data grid
                    div class="popup-weather-grid" {
                        div class="weather-grid-row weather-grid-header" {
                            div class="weather-grid-label" {}
                            div class="weather-grid-value" { "High" }
                            div class="weather-grid-value" { "Low" }
                        }
                        div class="weather-grid-row" {
                            div class="weather-grid-label" { "Forecast" }
                            div class="weather-grid-value" data-field="forecast-high" { "-" }
                            div class="weather-grid-value" data-field="forecast-low" { "-" }
                        }
                        div class="weather-grid-row" {
                            div class="weather-grid-label" { "Actual" }
                            div class="weather-grid-value" data-field="actual-high" { "-" }
                            div class="weather-grid-value" data-field="actual-low" { "-" }
                        }
                        div class="weather-grid-row" {
                            div class="weather-grid-label" { "Wind" }
                            div class="weather-grid-value" colspan="2" data-field="wind" { "-" }
                        }
                    }

                    div class="popup-action mt-2" {
                        button type="button" class="button is-small is-info is-outlined"
                               onclick="scrollToStationFromPopup()" {
                            "Go to picks"
                        }
                    }
                }

                // Zoom controls
                div class="map-zoom-controls" {
                    button type="button" class="button is-small" onclick="entriesMapZoomIn()" title="Zoom in" {
                        span class="icon is-small" { (plus_icon()) }
                    }
                    span id="entries-zoom-level" class="zoom-level" { "100%" }
                    button type="button" class="button is-small" onclick="entriesMapZoomOut()" title="Zoom out" {
                        span class="icon is-small" { (minus_icon()) }
                    }
                    button type="button" class="button is-small" onclick="entriesMapResetZoom()" title="Reset zoom" {
                        span class="icon is-small" { (reset_icon()) }
                    }
                }
            }

            // Region legend
            div class="region-legend mt-3" {
                span class="legend-item" {
                    span class="legend-dot region-eastern" {}
                    " Eastern"
                }
                span class="legend-item" {
                    span class="legend-dot region-central" {}
                    " Central"
                }
                span class="legend-item" {
                    span class="legend-dot region-mountain" {}
                    " Mountain"
                }
                span class="legend-item" {
                    span class="legend-dot region-pacific" {}
                    " Pacific"
                }
            }

            p class="help has-text-centered mt-2" {
                "Click a station to scroll to its picks"
            }
        }
    }
}

fn plus_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            line x1="12" y1="5" x2="12" y2="19" {}
            line x1="5" y1="12" x2="19" y2="12" {}
        }
    }
}

fn minus_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            line x1="5" y1="12" x2="19" y2="12" {}
        }
    }
}

fn reset_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" {}
            path d="M3 3v5h5" {}
        }
    }
}
