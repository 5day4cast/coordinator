use maud::{html, Markup};

use crate::templates::format::temperature_band;

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

/// A pin and what the map says beside it.
#[derive(Debug, Clone)]
pub struct MapMark<'a> {
    pub pin: &'a StationPin,
    /// A temperature in °F: the glow around the pin and the caption take its colour.
    pub temperature: Option<f64>,
    /// Under the city's name, in pixel numerals: the forecast high and low, or the readings
    /// so far, each temperature in its colour (`tspan`s).
    pub caption: Option<Markup>,
}

const MAP_WIDTH: f64 = 599.96;
const MAP_HEIGHT: f64 = 327.28;
/// The shown part's proportions: wider than the country, so the map stays short on a page.
const VIEW_ASPECT: f64 = 2.4;

/// The weather map: the stations' part of the country at night, a glow around each station in
/// its temperature's colour, and each city named with its caption. On the entry form (`linked`)
/// each pin links to that station's picks further down; no script is involved.
pub fn station_map(marks: &[MapMark], linked: bool) -> Markup {
    let bands: std::collections::BTreeSet<&str> = marks
        .iter()
        .filter_map(|mark| mark.temperature.map(temperature_band))
        .collect();
    let view = View::around(marks);
    // Pins, glows and type keep their size on screen however far the map zooms in.
    let k = view.width / MAP_WIDTH;
    let labels = place_labels(marks, &view, k);
    html! {
        figure class="station-map" {
            svg class="forecast-map"
                viewBox=(format!("{:.1} {:.1} {:.1} {:.1}", view.x, view.y, view.width, view.height))
                font-size=(format!("{:.2}", 17.0 * k))
                role="group"
                aria-label=(if linked { "The contest's cities: choose one to jump to its picks" } else { "The contest's cities" }) {
                defs {
                    @for band in &bands {
                        radialGradient id=(format!("fm-glow-{band}")) class=(format!("fm-glow {band}")) {
                            stop offset="0" stop-opacity="0.9" {}
                            stop offset="0.4" stop-opacity="0.45" {}
                            stop offset="1" stop-opacity="0" {}
                        }
                    }
                }
                rect class="fm-sea" width=(MAP_WIDTH) height=(MAP_HEIGHT) {}
                g class="fm-grid" aria-hidden="true" {
                    @for x in (1..24).map(|step| f64::from(step) * 25.0) {
                        line x1=(x) y1="0" x2=(x) y2=(MAP_HEIGHT) {}
                    }
                    @for y in (1..14).map(|step| f64::from(step) * 25.0) {
                        line x1="0" y1=(y) x2=(MAP_WIDTH) y2=(y) {}
                    }
                }
                use class="fm-land" href=(format!("{}#Layer_1", crate::templates::assets::USA_MAP_SVG.url)) {}
                g class="fm-glows" aria-hidden="true" {
                    @for mark in marks {
                        @if let Some(temperature) = mark.temperature {
                            circle cx=(format!("{:.1}", mark.pin.svg_x)) cy=(format!("{:.1}", mark.pin.svg_y))
                                   r=(format!("{:.1}", 80.0 * k))
                                   fill=(format!("url(#fm-glow-{})", temperature_band(temperature))) {}
                        }
                    }
                }
                g class="station-pins" {
                    @for (mark, label) in marks.iter().zip(&labels) {
                        @if linked {
                            a href=(format!("#station-{}", mark.pin.station_id))
                              aria-label=(format!("{}: jump to picks", mark.pin.name)) {
                                (pin(mark, label, k))
                            }
                        } @else {
                            g { (pin(mark, label, k)) }
                        }
                    }
                }
            }
        }
    }
}

