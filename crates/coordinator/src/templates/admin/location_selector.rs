use maud::{html, Markup};

use super::dashboard::StationWithWeather;

/// Geographic region based on longitude
fn get_region(longitude: f64) -> u8 {
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
fn region_class(region: u8) -> &'static str {
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
fn lat_lon_to_svg(lat: f64, lon: f64) -> Option<(f64, f64)> {
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

/// Location selector component - map and table views for selecting stations
pub fn location_selector(stations: &[StationWithWeather]) -> Markup {
    html! {
        div class="box" {
            div class="is-flex is-justify-content-space-between is-align-items-center mb-4" {
                h2 class="subtitle is-5 mb-0" { "Select Locations" }

                div class="is-flex is-align-items-center" style="gap: 0.75rem;" {
                    // Clear selection button
                    button
                        type="button"
                        class="button is-small is-outlined is-danger"
                        onclick="clearAllStations()"
                        title="Clear all selections" {
                        span class="icon is-small" { (clear_icon()) }
                        span { "Clear" }
                    }

                    // Selected count badge
                    span id="selected-count" class="tag is-info" { "0 selected" }
                }
            }

            // Map/Table toggle tabs with Create Competition button
            div class="is-flex is-justify-content-space-between is-align-items-end mb-0" {
                div class="tabs is-boxed mb-0" {
                    ul {
                        li class="is-active" data-view="map" onclick="switchLocationView('map')" {
                            a {
                                span class="icon is-small" { (map_icon()) }
                                span { "Map" }
                            }
                        }
                        li data-view="table" onclick="switchLocationView('table')" {
                            a {
                                span class="icon is-small" { (table_icon()) }
                                span { "Table" }
                            }
                        }
                    }
                }

                // Create Competition button next to tabs
                button class="button is-primary" type="submit" {
                    span class="icon is-small" id="submit-indicator" {}
                    span { "Create Competition" }
                }
            }

            // Notification area for success/error messages (below button, above map)
            div id="competition-notification" class="mt-3 mb-3" {}

            // Map view (default)
            div id="location-map-view" {
                (location_map(stations))
            }

            // Table view (hidden by default)
            div id="location-table-view" style="display: none;" {
                (location_table(stations))
            }

            // Container for hidden inputs (populated by JavaScript)
            div id="locations-container" {}
        }
    }
}

/// Map view with clickable station markers and zoom controls
fn location_map(stations: &[StationWithWeather]) -> Markup {
    html! {
        div class="location-map-container" {
            div class="map-wrapper" {
                // Zoomable container
                div class="map-zoomable" {
                    img src="/ui/usa-map.svg" alt="USA Map" class="usa-map";

                    svg class="station-markers" viewBox="0 0 599.96 327.28" preserveAspectRatio="none" {
                        @for sw in stations {
                            @if let Some((x, y)) = lat_lon_to_svg(sw.station.latitude, sw.station.longitude) {
                                @let region = get_region(sw.station.longitude);
                                @let class = region_class(region);
                                circle
                                    class={"station-marker " (class)}
                                    cx=(format!("{:.1}", x))
                                    cy=(format!("{:.1}", y))
                                    r="4"
                                    data-station-id=(sw.station.station_id)
                                    data-station-name=(sw.station.station_name)
                                    data-state=(sw.station.state)
                                    data-iata=(sw.station.iata_id)
                                    data-today-actual-high=(sw.today_actual_high.map(|t| format!("{:.0}", t)).unwrap_or_default())
                                    data-today-actual-low=(sw.today_actual_low.map(|t| format!("{:.0}", t)).unwrap_or_default())
                                    data-today-forecast-high=(sw.today_forecast_high.map(|t| t.to_string()).unwrap_or_default())
                                    data-today-forecast-low=(sw.today_forecast_low.map(|t| t.to_string()).unwrap_or_default())
                                    data-tomorrow-forecast-high=(sw.tomorrow_forecast_high.map(|t| t.to_string()).unwrap_or_default())
                                    data-tomorrow-forecast-low=(sw.tomorrow_forecast_low.map(|t| t.to_string()).unwrap_or_default())
                                    onclick="toggleStationSelection(this, event)" {}
                            }
                        }
                    }
                }

                // Station popup (oracle-style forecast grid)
                div id="station-popup" class="station-popup" style="display: none;" {
                    div class="popup-header" {
                        strong class="popup-station-id" {}
                        span class="popup-iata tag is-iata is-small" {}
                    }
                    div class="popup-name" {}

                    // 3-day compact forecast grid
                    div class="popup-forecast-grid" {
                        // Header row
                        div class="forecast-header-row" {
                            div class="forecast-col-label" {}
                            div class="forecast-col" { "Yesterday" }
                            div class="forecast-col" { "Today" }
                            div class="forecast-col" { "Tomorrow" }
                        }
                        // Temp row - Actual
                        div class="forecast-data-row" {
                            div class="forecast-col-label" { "Actual" }
                            div class="forecast-col" data-field="yesterday-temp-actual" { "-" }
                            div class="forecast-col" data-field="today-temp-actual" { "-" }
                            div class="forecast-col forecast-na" { "-" }
                        }
                        // Temp row - Forecast
                        div class="forecast-data-row" {
                            div class="forecast-col-label" { "Forecast" }
                            div class="forecast-col" data-field="yesterday-temp-forecast" { "-" }
                            div class="forecast-col" data-field="today-temp-forecast" { "-" }
                            div class="forecast-col" data-field="tomorrow-temp-forecast" { "-" }
                        }
                        // Wind row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" { "Wind" }
                            div class="forecast-col" data-field="yesterday-wind" { "-" }
                            div class="forecast-col" data-field="today-wind" { "-" }
                            div class="forecast-col" data-field="tomorrow-wind" { "-" }
                        }
                        // Precipitation chance row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" { "Chance" }
                            div class="forecast-col" data-field="yesterday-precip-chance" { "-" }
                            div class="forecast-col" data-field="today-precip-chance" { "-" }
                            div class="forecast-col" data-field="tomorrow-precip-chance" { "-" }
                        }
                        // Precipitation row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" { "Precip" }
                            div class="forecast-col" data-field="yesterday-rain" { "-" }
                            div class="forecast-col" data-field="today-rain" { "-" }
                            div class="forecast-col" data-field="tomorrow-rain" { "-" }
                        }
                        // Snow row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" { "Snow" }
                            div class="forecast-col" data-field="yesterday-snow" { "-" }
                            div class="forecast-col" data-field="today-snow" { "-" }
                            div class="forecast-col" data-field="tomorrow-snow" { "-" }
                        }
                        // Humidity row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" { "Humidity" }
                            div class="forecast-col" data-field="yesterday-humidity" { "-" }
                            div class="forecast-col" data-field="today-humidity" { "-" }
                            div class="forecast-col" data-field="tomorrow-humidity" { "-" }
                        }
                    }

                    // Loading indicator
                    div class="popup-loading" style="display: none;" { "Loading..." }
                }

                // Zoom controls
                div class="map-zoom-controls" {
                    button type="button" class="button is-small" onclick="zoomIn()" title="Zoom in" {
                        span class="icon is-small" { (plus_icon()) }
                    }
                    span id="zoom-level" class="zoom-level" { "100%" }
                    button type="button" class="button is-small" onclick="zoomOut()" title="Zoom out" {
                        span class="icon is-small" { (minus_icon()) }
                    }
                    button type="button" class="button is-small" onclick="resetZoom()" title="Reset zoom" {
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

            // Zoom hint
            p class="help has-text-centered mt-2" {
                "Scroll to zoom, drag to pan"
            }
        }
    }
}

