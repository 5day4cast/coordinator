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
    let refresh_path = nearby_link(filters, window, &filters.near, &filters.weather);
    let loading = data.latest.is_none() && data.refreshing;
    let selection = data.value().map(|data| filters.select(data));
    let usable = data
        .latest
        .as_ref()
        .is_some_and(|latest| latest.age().as_secs() <= 900);
    // Weather categories scored at each station: what one station adds to the picks.
    let metrics = crate::domain::WindowShape::of(window.start, window.end)
        .map_or(3, |shape| shape.metrics().len());
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
            // Only empty results poll. Cached results stay in place while the operator
            // selects stations or edits a game; filters also remain outside this region.
            section id="weather-discovery-results"
                hx-get=[loading.then_some(refresh_path.as_str())]
                hx-trigger=[loading.then_some("every 2s")]
                hx-select="#weather-discovery-results" hx-target="this" hx-swap="outerHTML"
                hx-sync="this:drop" hx-push-url="false" aria-busy=(loading.to_string()) {
            @if let Some(latest) = &data.latest {
                p.note {
                    (latest.value.eligible_count) " eligible stations with forecast coverage through this window. Updated "
                    time datetime=(latest.fetched_at.format(&Rfc3339).unwrap_or_default()) { (latest.fetched_at.format(&Rfc3339).unwrap_or_default()) }
                    ". " (latest.value.missing_forecasts) " without forecast rows."
                    @if data.refreshing { " Refresh in progress. Your current station choices stay in place." }
                    @if !usable { " These results are stale. Refresh before creating a game." }
                }
            }
            @if data.refreshing || !usable {
                p.note { a href=(&refresh_path) { "Refresh results" } }
            }
            @match selection {
                None => div.notice role="status" {
                    @if data.refreshing { "Loading eligible stations and forecasts. Results appear here when ready."
                        noscript { p { "Automatic updates need JavaScript. Use Refresh results to check progress." } } }
                    @else { "Weather discovery is unavailable. Retry shortly; the oracle could not supply a complete forecast set." }
                },
                Some(Err(error)) => div.notice role="alert" { (error) },
                Some(Ok(candidates)) => {
                    @if candidates.is_empty() {
                        h2 { "0 matching stations" }
                        p { "No stations match this window and these filters. Try a wider area or a shorter window." }
                    } @else {
                        form id="game-creation" method="post" action="/admin/api/competitions" hx-post="/admin/api/competitions" hx-target="#competition-notification" hx-swap="innerHTML" hx-indicator="#creation-progress" {
                            input type="hidden" name="id" id="competitionIdInput" value=(uuid::Uuid::now_v7());
                            input type="hidden" name="start_observation_date" value=(window.start.format(&Rfc3339).unwrap_or_default());
                            input type="hidden" name="end_observation_date" value=(window.end.format(&Rfc3339).unwrap_or_default());
                            input type="hidden" name="signing_date" value=((window.end + time::Duration::HOUR).format(&Rfc3339).unwrap_or_default());
                            input type="hidden" name="history_days" value=(window.history_days);
                            section.creation-panel {
                                h2 { "Set up this game" }
                                p { "Select up to 50 stations below. The server checks their current eligibility again when you create the competition." }
                                p { "Observation: " (window.start.format(&Rfc3339).unwrap_or_default()) " → " (window.end.format(&Rfc3339).unwrap_or_default()) ". Signing starts one hour later." }
                                div.form-grid {
                                    label { "Stake per entry (sats)" input type="number" name="entry_fee" value="5000" min="1" required; }
                                    // Empty means every category at every selected station; the admin
                                    // script keeps the maximum in step with the stations chosen.
                                    label { "Picks per entry"
                                        input type="number" name="number_of_values_per_entry" min="1" step="1" placeholder="All" data-metrics=(metrics);
                                        span.note data-picks-note { "Leave empty for all: " (metrics) " per selected station." }
                                    }
                                    label { "Entries per player" input type="number" name="max_entries_per_player" value="1" min="1" required; }
                                    label { "Coordinator fee (%)" input type="number" name="coordinator_fee_percentage" value="5" min="0" max="100" step="0.01" required; }
                                    label { "Minimum pool size" input type="number" name="min_players" value="2" min="2" max="13" required; }
                                    // The default game: one pool of 20 seats paying 70% and 30% from ten players.
                                    label { "Maximum pool size" input type="number" name="max_pool_size" value="20" min="3" max="25" required; }
                                    label { "Maximum queued entries" input type="number" name="max_entries" value="20" min="2" max="1500" required; }
                                    label { "Winning places" input type="number" name="number_of_places_win" value="2" min="1" max="2" required; }
                                }
                                p.note { "Two places pay 70% and 30% in pools of 10 or more, and need pools of at most 20; smaller pools pay their winner the pot." }
                                label.check { input type="checkbox" name="queued" value="true" checked; " Form pools when registration closes" }
                                details {
                                    summary { "Advanced terms and fixed-size games" }
                                    p.note { "Fixed-size terms apply when pool formation is unchecked." }
                                    div.form-grid {
                                        label { "Fixed-size seats" input type="number" name="total_allowed_entries" value="3" min="2" required; }
                                        label { "Scoring" select name="scoring_rules" { option value="lines" { "Lines" } option value="fixed" { "Fixed" } } }
                                        label { "Blocks between settlement stages" input type="number" name="relative_locktime_block_delta" value=(block_delta) min="1" required; }
                                    }
                                }
                                button type="submit" disabled[!usable] { "Create competition" }
                                span id="creation-progress" class="htmx-indicator" role="status" { " Checking eligibility and creating…" }
                                div id="competition-notification" role="status" aria-live="polite" {}
                            }
                            @if filters.near.trim().is_empty() {
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
                            (super::weather_map::map_slot(filters, window))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::{admin_weather::Candidate, refresh_cache::Fetched};
    use std::sync::Arc;

    fn window() -> Window {
        Filters::default()
            .window(time::macros::datetime!(2026-10-02 12:00 UTC))
            .unwrap()
    }

    #[test]
    fn cold_loading_polls_only_until_a_result_or_failure_and_keeps_a_native_link() {
        let filters = Filters {
            location: "Portland & coast".into(),
            near: "KPDX".into(),
            weather: "wind".into(),
            radius_km: Some(250),
            ..Default::default()
        };
        let window = window();
        let path = nearby_link(&filters, &window, &filters.near, &filters.weather);
        let uri = path.parse().unwrap();
        let parsed = axum::extract::Query::<Filters>::try_from_uri(&uri)
            .unwrap()
            .0;
        assert_eq!(parsed.location, filters.location);
        assert_eq!(parsed.near, filters.near);
        assert_eq!(parsed.weather, filters.weather);
        assert_eq!(parsed.radius_km, filters.radius_km);
        assert_eq!(parsed.day, "2026-10-03");
        assert_eq!(parsed.history_days, Some(3));
        let mut data = Cached {
            latest: None,
            refreshing: true,
        };
        let loading = discovery(&filters, &window, &data, "signet").into_string();
        assert!(loading.contains("hx-trigger=\"every 2s\""));
        assert!(loading.contains("hx-select=\"#weather-discovery-results\""));
        assert!(loading.contains("hx-sync=\"this:drop\""));
        assert!(loading.contains("<noscript>"));
        assert!(loading.contains("Refresh results</a>"));
        assert!(!loading.contains("id=\"game-creation\""));
        assert!(
            loading.find("</form>").unwrap()
                < loading.find("id=\"weather-discovery-results\"").unwrap()
        );

        data.refreshing = false;
        let failed = discovery(&filters, &window, &data, "signet").into_string();
        assert!(!failed.contains("hx-get="));
        assert!(failed.contains("Weather discovery is unavailable"));
        assert!(failed.contains("Refresh results</a>"));

        data.latest = Some(Arc::new(Fetched::new(Discovery {
            candidates: Vec::new(),
            eligible_count: 0,
            missing_forecasts: 0,
        })));
        let finished = discovery(&Filters::default(), &window, &data, "signet").into_string();
        assert!(!finished.contains("hx-get="));
        assert!(!finished.contains("hx-trigger="));
        assert!(finished.contains("0 matching stations"));
    }

    #[test]
    fn a_background_refresh_does_not_replace_cached_results_or_game_choices() {
        let candidate = Candidate {
            eligible: serde_json::from_value(serde_json::json!({
                "station_id":"KPDX", "station_name":"Portland", "state":"OR", "iata_id":"PDX",
                "latitude":45.58, "longitude":-122.6, "clean_days":3, "days_checked":3,
                "last_report":"2026-10-02T10:00:00Z", "forecast_through":"2026-10-05T00:00:00Z"
            }))
            .unwrap(),
            high: 68,
            low: 50,
            wind_knots: None,
            rain_chance: None,
            forecasts: Vec::new(),
        };
        let data = Cached {
            latest: Some(Arc::new(Fetched::new(Discovery {
                candidates: vec![candidate],
                eligible_count: 1,
                missing_forecasts: 0,
            }))),
            refreshing: true,
        };
        let html = discovery(&Filters::default(), &window(), &data, "signet").into_string();
        assert!(html.contains("id=\"game-creation\""));
        assert!(html.contains("name=\"locations\" value=\"KPDX\""));
        // Picks per entry is optional: empty takes every category at every selected station.
        assert!(html.contains("name=\"number_of_values_per_entry\" min=\"1\""));
        assert!(html.contains("data-metrics=\"3\""));
        assert!(html.contains("Leave empty for all: 3 per selected station."));
        assert!(html.contains("Your current station choices stay in place"));
        // Game rules and the create button come before the stations and map.
        let create = html.find("Create competition</button>").unwrap();
        assert!(html.find("Set up this game").unwrap() < create);
        assert!(create < html.find("1 matching stations").unwrap());
        assert!(create < html.find("/admin/competition/map?").unwrap());
        assert!(create < html.find("name=\"locations\"").unwrap());
        assert!(!html.contains("every 2s"));
        assert!(html.contains("Refresh results</a>"));
        // The map loads once, after the cards, and is not polled.
        assert_eq!(html.matches("hx-get=").count(), 1);
        assert!(html.contains("hx-get=\"/admin/competition/map?day=2026-10-03"));
        assert!(html.contains("hx-trigger=\"load\""));
        assert!(!html.contains("<svg"));
        assert!(html.contains("Open the station map</a>"));
    }
}
