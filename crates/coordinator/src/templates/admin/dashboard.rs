use maud::{html, Markup};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Station data from the oracle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Station {
    pub station_id: String,
    pub station_name: String,
    pub latitude: f64,
    pub longitude: f64,
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
pub fn admin_dashboard(stations: &[Station], defaults: &CompetitionDefaults) -> Markup {
    html! {
        section class="section" {
            div class="container" {
                h1 class="title" { "Fantasy Weather Admin" }
            }

            div class="container" {
                h6 class="subtitle" { "Create Competition" }

                // Competition form using HTMX
                form hx-post="/admin/api/competitions"
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

                    div class="columns" {
                        div class="column is-half" {
                            div class="field" {
                                label class="label" { "Number of Values Per Entry" }
                                div class="control" {
                                    input class="input" type="number" name="number_of_values_per_entry"
                                          value="1" min="1";
                                }
                            }

                            div class="field" {
                                label class="label" { "Total Allowed Entries" }
                                div class="control" {
                                    input class="input" type="number" name="total_allowed_entries"
                                          value="3" min="1";
                                }
                            }

                            div class="field" {
                                label class="label" { "Entry Fee (sats)" }
                                div class="control" {
                                    input class="input" type="number" name="entry_fee"
                                          value="5000" min="1";
                                }
                            }

                            div class="field" {
                                label class="label" { "Coordinator Fee (%)" }
                                div class="control" {
                                    input class="input" type="number" name="coordinator_fee_percentage"
                                          value="5" min="0" max="100";
                                }
                            }

                            div class="field" {
                                label class="label" { "Number of Places Win" }
                                div class="control" {
                                    input class="input" type="number" name="number_of_places_win"
                                          value="1" min="1";
                                }
                            }
                        }

                        div class="column is-half" {
                            div class="field" {
                                label class="label" { "Locations (select multiple)" }
                                div class="control" {
                                    div class="select is-multiple is-fullwidth" {
                                        select name="locations" multiple size="10" {
                                            @for station in stations {
                                                option value=(station.station_id) {
                                                    (station.station_id) " - " (station.station_name)
                                                }
                                            }
                                        }
                                    }
                                }
                                p class="help" { "Hold Ctrl/Cmd to select multiple stations" }
                            }
                        }
                    }

                    div class="field" {
                        div class="control" {
                            button class="button is-primary" type="submit" {
                                span class="icon is-small" id="submit-indicator" {
                                    // Loading spinner shown during submission
                                }
                                span { "Create Competition" }
                            }
                        }
                    }
                }

                // Notification area for success/error messages
                div id="competition-notification" class="mt-4" {}
            }
        }

        // Stations reference table
        section class="section" {
            div class="container" {
                h6 class="subtitle" { "Available Stations" }
                div class="columns is-centered is-mobile" {
                    div class="column" {
                        div class="table-container" {
                            table id="stations_container" class="table is-striped is-bordered is-narrow is-fullwidth" {
                                thead {
                                    tr {
                                        th { "Station ID" }
                                        th { "Station Name" }
                                        th { "Latitude" }
                                        th { "Longitude" }
                                    }
                                }
                                tbody {
                                    @for station in stations {
                                        tr {
                                            td { (station.station_id) }
                                            td { (station.station_name) }
                                            td { (format!("{:.4}", station.latitude)) }
                                            td { (format!("{:.4}", station.longitude)) }
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
