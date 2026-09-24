use maud::{html, Markup};

use crate::domain::EligiblePayout;
use crate::templates::format::{copyable_id, sats};

/// Payout status and invoice fallback. Each entry retains its authorized address;
/// the profile address is a default for future entries only.
pub fn payouts_page(payouts: &[EligiblePayout], lightning_address: Option<&str>) -> Markup {
    html! {
        div id="payouts" class="account-page" {
            div {
                h1 class="title is-4 mb-4" { "Payouts" }

                (lightning_address_panel(lightning_address))

                @if payouts.is_empty() {
                    (no_payouts())
                } @else {
                    div class="table-container" {
                        table class="table is-fullwidth is-striped is-hoverable is-card-mobile" {
                            thead {
                                tr {
                                    th { "Competition" }
                                    th { "Entry" }
                                    th { "Amount" }
                                    th { "Status" }
                                    th { "Action" }
                                }
                            }
                            tbody {
                                @for payout in payouts {
                                    tr {
                                        td data-label="Competition" {
                                            a href=(format!("/competitions/{}/leaderboard", payout.competition_id))
                                              hx-get=(format!("/competitions/{}/leaderboard", payout.competition_id))
                                              hx-target="#main-content" hx-push-url="true" { "Leaderboard" }
                                        }
                                        td data-label="Entry" { (copyable_id(&payout.entry_id.to_string())) }
                                        td data-label="Amount" { (sats(payout.amount_sats)) }
                                        td data-label="Status" { (payout.status) }
                                        td data-label="Action" {
                                            @if let Some(address) = &payout.automatic_lightning_address {
                                                p class="is-size-7 mb-2" { "Automatic payout to " (address) }
                                            }
                                            @if payout.escrow_enabled {
                                                @if payout.allow_invoice_fallback && matches!(payout.status.as_str(), "Awaiting invoice" | "Queued automatically" | "Retrying automatically") {
                                                    button class="button is-light is-small"
                                                        data-entry-id=(payout.entry_id)
                                                        data-competition-id=(payout.competition_id)
                                                        data-payout-amount=(payout.amount_sats)
                                                        data-payout-action="invoice" {
                                                        "Use invoice"
                                                    }
                                                }
                                            } @else {
                                                button class="button is-warning is-light is-small"
                                                    data-entry-id=(payout.entry_id)
                                                    data-competition-id=(payout.competition_id)
                                                    data-payout-amount=(payout.amount_sats)
                                                    data-legacy="true"
                                                    data-payout-action="invoice" {
                                                    "Legacy recovery"
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
                        "Default address for new entries: " strong { (address) } ". "
                        button type="button" class="button is-text is-small" data-payout-action="edit-address" { "Change" }
                    }
                }
                None => {
                    p id="payoutAddress" class="has-text-danger" {
                        "Add a Lightning Address to receive automatic payouts on new entries. "
                        button type="button" class="button is-text is-small" data-payout-action="edit-address" { "Add" }
                    }
                }
            }
            p class="help" {
                "Automatic payouts run after finalization; you do not need to click Claim or keep this page open. "
                "An address change applies to future entries. Existing entries keep the address you authorized."
            }
            div id="lightningAddressForm" class="field has-addons is-hidden" {
                div class="control is-expanded" {
                    input class="input" type="text" id="payoutLightningAddress"
                          placeholder="you@cash.app" value=[lightning_address]
                          autocomplete="off" spellcheck="false";
                }
                div class="control" {
                    button type="button" class="button is-primary" id="saveLightningAddress"
                           data-payout-action="save-address" { "Save" }
                }
            }
            p class="help is-danger" id="lightningAddressError" {}
        }
    }
}

/// No payouts available message
pub fn no_payouts() -> Markup {
    html! {
        div id="noPayoutsMessage" class="empty-state-box" {
            "Nothing to collect right now. Winnings appear here after a competition you placed in finishes."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_carry_actions_instead_of_inline_handlers() {
        let payout = EligiblePayout {
            competition_id: uuid::Uuid::now_v7(),
            entry_id: "01a0d0f5-52e1-7141-b4e1-8dbd7169fc2e".parse().unwrap(),
            status: "Awaiting invoice".into(),
            amount_sats: 10_500,
            automatic_lightning_address: None,
            allow_invoice_fallback: true,
            escrow_enabled: true,
        };
        let html = payouts_page(&[payout], Some("freya@lnurl.example")).into_string();
        // The page's CSP allows no inline script, handlers included.
        assert!(!html.contains("onclick"));
        assert!(html.contains(r#"data-payout-action="invoice""#));
        assert!(html.contains(r#"data-payout-action="edit-address""#));
        assert!(html.contains(r#"data-payout-action="save-address""#));
        assert!(html.contains("10,500 sats"));
    }
}
