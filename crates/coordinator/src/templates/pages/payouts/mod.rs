use maud::{html, Markup};

/// View data for an eligible payout
#[derive(Debug, Clone)]
pub struct PayoutView {
    pub competition_id: String,
    pub entry_id: String,
    pub status: String,
    pub payout_amount: u64,
}

/// Payouts page content (requires auth)
pub fn payouts_page(payouts: &[PayoutView]) -> Markup {
    html! {
        div id="payouts" class="container" {
            div class="box" {
                h3 class="title is-4 mb-4" { "Available Payouts" }

                @if payouts.is_empty() {
                    (no_payouts())
                } @else {
                    div class="table-container" {
                        table class="table is-fullwidth is-striped is-hoverable" {
                            thead {
                                tr {
                                    th { "Competition ID" }
                                    th { "Entry ID" }
                                    th { "Amount (sats)" }
                                    th { "Status" }
                                    th { "Action" }
                                }
                            }
                            tbody {
                                @for payout in payouts {
                                    tr {
                                        td title=(payout.competition_id) { (&payout.competition_id[..8]) }
                                        td title=(payout.entry_id) { (&payout.entry_id[..8]) }
                                        td { (payout.payout_amount) }
                                        td { (payout.status) }
                                        td {
                                            button class="button is-primary is-small"
                                                   data-entry-id=(payout.entry_id)
                                                   data-competition-id=(payout.competition_id)
                                                   data-payout-amount=(payout.payout_amount)
                                                   onclick="openPayoutModal(this)" {
                                                "Submit Invoice"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div id="payoutsError" class="notification is-danger hidden" {}
            }
        }
    }
}

/// No payouts available message
pub fn no_payouts() -> Markup {
    html! {
        div id="noPayoutsMessage" class="notification is-info" {
            "No entries eligible for payout at this time."
        }
    }
}
