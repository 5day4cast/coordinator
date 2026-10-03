//! Server-rendered map; a small optional script switches layers and selects stations.
use crate::infra::admin_weather::{Candidate, Filters, Window};
use maud::{html, Markup};

pub fn project(lon: f64, lat: f64) -> (f64, f64) {
    (
        lon,
        -lat.clamp(-85.0, 85.0)
            .to_radians()
            .tan()
            .asinh()
            .to_degrees(),
    )
}

/// Where the discovery map is served, apart from the page that lists its stations.
pub fn map_link(filters: &Filters, window: &Window) -> String {
    super::discovery::nearby_link(filters, window, &filters.near, &filters.weather).replacen(
        "/admin/competition?",
        "/admin/competition/map?",
        1,
    )
}

/// The map's place in the game form. The map is a large SVG with a marker for every
/// eligible station, so it loads after the station cards. Without scripting it opens
/// as its own page.
pub fn map_slot(filters: &Filters, window: &Window) -> Markup {
    let link = map_link(filters, window);
    html! {
        section.weather-map-slot hx-get=(link) hx-trigger="load" hx-swap="outerHTML" {
            h2 { "Read the weather on the map" }
            p.note role="status" { "Loading the station map…" }
            noscript { p { a href=(link) { "Open the station map" } " to compare stations by forecast. Its station links open a nearby-station search." } }
        }
    }
}