/// Table view with checkboxes grouped by region
fn location_table(stations: &[StationWithWeather]) -> Markup {
    // Group stations by region
    let mut by_region: Vec<(u8, Vec<&StationWithWeather>)> = Vec::new();

    for sw in stations {
        let region = get_region(sw.station.longitude);
        if let Some((_, region_stations)) = by_region.iter_mut().find(|(r, _)| *r == region) {
            region_stations.push(sw);
        } else {
            by_region.push((region, vec![sw]));
        }
    }

    // Sort regions east to west (reverse order since higher region = further east)
    by_region.sort_by_key(|(r, _)| std::cmp::Reverse(*r));

    html! {
        div class="location-table-container" {
            // Quick actions
            div class="buttons is-small mb-3" {
                button type="button" class="button is-small is-info is-outlined" onclick="selectAllStations()" {
                    "Select All"
                }
                button type="button" class="button is-small is-outlined" onclick="clearAllStations()" {
                    "Clear All"
                }
            }

            // Regions as collapsible sections
            @for (region, region_stations) in &by_region {
                div class={"region-section " (region_class(*region))} {
                    // Region header with select all for region
                    div class="region-header is-flex is-justify-content-space-between is-align-items-center" {
                        label class="checkbox" {
                            input type="checkbox"
                                  class="region-checkbox"
                                  data-region=(*region)
                                  onchange="toggleRegion(this)";
                            strong { " " (region_name(*region)) }
                            span class="tag is-small ml-2" { (region_stations.len()) " stations" }
                        }
                    }

                    // Station list for this region
                    div class="region-stations columns is-multiline is-mobile" {
                        @for sw in region_stations {
                            div class="column is-half-mobile is-one-third-tablet is-one-quarter-desktop" {
                                label class="checkbox station-checkbox-label" {
                                    input type="checkbox"
                                          class="station-checkbox"
                                          data-station-id=(sw.station.station_id)
                                          data-region=(*region)
                                          onchange="updateStationSelection(this)";
                                    " "
                                    strong { (sw.station.station_id.clone()) }
                                    @if !sw.station.state.is_empty() {
                                        span class="is-size-7 has-text-grey ml-1" { "(" (sw.station.state.clone()) ")" }
                                    }
                                    br;
                                    span class="is-size-7 has-text-grey" {
                                        (sw.station.station_name.clone())
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

fn map_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            polygon points="1 6 1 22 8 18 16 22 23 18 23 2 16 6 8 2 1 6" {}
            line x1="8" y1="2" x2="8" y2="18" {}
            line x1="16" y1="6" x2="16" y2="22" {}
        }
    }
}

fn table_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            rect x="3" y="3" width="18" height="18" rx="2" ry="2" {}
            line x1="3" y1="9" x2="21" y2="9" {}
            line x1="3" y1="15" x2="21" y2="15" {}
            line x1="9" y1="3" x2="9" y2="21" {}
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

fn clear_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            line x1="18" y1="6" x2="6" y2="18" {}
            line x1="6" y1="6" x2="18" y2="18" {}
        }
    }
}
