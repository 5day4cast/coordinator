use maud::{html, Markup};

use crate::templates::pages::competitions::CompetitionView;
use crate::templates::shared_map::{station_map, StationMarker};

/// Weather forecast data for a station
#[derive(Debug, Clone)]
pub struct StationForecast {
    pub station_id: String,
    pub station_name: String,
    pub wind_speed: Option<ForecastValue>,
    pub temp_high: Option<ForecastValue>,
    pub temp_low: Option<ForecastValue>,
    /// Weather context for user reference
    pub weather_context: Option<WeatherContext>,
}

/// Current and forecast weather context for reference
#[derive(Debug, Clone)]
pub struct WeatherContext {
    /// Current actual temperature (if available)
    pub current_temp: Option<f64>,
    /// Today's forecast high
    pub today_forecast_high: Option<f64>,
    /// Today's forecast low
    pub today_forecast_low: Option<f64>,
    /// Today's actual high (if observation available)
    pub today_actual_high: Option<f64>,
    /// Today's actual low (if observation available)
    pub today_actual_low: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ForecastValue {
    pub value: f64,
    pub unit: String,
}

/// Entry form for a competition
///
/// This renders the form shell - the actual submission is handled by JS
/// because it requires WASM for key generation and keymeld registration.
pub fn entry_form(
    competition: &CompetitionView,
    forecasts: &[StationForecast],
    markers: &[StationMarker],
) -> Markup {
    html! {
        div id="entryContainer" class="container" {
            div class="box" {
                div class="level" {
                    div class="level-left" {
                        h2 class="title is-4" { "Submit Entry" }
                    }
                    div class="level-right" {
                        button id="backToCompetitions" class="button is-light"
                               hx-get="/competitions"
                               hx-target="#main-content"
                               hx-push-url="true" {
                            "Back to Competitions"
                        }
                    }
                }

                div id="entryContent" {
                    // Competition info
                    div class="notification is-light mb-4" {
                        p { strong { "Competition: " } (competition.id) }
                        p {
                            strong { "Ticket price: " } (competition.ticket_price) " sats"
                            @if competition.ticket_price > competition.entry_fee {
                                " (" (competition.entry_fee) " sats entry fee + "
                                (competition.ticket_price - competition.entry_fee) " sats coordinator fee)"
                            }
                        }
                        p {
                            strong { "Observation Period: " }
                            span class="utc-time" data-utc=(competition.start_time) { (competition.start_time) }
                            " - "
                            span class="utc-time" data-utc=(competition.end_time) { (competition.end_time) }
                        }
                    }

                    // Station map
                    @if !markers.is_empty() {
                        div class="mb-4" {
                            (station_map(markers))
                        }
                    }

                    // Station forecast picks
                    form id="entryForm" data-competition-id=(competition.id)
                         data-entry-fee=(competition.entry_fee)
                         data-ticket-price=(competition.ticket_price)
                         data-total-pool=(competition.total_pool)
                         data-winner-count=(competition.num_winners)
                         data-max-values=(competition.number_of_values_per_entry) {
                        @for forecast in forecasts {
                            (station_picks(forecast))
                        }
                    }
                }

                div class="box mt-4" id="entryPayoutConsent" {
                    h3 class="title is-5" { "Receive your winnings" }
                    // Filled by entries.js from the profile once the user is logged in.
                    p id="entryPayoutDestination" class="mb-2" { "Log in to see where your winnings are paid." }
                    p class="help mb-3" {
                        "Winnings go to the Lightning Address on your profile, automatically after the result, "
                        "even while you are offline. You can submit an invoice instead if your provider is unavailable. "
                        "Change the address on the "
                        a href="/payouts" hx-get="/payouts" hx-target="#main-content" hx-push-url="true" { "Payouts" }
                        " page; an entry keeps the address it was made with."
                    }
                    p id="entryPayoutTermsText" class="help mb-3" { "Loading payout terms…" }
                    div class="field" {
                        label class="checkbox" {
                            input type="checkbox" id="entryPayoutApproved";
                            span id="entryPayoutConsentText" {
                                " I authorize this payout method and agree to exchange this entry's on-chain claim "
                                "for the Lightning payout only after payment is verified."
                            }
                        }
                    }
                    script { (maud::PreEscaped("window.setupEntryPayoutConsent?.();")) }
                }

                // JavaScript to convert UTC times to local timezone
                script {
                    (maud::PreEscaped(r#"
                    (function() {
                        document.querySelectorAll('.utc-time').forEach(function(el) {
                            const utc = el.dataset.utc;
                            if (utc) {
                                const date = new Date(utc);
                                el.textContent = date.toLocaleString(undefined, {
                                    month: 'short',
                                    day: 'numeric',
                                    hour: 'numeric',
                                    minute: '2-digit'
                                });
                                el.title = date.toLocaleString();
                            }
                        });
                    })();
                    "#))
                }

                div class="mt-4" {
                    p class="help mb-3" {
                        "Submitting an entry authorizes automatic signing for this competition. "
                        "You do not need to stay online or approve each signing request."
                    }
                    // Filled by entries.js from the WASM build's pinned keymeld measurements.
                    p id="keymeldTrust" class="help mb-3" {}
                    script { (maud::PreEscaped("window.showKeymeldTrust?.();")) }
                    // Submit button - triggers JS handler
                    button type="button" id="submitEntry" class="button is-info"
                           onclick="submitEntry()" {
                        "Pay " (competition.ticket_price) " sats and submit entry"
                    }
                    div id="successMessage" class="notification is-success hidden" {
                        "Successfully Submitted Entry!"
                    }
                    div id="errorMessage" class="notification is-danger hidden" {}
                }
            }
        }
    }
}

/// Pick buttons for a single station
fn station_picks(forecast: &StationForecast) -> Markup {
    html! {
        div class="box mb-4" data-station=(forecast.station_id) {
            h5 class="title is-5" {
                (forecast.station_id) " - " (forecast.station_name)
            }

            // Weather context for reference
            @if let Some(ctx) = &forecast.weather_context {
                div class="weather-context" {
                    @if let (Some(actual_high), Some(actual_low)) = (ctx.today_actual_high, ctx.today_actual_low) {
                        div class="weather-row" {
                            span class="weather-label" { "Today's Actual:" }
                            span class="weather-value" {
                                (format!("{:.0}°F", actual_high)) " / " (format!("{:.0}°F", actual_low))
                            }
                        }
                    }
                    @if let (Some(fc_high), Some(fc_low)) = (ctx.today_forecast_high, ctx.today_forecast_low) {
                        div class="weather-row" {
                            span class="weather-label" { "Today's Forecast:" }
                            span class="weather-value forecast-value" {
                                (format!("{:.0}°F", fc_high)) " / " (format!("{:.0}°F", fc_low))
                            }
                        }
                    }
                }
            }

            @if let Some(wind) = &forecast.wind_speed {
                (pick_row(&forecast.station_id, "wind_speed", "Wind Speed", wind))
            }

            @if let Some(high) = &forecast.temp_high {
                (pick_row(&forecast.station_id, "temp_high", "High Temp", high))
            }

            @if let Some(low) = &forecast.temp_low {
                (pick_row(&forecast.station_id, "temp_low", "Low Temp", low))
            }
        }
    }
}

/// Single pick row with Over/Par/Under buttons
fn pick_row(station_id: &str, metric: &str, label: &str, value: &ForecastValue) -> Markup {
    let field_name = format!("{}_{}", station_id, metric);

    html! {
        div class="field" {
            label class="label" {
                (label) ": " (format!("{:.1}", value.value)) " " (value.unit)
            }
            div class="buttons has-addons" {
                button type="button"
                       class="button pick-button is-outlined"
                       data-field=(field_name)
                       data-value="over"
                       onclick="selectPick(this)" {
                    "Over"
                }
                button type="button"
                       class="button pick-button is-outlined"
                       data-field=(field_name)
                       data-value="par"
                       onclick="selectPick(this)" {
                    "Par"
                }
                button type="button"
                       class="button pick-button is-outlined"
                       data-field=(field_name)
                       data-value="under"
                       onclick="selectPick(this)" {
                    "Under"
                }
                input type="hidden" name=(field_name) id=(field_name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn competition(entry_fee: u64, ticket_price: u64) -> CompetitionView {
        CompetitionView {
            id: "01a0c225-f3c4-71f3-9f62-4b74859cfc25".into(),
            start_time: String::new(),
            end_time: String::new(),
            signing_time: String::new(),
            status: "Registration".into(),
            entry_fee,
            ticket_price,
            total_pool: entry_fee * 3,
            total_entries: 0,
            num_winners: 1,
            can_enter: true,
            number_of_values_per_entry: 1,
        }
    }

    #[test]
    fn entry_form_shows_the_ticket_price_the_browser_approves() {
        let html = entry_form(&competition(5000, 5250), &[], &[]).into_string();
        assert!(html.contains(r#"data-ticket-price="5250""#));
        assert!(html.contains("5250 sats (5000 sats entry fee + 250 sats coordinator fee)"));
        assert!(html.contains("Pay 5250 sats and submit entry"));
    }

    #[test]
    fn entry_form_takes_the_payout_address_from_the_profile() {
        let html = entry_form(&competition(1000, 1000), &[], &[]).into_string();
        assert!(html.contains(r#"id="entryPayoutDestination""#));
        assert!(!html.contains("entryLightningAddress"));
        assert!(!html.contains("coordinator fee)"), "no fee breakdown without a fee");
    }
}
