use maud::{html, Markup, PreEscaped};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::location_selector::location_selector;

/// Station data from the oracle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Station {
    pub station_id: String,
    pub station_name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub iata_id: String,
    #[serde(default)]
    pub elevation_m: Option<f64>,
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

                    // Hidden field for auto-generated competition ID
                    input type="hidden" name="id" value=(defaults.id);

                    // Hidden fields synced from datetime-local inputs
                    input type="hidden" name="start_observation_date" id="start_observation_date_hidden"
                          value=(defaults.start_observation_date.format(&time::format_description::well_known::Rfc3339).unwrap_or_default());
                    input type="hidden" name="end_observation_date" id="end_observation_date_hidden"
                          value=(defaults.end_observation_date.format(&time::format_description::well_known::Rfc3339).unwrap_or_default());
                    input type="hidden" name="signing_date" id="signing_date_hidden"
                          value=(defaults.signing_date.format(&time::format_description::well_known::Rfc3339).unwrap_or_default());

                    // Timing parameters
                    div class="box" {
                        h3 class="subtitle is-5" { "Timing (local)" }
                        div class="columns" {
                            div class="column" {
                                div class="field" {
                                    label class="label" { "Observation Start" }
                                    div class="control" {
                                        input class="input" type="datetime-local"
                                              id="start_observation_date_input"

                                              onchange="syncDateField('start_observation_date')";
                                    }
                                }
                            }
                            div class="column" {
                                div class="field" {
                                    label class="label" { "Observation End" }
                                    div class="control" {
                                        input class="input" type="datetime-local"
                                              id="end_observation_date_input"

                                              onchange="syncDateField('end_observation_date')";
                                    }
                                }
                            }
                            div class="column" {
                                div class="field" {
                                    label class="label" { "Signing Date" }
                                    div class="control" {
                                        input class="input" type="datetime-local"
                                              id="signing_date_input"

                                              onchange="syncDateField('signing_date')";
                                    }
                                }
                            }
                        }
                    }

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
                                div class="field" {
                                    label class="label" { "Block Delta" }
                                    div class="control" {
                                        input class="input" type="number"
                                              name="relative_locktime_block_delta"
                                              id="block_delta_input"
                                              value="" min="1";
                                    }
                                    p class="help" id="block_delta_help" {
                                        "Blocks between tx stages"
                                    }
                                }
                            }
                        }
                    }

                    // Location selector with map, table, and Create Competition button
                    (location_selector(stations))
                }
            }
        }

            // Delete Competition section
            div class="container mt-5" {
                h6 class="subtitle" { "Delete Competition" }

                div class="box" {
                    form id="delete-competition-form"
                         hx-post="/admin/api/competitions/delete"
                         hx-target="#delete-notification"
                         hx-swap="innerHTML"
                         hx-confirm="Are you sure you want to delete this competition? This action cannot be undone." {

                        div class="field has-addons" {
                            div class="control is-expanded" {
                                input class="input" type="text" name="competition_id"
                                      placeholder="Competition ID (e.g., 019c21de-e786-7992-853e-c27238a61925)";
                            }
                            div class="control" {
                                button class="button is-danger" type="submit" {
                                    "Delete"
                                }
                            }
                        }
                        p class="help" { "Only competitions with no paid entries can be deleted." }
                    }
                    div id="delete-notification" class="mt-3" {}
                }
            }

        // Include location selector JavaScript
        script src="/ui/location_selector.js" {}

        // Sync datetime-local inputs to hidden RFC3339 fields
        script {
            (PreEscaped(r#"
            function syncDateField(name) {
                var input = document.getElementById(name + '_input');
                var hidden = document.getElementById(name + '_hidden');
                if (input && hidden && input.value) {
                    // datetime-local gives local time; convert to UTC via Date object
                    var date = new Date(input.value);
                    hidden.value = date.toISOString();
                }
            }
            "#))
        }

        // Set network-aware defaults for block delta
        script {
            (PreEscaped(r#"
            (function() {
                var network = (document.body.dataset.network || '').toLowerCase();
                var input = document.getElementById('block_delta_input');
                var help = document.getElementById('block_delta_help');
                if (!input) return;

                var config;
                if (network === 'signet') {
                    // Mutinynet/Signet: ~30s blocks
                    config = { min: 2, max: 40320, def: 2880, blockSecs: 30, label: 'signet ~30s blocks' };
                } else if (network === 'bitcoin') {
                    // Mainnet: ~10min blocks
                    config = { min: 6, max: 2016, def: 144, blockSecs: 600, label: 'mainnet ~10min blocks' };
                } else {
                    // Regtest/Testnet: ~10min blocks (but often instant in regtest)
                    config = { min: 1, max: 2016, def: 6, blockSecs: 600, label: network + ' ~10min blocks' };
                }

                input.min = config.min;
                input.max = config.max;
                input.value = config.def;
                input.placeholder = config.def;

                function updateHelp() {
                    var val = parseInt(input.value) || config.def;
                    var totalSecs = val * config.blockSecs;
                    var hours = Math.floor(totalSecs / 3600);
                    var mins = Math.floor((totalSecs % 3600) / 60);
                    var timeStr = hours > 0 ? hours + 'h ' + mins + 'm' : mins + 'm';
                    help.textContent = '~' + timeStr + ' (' + config.label + ', range: ' + config.min + '-' + config.max + ')';
                }

                input.addEventListener('input', updateHelp);
                updateHelp();
            })();
            "#))
        }

        // Convert UTC default values in datetime-local inputs to browser local time
        script {
            (PreEscaped(r#"
            (function() {
                ['start_observation_date', 'end_observation_date', 'signing_date'].forEach(function(name) {
                    var hidden = document.getElementById(name + '_hidden');
                    var input = document.getElementById(name + '_input');
                    if (hidden && input && hidden.value) {
                        var date = new Date(hidden.value);
                        var year = date.getFullYear();
                        var month = String(date.getMonth() + 1).padStart(2, '0');
                        var day = String(date.getDate()).padStart(2, '0');
                        var hours = String(date.getHours()).padStart(2, '0');
                        var minutes = String(date.getMinutes()).padStart(2, '0');
                        input.value = year + '-' + month + '-' + day + 'T' + hours + ':' + minutes;
                    }
                });
            })();
            "#))
        }
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

/// Success message notification fragment (generic)
pub fn competition_success_message(message: &str) -> Markup {
    html! {
        div class="notification is-success" {
            button class="delete"
                   onclick="this.parentElement.remove()" {}
            (message)
        }
    }
}
