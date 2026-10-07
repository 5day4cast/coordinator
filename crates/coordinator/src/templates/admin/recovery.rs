//! Recovery records on the Services page: each relay with the live records it has not taken,
//! the records by kind, and the republish that backfills a relay. See docs/RECOVERY.md,
//! "Adding a relay".

use maud::{html, Markup};

use crate::domain::recovery::{RecoveryStatus, RepublishReport};
use crate::templates::format::thousands;

fn count(value: i64) -> String {
    thousands(u64::try_from(value).unwrap_or_default())
}

/// The recovery records' card; `None` when their status could not be read.
pub fn recovery_section(status: Option<&RecoveryStatus>) -> Markup {
    html! {
        section id="recovery-records" {
            h2 { "Recovery records" }
            @match status {
                None => p.notice { "The recovery records' status is unavailable." },
                Some(status) if !status.enabled => p { "Recovery records are off (" code { "[recovery].enabled" } ")." },
                Some(status) => {
                    p { "Players' recovery records on the Nostr relays, signed by " code { (status.coordinator_pubkey) } " on " (status.network) ". A relay added to " code { "[recovery].relays" } " gets the records published before it only once you republish to it. Records whose money is settled are deleted after the grace period and not sent again." }
                    p { "Waiting to publish: " strong { (count(status.outbox_depth)) } }
                    @if status.relays.is_empty() {
                        p.notice { "No relay is configured; the records are kept for the recovery file only." }
                    } @else {
                        div.scroll { table.ops-table {
                            thead { tr { th { "Relay" } th { "Live records it lacks" } th { "Action" } } }
                            tbody {
                                @for (index, relay) in status.relays.iter().enumerate() {
                                    @let result = format!("recovery-relay-{index}-result");
                                    tr {
                                        td { code { (relay.url) } }
                                        td { (count(relay.missing)) }
                                        td {
                                            form hx-post="/admin/api/recovery/republish" hx-target=(format!("#{result}")) hx-swap="innerHTML" hx-confirm=(format!("Offer every live recovery record to {} again? They go out a few every few seconds.", relay.url)) {
                                                input type="hidden" name="relay" value=(relay.url);
                                                button type="submit" { "Republish" }
                                            }
                                            div id=(result) role="status" {}
                                        }
                                    }
                                }
                            }
                        } }
                        form hx-post="/admin/api/recovery/republish" hx-target="#recovery-all-result" hx-swap="innerHTML" hx-confirm="Offer every live recovery record to every relay again?" {
                            button type="submit" { "Republish to every relay" }
                        }
                        div id="recovery-all-result" role="status" {}
                    }
                    div.scroll { table.ops-table {
                        thead { tr { th { "Records" } th { "Live" } th { "Settled, awaiting deletion" } th { "Deleted" } } }
                        tbody {
                            @for kind in &status.records {
                                tr {
                                    td { (kind.kind) }
                                    td { (count(kind.live)) }
                                    td { (count(kind.settled)) }
                                    td { (count(kind.retired)) }
                                }
                            }
                        }
                    } }
                }
            }
        }
    }
}

/// What a republish queued.
pub fn republish_result(report: &RepublishReport) -> Markup {
    html! {
        div class="notification is-success" {
            "Queued " (thousands(report.queued)) " records for " (report.relays.join(", "))
            "; the last goes out in about " (report.seconds.div_euclid(60).max(1)) " min."
        }
    }
}

/// Why a republish was refused.
pub fn republish_error(message: &str) -> Markup {
    html! {
        div class="notification is-danger" { (message) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        recovery::{RecoveryStatus, RelayStatus},
        RecoveryKindCount,
    };

    #[test]
    fn each_relay_offers_a_republish_and_the_records_are_counted() {
        let status = RecoveryStatus {
            enabled: true,
            coordinator_pubkey: "bd".repeat(32),
            network: "signet".into(),
            relays: vec![
                RelayStatus {
                    url: "wss://relay.example.org".into(),
                    missing: 1_250,
                },
                RelayStatus {
                    url: "wss://new.example.org".into(),
                    missing: 0,
                },
            ],
            outbox_depth: 3,
            records: vec![RecoveryKindCount {
                kind: "entry".into(),
                live: 40,
                settled: 12,
                retired: 7,
            }],
        };
        let html = recovery_section(Some(&status)).into_string();
        assert!(html.contains("wss://relay.example.org"));
        assert!(html.contains("1,250"));
        assert_eq!(
            html.matches("hx-post=\"/admin/api/recovery/republish\"")
                .count(),
            3
        );
        assert!(html.contains("name=\"relay\" value=\"wss://new.example.org\""));
        assert!(html.contains("<td>12</td>"));

        let off = recovery_section(Some(&RecoveryStatus::default())).into_string();
        assert!(off.contains("Recovery records are off"));
        assert!(!off.contains("hx-post"));
        assert!(recovery_section(None).into_string().contains("unavailable"));
    }

    #[test]
    fn a_republish_says_how_much_it_queued() {
        let html = republish_result(&RepublishReport {
            relays: vec!["wss://new.example.org".into()],
            queued: 1_200,
            seconds: 300,
        })
        .into_string();
        assert!(html.contains("Queued 1,200 records for wss://new.example.org"));
        assert!(html.contains("about 5 min"));
    }
}
