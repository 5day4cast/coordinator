use maud::{html, Markup};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::location_selector::location_selector;

/// Station data from the oracle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Station {
    pub station_id: String,
    pub station_name: String,
    pub latitude: f64,
    pub longitude: f64,
}

/// Station with weather data for display
#[derive(Debug, Clone)]
pub struct StationWithWeather {
    pub station: Station,
    /// Today's actual observed high temperature
    pub today_actual_high: Option<f64>,
    /// Today's actual observed low temperature
    pub today_actual_low: Option<f64>,
    /// Today's forecasted high temperature
    pub today_forecast_high: Option<i64>,
    /// Today's forecasted low temperature
    pub today_forecast_low: Option<i64>,
    /// Tomorrow's forecasted high temperature
    pub tomorrow_forecast_high: Option<i64>,
    /// Tomorrow's forecasted low temperature
    pub tomorrow_forecast_low: Option<i64>,
}

impl From<Station> for StationWithWeather {
    fn from(station: Station) -> Self {
        Self {
            station,
            today_actual_high: None,
            today_actual_low: None,
            today_forecast_high: None,
            today_forecast_low: None,
            tomorrow_forecast_high: None,
            tomorrow_forecast_low: None,
        }
    }
}

/// Forecast data from the oracle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forecast {
    pub station_id: String,
    pub date: String,
    pub temp_high: i64,
    pub temp_low: i64,
}

/// Observation data from the oracle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub station_id: String,
    pub temp_high: f64,
    pub temp_low: f64,
}

/// Default values for competition creation form
#[derive(Debug, Clone)]
pub struct CompetitionDefaults {
    pub id: Uuid,
    pub signing_date: OffsetDateTime,
    pub start_observation_date: OffsetDateTime,
    pub end_observation_date: OffsetDateTime,
}

impl Default for CompetitionDefaults {
    fn default() -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id: Uuid::now_v7(),
            // Start observation: 6 hours from now
            start_observation_date: now + time::Duration::hours(6),
            // End observation: 24 hours from now
            end_observation_date: now + time::Duration::hours(24),
            // Signing: 33 hours from now
            signing_date: now + time::Duration::hours(33),
        }
    }
}

/// Admin dashboard page - competition creation
pub fn admin_dashboard(stations: &[StationWithWeather], defaults: &CompetitionDefaults) -> Markup {
    html! {
        section class="section" {
            div class="container" {
                h1 class="title" { "Fantasy Weather Admin" }
            }

            div class="container" {
                h6 class="subtitle" { "Create Competition" }

                // Competition form using HTMX
                form id="competition-form"
                     hx-post="/admin/api/competitions"
                     hx-target="#competition-notification"
                     hx-swap="innerHTML"
                     hx-indicator="#submit-indicator" {

                    // Hidden fields with auto-generated defaults
                    input type="hidden" name="id" value=(defaults.id);
                    input type="hidden" name="signing_date"
                          value=(defaults.signing_date.format(&time::format_description::well_known::Rfc3339).unwrap_or_default());
                    input type="hidden" name="start_observation_date"
                          value=(defaults.start_observation_date.format(&time::format_description::well_known::Rfc3339).unwrap_or_default());
                    input type="hidden" name="end_observation_date"
                          value=(defaults.end_observation_date.format(&time::format_description::well_known::Rfc3339).unwrap_or_default());

                    // Competition parameters
                    div class="box" {
                        h3 class="subtitle is-5" { "Competition Parameters" }
                        div class="columns" {
                            div class="column" {
                                div class="field" {
                                    label class="label" { "Number of Values Per Entry" }
                                    div class="control" {
                                        input class="input" type="number" name="number_of_values_per_entry"
                                              value="1" min="1";
                                    }
                                }
                            }

                            div class="column" {
                                div class="field" {
                                    label class="label" { "Total Allowed Entries" }
                                    div class="control" {
                                        input class="input" type="number" name="total_allowed_entries"
                                              value="3" min="1";
                                    }
                                }
                            }

                            div class="column" {
                                div class="field" {
                                    label class="label" { "Entry Fee (sats)" }
                                    div class="control" {
                                        input class="input" type="number" name="entry_fee"
                                              value="5000" min="1";
                                    }
                                }
                            }
                        }

                        div class="columns" {
                            div class="column" {
                                div class="field" {
                                    label class="label" { "Coordinator Fee (%)" }
                                    div class="control" {
                                        input class="input" type="number" name="coordinator_fee_percentage"
                                              value="5" min="0" max="100";
                                    }
                                }
                            }

                            div class="column" {
                                div class="field" {
                                    label class="label" { "Number of Places Win" }
                                    div class="control" {
                                        input class="input" type="number" name="number_of_places_win"
                                              value="1" min="1";
                                    }
                                }
                            }

                            div class="column" {
                                // Empty column for layout balance
                            }
                        }
                    }

                    // Location selector with map, table, and Create Competition button
                    (location_selector(stations))
                }
            }
        }

        // Include location selector JavaScript
        script src="/ui/location_selector.js" {}
    }
}

/// Success notification fragment returned after competition creation
pub fn competition_success(competition_id: &Uuid) -> Markup {
    html! {
        div class="notification is-success" {
            button class="delete"
                   onclick="this.parentElement.remove()" {}
            "Competition created successfully! ID: " (competition_id)
        }
    }
}

/// Error notification fragment
pub fn competition_error(message: &str) -> Markup {
    html! {
        div class="notification is-danger" {
            button class="delete"
                   onclick="this.parentElement.remove()" {}
            "Failed to create competition: " (message)
        }
    }
}
