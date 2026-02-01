use maud::{html, Markup};

use crate::domain::UserEntryView;

/// Entries page content (requires auth)
pub fn entries_page(entries: &[UserEntryView]) -> Markup {
    html! {
        div id="allEntries" class="container" {
            div class="box" {
                div class="table-container" {
                    table id="entriesDataTable"
                          class="table is-fullwidth is-striped is-hoverable is-card-mobile" {
                        thead {
                            tr {
                                th { "Entry ID" }
                                th { "Competition" }
                                th { "Start Time" }
                                th { "End Time" }
                                th { "Status" }
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
                                    td data-label="Entry ID" title=(entry.entry_id) { (&entry.entry_id[..8]) }
                                    td data-label="Competition" title=(entry.competition_id) { (&entry.competition_id[..8]) }
                                    td data-label="Start" {
                                        span class="utc-time" data-utc=(entry.start_time) { (entry.start_time) }
                                    }
                                    td data-label="End" {
                                        span class="utc-time" data-utc=(entry.end_time) { (entry.end_time) }
                                    }
                                    td data-label="Status" { (entry.status) }
                                }
                            }
                        }
                    }
                }
            }
            script {
                (maud::PreEscaped(r#"
                (function() {
                    document.querySelectorAll('#allEntries .utc-time').forEach(function(el) {
                        var utc = el.dataset.utc;
                        if (utc) {
                            var date = new Date(utc);
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
