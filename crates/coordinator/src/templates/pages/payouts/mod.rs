use maud::{html, Markup};

/// View data for an eligible payout
#[derive(Debug, Clone)]
pub struct PayoutView {
    pub competition_id: String,
    pub entry_id: String,
    pub status: String,
    pub payout_amount: u64,
}

/// Payouts page content (requires auth). `lightning_address` is where the
/// account's winnings are paid; without one only the pasted-invoice path is
/// offered.
pub fn payouts_page(payouts: &[PayoutView], lightning_address: Option<&str>) -> Markup {
    html! {
        div id="payouts" class="container" {
            div class="box" {
                h3 class="title is-4 mb-4" { "Available Payouts" }

                (lightning_address_panel(lightning_address))

                @if payouts.is_empty() {
                    (no_payouts())
                } @else {
                    div class="table-container" {
                        table class="table is-fullwidth is-striped is-hoverable is-card-mobile" {
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
                                        td data-label="Competition" title=(payout.competition_id) { (&payout.competition_id[..8]) }
                                        td data-label="Entry ID" title=(payout.entry_id) { (&payout.entry_id[..8]) }
                                        td data-label="Amount" { (payout.payout_amount) " sats" }
                                        td data-label="Status" { (payout.status) }
                                        td data-label="Action" {
                                            @if lightning_address.is_some() {
                                                button class="button is-primary is-small mr-2 claim-payout"
                                                       data-entry-id=(payout.entry_id)
                                                       data-competition-id=(payout.competition_id)
                                                       data-payout-amount=(payout.payout_amount)
                                                       onclick="claimPayout(this)" {
                                                    "Claim"
                                                }
                                                button class="button is-light is-small"
                                                       data-entry-id=(payout.entry_id)
                                                       data-competition-id=(payout.competition_id)
                                                       data-payout-amount=(payout.payout_amount)
                                                       onclick="openPayoutModal(this)" {
                                                    "Use invoice"
                                                }
                                            } @else {
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
                }

                div id="payoutsError" class="notification is-danger hidden" {}
            }
        }
    }
}

/// The account's payout address with an inline form to change it.
fn lightning_address_panel(lightning_address: Option<&str>) -> Markup {
    html! {
        div class="content mb-4" id="payoutAddressPanel" {
            @match lightning_address {
                Some(address) => {
                    p id="payoutAddress" {
                        "Winnings are paid to " strong { (address) } ". "
                        a href="#" onclick="toggleLightningAddressForm(event)" { "Change" }
                    }
                }
                None => {
                    p id="payoutAddress" class="has-text-danger" {
                        "No Lightning Address on file. Add one to claim winnings with one click. "
                        a href="#" onclick="toggleLightningAddressForm(event)" { "Add" }
                    }
                }
            }
            div id="lightningAddressForm" class="field has-addons is-hidden" {
                div class="control is-expanded" {
                    input class="input" type="text" id="payoutLightningAddress"
                          placeholder="you@cash.app" value=[lightning_address]
                          autocomplete="off" spellcheck="false";
                }
                div class="control" {
                    button class="button is-primary" id="saveLightningAddress"
                           onclick="saveLightningAddress()" { "Save" }
                }
            }
            p class="help is-danger" id="lightningAddressError" {}
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
