use maud::{html, Markup};

use crate::templates::pages::competitions::CompetitionView;

/// Weather forecast data for a station
#[derive(Debug, Clone)]
pub struct StationForecast {
    pub station_id: String,
    pub station_name: String,
    pub wind_speed: Option<ForecastValue>,
    pub temp_high: Option<ForecastValue>,
    pub temp_low: Option<ForecastValue>,
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
pub fn entry_form(competition: &CompetitionView, forecasts: &[StationForecast]) -> Markup {
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
                        p { strong { "Entry Fee: " } (competition.entry_fee) " sats" }
                        p { strong { "Observation Period: " } (competition.start_time) " - " (competition.end_time) }
                    }

                    // Station forecast picks
                    form id="entryForm" data-competition-id=(competition.id) {
                        @for forecast in forecasts {
                            (station_picks(forecast))
                        }
                    }
                }

                div class="mt-4" {
                    // Submit button - triggers JS handler
                    button type="button" id="submitEntry" class="button is-info"
                           onclick="submitEntry()" {
                        "Submit Entry"
                    }
                    div id="successMessage" class="has-text-success-dark hidden" {
                        "Successfully Submitted Entry!"
                    }
                    div id="errorMessage" class="has-text-danger-dark hidden" {}
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
                       class="button pick-button"
                       data-field=(field_name)
                       data-value="over"
                       onclick="selectPick(this)" {
                    "Over"
                }
                button type="button"
                       class="button pick-button"
                       data-field=(field_name)
                       data-value="par"
                       onclick="selectPick(this)" {
                    "Par"
                }
                button type="button"
                       class="button pick-button"
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