/// The map. `standalone` is the page opened without scripting, outside the game form:
/// its station links search near that station rather than selecting it.
pub fn weather_map(
    candidates: &[&Candidate],
    filters: &Filters,
    window: &Window,
    usable: bool,
    standalone: bool,
) -> Markup {
    let points: Vec<_> = candidates
        .iter()
        .filter(|c| {
            let s = &c.eligible.station;
            s.latitude.is_finite()
                && s.longitude.is_finite()
                && (-90.0..=90.0).contains(&s.latitude)
                && (-180.0..=180.0).contains(&s.longitude)
        })
        .collect();
    if points.is_empty() {
        return html! { p.note { "No station coordinates available for the map." } };
    }
    let mut bounds = (180.0_f64, 180.0_f64, -180.0_f64, -180.0_f64);
    for c in &points {
        let s = &c.eligible.station;
        let (x, y) = project(s.longitude, s.latitude);
        bounds = (
            bounds.0.min(x),
            bounds.1.min(y),
            bounds.2.max(x),
            bounds.3.max(y),
        );
    }
    let width = (bounds.2 - bounds.0 + 8.0).max(12.0);
    let height = (bounds.3 - bounds.1 + 8.0).max(width / 2.1);
    let view = format!(
        "{} {} {width} {height}",
        (bounds.0 + bounds.2 - width) / 2.0,
        (bounds.1 + bounds.3 - height) / 2.0
    );
    let radius = width / 180.0;
    html! {
        section.weather-map data-usable=(usable.to_string()) {
            h2 { "Read the weather on the map" }
            @if standalone {
                p.note { "Select a station to compare stations near it. Colors use the same scale at every forecast time." }
                p { a href=(super::discovery::nearby_link(filters, window, &filters.near, &filters.weather)) { "Back to the station list" } }
            } @else {
                p.note { "Select a station to add it to this game. Zoom and drag to compare nearby airports. Colors use the same scale at every forecast time." }
            }
            div.map-controls {
                label { "Color layer" select data-map-layer disabled {
                    option value="high" { "High temperature" } option value="low" { "Low temperature" }
                    option value="wind" { "Wind speed" } option value="rain" { "Precipitation chance" }
                } }
                label { "Forecasts covering (UTC)" select data-map-time disabled {
                    option value="" { "Whole game window" }
                    @for hour in (0..(window.end-window.start).whole_hours()).step_by(6) {
                        @let at = window.start + time::Duration::hours(hour);
                        option value=(at.unix_timestamp()*1000) { (at.date()) " · " (format!("{:02}:00",at.hour())) }
                    }
                } }
                label.check { input type="checkbox" data-map-wind checked disabled; " Wind direction" }
                div.map-buttons {
                    button type="button" data-map-zoom="0.6" disabled aria-label="Zoom in" { "+" }
                    button type="button" data-map-zoom="1.6" disabled aria-label="Zoom out" { "−" }
                    button type="button" data-map-reset disabled { "Fit stations" }
                }
            }
            div.map-legend data-map-legend { "Temperature °F: <32 · 32–49 · 50–64 · 65–79 · 80–94 · ≥95" }
            svg.weather-canvas xmlns="http://www.w3.org/2000/svg" viewBox=(view) aria-label="Eligible weather stations on a geographic map" {
                path.map-land d=(include_str!("map-land.path")) {}
                @for lon in (-180..180).step_by(10) { path.map-grid d=(format!("M{lon},-180V180")) {} }
                @for lat in (-80..81).step_by(10) { @let y = project(0.0,f64::from(lat)).1; path.map-grid d=(format!("M-180,{y}H180")) {} }
                @for c in points {
                    @let s = &c.eligible.station;
                    @let (x,y) = project(s.longitude,s.latitude);
                    a.map-station href=(super::discovery::nearby_link(filters,window,&s.station_id,&filters.weather))
                        data-station=(s.station_id) data-name=(s.station_name) data-high=(c.high) data-low=(c.low)
                        data-wind=[c.wind_knots] data-rain=[c.rain_chance]
                        data-forecasts=(serde_json::to_string(&c.forecasts).unwrap_or_default()) transform=(format!("translate({x},{y})")) {
                        title { (s.station_id) " · " (s.station_name) " · " (c.low) "–" (c.high) " °F" }
                        circle r=(radius) fill=(temperature_color(c.high)) {}
                        path.map-wind d=(format!("M0,{}V{}M{},0L0,{}L{},0",radius*2.5,-radius*2.5,-radius,-radius*2.5,radius)) hidden {}
                    }
                }
            }
            p.map-inspector data-map-inspector role="status" { "Hover or focus a station for its forecast. With scripting disabled, map links open a nearby-station search; use the station checkboxes below to select." }
            @if !standalone { p.note data-map-count aria-live="polite" { "0 stations selected" } }
            p.note { "Station forecasts, not a continuous weather field. High/low temperatures describe forecast periods, not the temperature at the selected instant. Arrows point downwind and appear only for a timed forecast with known direction. Pressure centers and analyzed fronts: " a href="https://www.wpc.ncep.noaa.gov/html/sfc2.shtml" rel="noreferrer" { "NOAA surface analysis" } ". Map: Natural Earth (public domain)." }
        }
        script src=(crate::templates::assets::WEATHER_MAP_JS.url) defer {}
    }
}
fn temperature_color(value: i64) -> &'static str {
    match value {
        ..=31 => "#6865c7",
        32..=49 => "#408cca",
        50..=64 => "#39a99b",
        65..=79 => "#dfbb4d",
        80..=94 => "#ed853c",
        _ => "#d34857",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::admin_weather::Forecast;

    fn candidate() -> Candidate {
        Candidate {
            eligible: serde_json::from_value(serde_json::json!({
                "station_id":"KPDX", "station_name":"Portland", "state":"OR", "iata_id":"PDX",
                "latitude":45.58, "longitude":-122.6, "clean_days":3, "days_checked":3,
                "last_report":"2026-10-02T10:00:00Z", "forecast_through":"2026-10-05T00:00:00Z"
            }))
            .unwrap(),
            high: 68,
            low: 50,
            wind_knots: Some(8),
            rain_chance: None,
            forecasts: vec![Forecast {
                station_id: "KPDX".into(),
                temp_high: 68,
                temp_low: 50,
                wind_speed: Some(8),
                precip_chance: None,
                wind_direction: Some(270),
                start_time: Some("2026-10-03T00:00:00Z".into()),
                end_time: Some("2026-10-04T00:00:00Z".into()),
            }],
        }
    }

    #[test]
    fn the_map_loads_separately_and_opens_as_a_page_without_scripting() {
        let filters = Filters {
            location: "Portland".into(),
            weather: "wind".into(),
            ..Default::default()
        };
        let window = filters
            .window(time::macros::datetime!(2026-10-02 12:00 UTC))
            .unwrap();
        let link = map_link(&filters, &window);
        assert!(link.starts_with("/admin/competition/map?day=2026-10-03&"));
        assert!(link.contains("location=Portland") && link.contains("weather=wind"));

        let slot = map_slot(&filters, &window).into_string();
        assert!(slot.contains("hx-trigger=\"load\""));
        assert!(slot.contains("<noscript>"));
        assert!(!slot.contains("<svg") && !slot.contains("class=\"weather-map\""));

        let candidate = candidate();
        let fragment = weather_map(&[&candidate], &filters, &window, true, false).into_string();
        assert!(fragment.contains("data-map-count"));
        assert!(fragment.contains("data-station=\"KPDX\""));
        // Forecast rows are carried by their station's marker without repeating its id.
        assert!(fragment.contains("temp_high"));
        assert!(!fragment.contains("station_id"));

        let page = weather_map(&[&candidate], &filters, &window, true, true).into_string();
        assert!(!page.contains("data-map-count"));
        assert!(page.contains("Back to the station list</a>"));
        assert!(page.contains("href=\"/admin/competition?day=2026-10-03"));
    }
}