/// The part of the map shown: the stations with room for their glows and names, or the
/// country's width when they are spread across it.
struct View {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl View {
    fn around(marks: &[MapMark]) -> Self {
        let whole = View {
            x: 0.0,
            y: 0.0,
            width: MAP_WIDTH,
            height: MAP_WIDTH / VIEW_ASPECT,
        };
        let xs = marks.iter().map(|mark| mark.pin.svg_x);
        let ys = marks.iter().map(|mark| mark.pin.svg_y);
        let (Some(left), Some(right)) = (xs.clone().reduce(f64::min), xs.reduce(f64::max)) else {
            return whole;
        };
        let (top, bottom) = (
            ys.clone().reduce(f64::min).unwrap_or(0.0),
            ys.reduce(f64::max).unwrap_or(MAP_HEIGHT),
        );
        // Names go to the right of their pins, so more room there.
        let (x0, x1, y0, y1) = (left - 90.0, right + 150.0, top - 60.0, bottom + 70.0);
        let width = (x1 - x0)
            .max((y1 - y0) * VIEW_ASPECT)
            .clamp(280.0, MAP_WIDTH);
        let height = width / VIEW_ASPECT;
        View {
            x: ((x0 + x1 - width) / 2.0).clamp(0.0, MAP_WIDTH - width),
            y: ((y0 + y1 - height) / 2.0).clamp(0.0, MAP_HEIGHT - height),
            width,
            height,
        }
    }
}

/// Where a pin's name and caption go: beside it, moved up or down clear of the ones before.
struct Label {
    x: f64,
    y: f64,
    anchor: &'static str,
}

fn place_labels(marks: &[MapMark], view: &View, k: f64) -> Vec<Label> {
    let mut taken: Vec<(f64, f64, f64, f64)> = Vec::new();
    marks
        .iter()
        .map(|mark| {
            let (x, y) = (mark.pin.svg_x, mark.pin.svg_y);
            // Wide enough for the phone's larger type (entry_form.css).
            let width = 13.0 * k * map_city(&mark.pin.name).chars().count().max(6) as f64;
            let height = if mark.caption.is_some() { 50.0 } else { 26.0 } * k;
            let left = x + 14.0 * k + width > view.x + view.width;
            let (start, end) = if left {
                (x - 14.0 * k - width, x - 14.0 * k)
            } else {
                (x + 14.0 * k, x + 14.0 * k + width)
            };
            let (lowest, highest) = (view.y + 4.0 * k, view.y + view.height - height - 4.0 * k);
            let top = [-10.0, 22.0, -42.0, 54.0, -74.0]
                .into_iter()
                .map(|offset| (y + offset * k).clamp(lowest, highest.max(lowest)))
                .find(|top| {
                    !taken.iter().any(|&(x0, y0, x1, y1)| {
                        start < x1 && end > x0 && *top < y1 && top + height > y0
                    })
                })
                .unwrap_or(y - 10.0 * k);
            taken.push((start, top, end, top + height));
            Label {
                x: if left { end } else { start },
                y: top + 18.0 * k,
                anchor: if left { "end" } else { "start" },
            }
        })
        .collect()
}

/// `Chicago` for `Chicago/O'Hare International, IL`: the map is too small for states and
/// airports, which the picks below name.
fn map_city(name: &str) -> String {
    let city = crate::templates::format::city_name(name);
    city.split(", ").next().unwrap_or(&city).to_owned()
}

fn pin(mark: &MapMark, label: &Label, k: f64) -> Markup {
    let (x, y) = (mark.pin.svg_x, mark.pin.svg_y);
    let band = mark.temperature.map(temperature_band);
    html! {
        title { (mark.pin.name) }
        // The pin's tap on a touch screen (entry_form.css).
        rect class="station-pin-hit" x=(format!("{:.1}", x - 0.5)) y=(format!("{:.1}", y - 0.5))
             width="1" height="1" {}
        circle class=(format!("station-pin-ring {}", band.unwrap_or("")))
               cx=(format!("{x:.1}")) cy=(format!("{y:.1}")) r=(format!("{:.1}", 9.0 * k)) {}
        circle class="station-pin" cx=(format!("{x:.1}")) cy=(format!("{y:.1}")) r=(format!("{:.1}", 5.0 * k)) {}
        text class="station-pin-label" x=(format!("{:.1}", label.x)) y=(format!("{:.1}", label.y))
             text-anchor=(label.anchor) {
            (map_city(&mark.pin.name))
        }
        @if let Some(caption) = &mark.caption {
            text class="station-pin-caption"
                 x=(format!("{:.1}", label.x)) y=(format!("{:.1}", label.y + 26.0 * k))
                 text-anchor=(label.anchor) {
                (caption)
            }
        }
    }
}
