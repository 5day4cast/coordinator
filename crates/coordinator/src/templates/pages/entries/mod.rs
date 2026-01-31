use maud::{html, Markup};

/// View data for a user entry
#[derive(Debug, Clone)]
pub struct EntryView {
    pub competition_id: String,
    pub start_time: String,
    pub end_time: String,
    pub status: String,
    pub entry_id: String,
    pub username: String,
}

/// Entries page content (requires auth)
pub fn entries_page(entries: &[EntryView]) -> Markup {
    html! {
        div id="allEntries" class="container" {
            div class="box" {
                div class="table-container" {
                    table id="entriesDataTable"
                          class="table is-fullwidth is-striped is-hoverable" {
                        thead {
                            tr {
                                th { "Username" }
                                th { "Competition ID" }
                                th { "Start Time" }
                                th { "End Time" }
                                th { "Status" }
                                th { "Entry ID" }
                            }
                        }
                        tbody {
                            @for entry in entries {
                                tr hx-get=(format!("/entries/{}/detail", entry.entry_id))
                                   hx-target="#entryScore .modal-content .box"
                                   hx-swap="innerHTML"
                                   onclick="document.getElementById('entryScore').classList.add('is-active')"
                                   style="cursor: pointer;"
                                   title="Click to view entry details" {
                                    td { (entry.username) }
                                    td title=(entry.competition_id) { (&entry.competition_id[..8]) }
                                    td { (entry.start_time) }
                                    td { (entry.end_time) }
                                    td { (entry.status) }
                                    td title=(entry.entry_id) { (&entry.entry_id[..8]) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Empty entries message
pub fn no_entries() -> Markup {
    html! {
        div id="allEntries" class="container" {
            div class="box" {
                div class="notification is-info" {
                    "You haven't submitted any entries yet. Browse competitions to get started!"
                }
            }
        }
    }
}
