use maud::{html, Markup};
use time::format_description::well_known::Rfc3339;

use crate::infra::{
    admin_weather::{Discovery, Filters, Window},
    refresh_cache::Cached,
};

pub fn discovery(
    filters: &Filters,
    window: &Window,
    data: &Cached<Discovery>,
    network: &str,
) -> Markup {
    let day = window.start.date().to_string();
    let selection = data.value().map(|data| filters.select(data));
    let usable = data
        .latest
        .as_ref()
        .is_some_and(|latest| latest.age().as_secs() <= 900);
    let block_delta = match network {
        "bitcoin" => 144,
        "signet" => 2880,
        _ => 6,
    };
    html! {
        main.admin-workspace {
            header.page-heading {
                div { p.eyebrow { "Competition studio" } h1 { "Find the next weather game" }
                    p { "Choose a window, follow the forecast, and compare stations the oracle can settle." } }
            }
            form.discovery-filters method="get" action="/admin/competition" {
                label { "Day (UTC)" input type="date" name="day" value=(day) required; }
                label { "Observation window" select name="window" {
                    @for (value, title) in [("full", "Full day · 24 hours"), ("day", "Daytime · 12–24 UTC"), ("night", "Nighttime · 00–12 UTC"), ("two_days", "Two days · 48 hours")] {
                        option value=(value) selected[filters.window == value || (filters.window.is_empty() && value == "full")] { (title) }
                    }
                } }
                label { "City, state or airport" input name="location" value=(filters.location) placeholder="Oregon, OR, KPDX…" maxlength="100"; }
                label { "Find weather" select name="weather" {
                    @for (value, title) in [("swing", "Largest temperature range"), ("wind", "Strongest wind"), ("rain", "Highest precipitation chance"), ("hot", "Highest temperatures"), ("cold", "Lowest temperatures")] {
                        option value=(value) selected[filters.weather == value || (filters.weather.is_empty() && value == "swing")] { (title) }
                    }
                } }
                label { "Near station (optional)" input name="near" value=(filters.near) placeholder="KPDX" maxlength="8"; }
                label { "Within (km)" input name="radius_km" type="number" min="25" max="3000" value=(filters.radius_km.unwrap_or(500)); }
                label { "Eligibility history (days)" input name="history_days" type="number" min="1" max="31" value=(window.history_days) required; }
                button type="submit" { "Find games" }
            }
            p.note { "Forecasts are discovery signals. Temperature and wind are scored by the contract; precipitation helps identify weather to watch. Nearby stations are a geographic comparison, not a confirmed storm track." }
            @if let Some(latest) = &data.latest {
                p.note {
                    (latest.value.eligible_count) " eligible stations with forecast coverage through this window. Updated "
                    time datetime=(latest.fetched_at.format(&Rfc3339).unwrap_or_default()) { (latest.fetched_at.format(&Rfc3339).unwrap_or_default()) }
                    ". " (latest.value.missing_forecasts) " without forecast rows."
                    @if data.refreshing { " Refresh in progress; reload to see the new data." }
                    @if !usable { " These results are stale. Refresh before creating a game." }
                }
            }
            @match selection {
                None => div.notice role="status" {
                    @if data.refreshing { "Loading eligible stations and forecasts. Reload this page shortly." }
                    @else { "Weather discovery is unavailable. Retry shortly; the oracle could not supply a complete forecast set." }
                },
                Some(Err(error)) => div.notice role="alert" { (error) },
                Some(Ok(candidates)) => {
                    @if !candidates.is_empty() && filters.near.trim().is_empty() {
                        h2 { "Start with a forecast" }
                        div.metric-grid {
                            @for (kind, label) in [("wind", "Follow stronger wind"), ("rain", "Compare wet-weather forecasts"), ("swing", "Explore temperature range")] {
                                @let value = |c: &&crate::infra::admin_weather::Candidate| match kind {
                                    "wind" => c.wind_knots,
                                    "rain" => c.rain_chance,
                                    _ => Some(c.high - c.low),
                                };
                                @if let Some(lead) = candidates.iter().filter(|c| value(c).is_some()).max_by_key(|c| value(c)) {
                                    div.metric {
                                        h3 { (label) }
                                        p { (lead.eligible.station.station_name) }
                                        p { @match kind {
                                            "wind" => (format!("{:.0} mph forecast wind", lead.wind_knots.unwrap_or_default() as f64 * 1.15078)),
                                            "rain" => (format!("{}% peak precipitation chance", lead.rain_chance.unwrap_or_default())),
                                            _ => (format!("{}–{} °F forecast low and high", lead.low, lead.high)),
                                        } }
                                        a href=(nearby_link(filters, window, &lead.eligible.station.station_id, kind)) { "Compare nearby stations →" }
                                    }
                                }
                            }
                        }
                    }
                    h2 { (candidates.len()) " matching stations" }
                    @if candidates.is_empty() { p { "No stations match this window and these filters. Try a wider area or a shorter window." } }
                    @else {
                        form id="game-creation" method="post" action="/admin/api/competitions" hx-post="/admin/api/competitions" hx-target="#competition-notification" hx-swap="innerHTML" hx-indicator="#creation-progress" {
                            input type="hidden" name="id" id="competitionIdInput" value=(uuid::Uuid::now_v7());
                            input type="hidden" name="start_observation_date" value=(window.start.format(&Rfc3339).unwrap_or_default());
                            input type="hidden" name="end_observation_date" value=(window.end.format(&Rfc3339).unwrap_or_default());
                            input type="hidden" name="signing_date" value=((window.end + time::Duration::HOUR).format(&Rfc3339).unwrap_or_default());
                            input type="hidden" name="history_days" value=(window.history_days);
                            (super::weather_map::weather_map(&candidates, filters, window, usable))
                            div id="map-selections" {}
                            div.station-grid {
                                @for c in candidates.iter().take(100) {
                                    label.station-card {
                                        div.station-heading { input type="checkbox" name="locations" value=(c.eligible.station.station_id) disabled[!usable];
                                            strong { (c.eligible.station.station_name) }
                                        }
                                        p.note { (c.eligible.station.station_id) " · " (c.eligible.station.state) }
                                        dl.weather-facts {
                                            div { dt { "Low → high" } dd { (c.low) " → " (c.high) " °F" } }
                                            div { dt { "Wind" } dd { (c.wind_knots.map(|v| format!("{:.0} mph", v as f64 * 1.15078)).unwrap_or_else(|| "Unknown".into())) } }
                                            div { dt { "Precipitation" } dd { (c.rain_chance.map(|v| format!("{v}%")).unwrap_or_else(|| "Unknown".into())) } }
                                        }
                                        p.quality { (c.eligible.clean_days) "/" (c.eligible.days_checked) " clean observation days" }
                                        p.note { "Latest report " (c.eligible.last_report) }
                                    }
                                }
                            }
                            @if candidates.len() > 100 { p.note { "Showing the first 100 matches. Narrow the location or radius to see others." } }
                            section.creation-panel {
                                h2 { "Set up this game" }
                                p { "Select up to 50 stations above. The server checks their current eligibility again when you create the competition." }
                                p { "Observation: " (window.start.format(&Rfc3339).unwrap_or_default()) " → " (window.end.format(&Rfc3339).unwrap_or_default()) ". Signing starts one hour later." }
                                div.form-grid {
                                    label { "Stake per entry (sats)" input type="number" name="entry_fee" value="5000" min="1" required; }
                                    div { strong { "Required picks" } p.note { "Every weather category for each selected station; calculated from the observation window." } }
                                    label { "Entries per player" input type="number" name="max_entries_per_player" value="1" min="1" required; }
                                    label { "Coordinator fee (%)" input type="number" name="coordinator_fee_percentage" value="5" min="0" max="100" step="0.01" required; }
                                    label { "Minimum pool size" input type="number" name="min_players" value="2" min="2" max="13" required; }
                                    label { "Maximum pool size" input type="number" name="max_pool_size" value="25" min="3" max="25" required; }
                                    label { "Maximum queued entries" input type="number" name="max_entries" value="500" min="2" max="1500" required; }
                                }
                                label.check { input type="checkbox" name="queued" value="true" checked; " Form pools when registration closes" }
                                details {
                                    summary { "Advanced terms and fixed-size games" }
                                    p.note { "Fixed-size terms apply when pool formation is unchecked." }
                                    div.form-grid {
                                        label { "Fixed-size seats" input type="number" name="total_allowed_entries" value="3" min="2" required; }
                                        label { "Winning places" input type="number" name="number_of_places_win" value="1" min="1" required; }
                                        label { "Scoring" select name="scoring_rules" { option value="lines" { "Lines" } option value="fixed" { "Fixed" } } }
                                        label { "Blocks between settlement stages" input type="number" name="relative_locktime_block_delta" value=(block_delta) min="1" required; }
                                    }
                                }
                                button type="submit" disabled[!usable] { "Create competition" }
                                span id="creation-progress" class="htmx-indicator" role="status" { " Checking eligibility and creating…" }
                                div id="competition-notification" role="status" aria-live="polite" {}
                            }
                        }
                    }
                }
            }
        }
    }
}

pub(super) fn nearby_link(
    filters: &Filters,
    window: &Window,
    station: &str,
    weather: &str,
) -> String {
    let mut url = reqwest::Url::parse("http://localhost/admin/competition").expect("static URL");
    url.query_pairs_mut()
        .append_pair("day", &window.start.date().to_string())
        .append_pair("window", &filters.window)
        .append_pair("location", &filters.location)
        .append_pair("near", station)
        .append_pair("radius_km", &filters.radius_km.unwrap_or(500).to_string())
        .append_pair("history_days", &window.history_days.to_string())
        .append_pair("weather", weather);
    format!("{}?{}", url.path(), url.query().unwrap_or_default())
}
