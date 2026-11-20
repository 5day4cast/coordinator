use maud::{html, Markup};

use crate::domain::EligiblePayout;
use crate::templates::format::{copyable_id, sats};

/// The Wallet page: where winnings go, the name the player shows on leaderboards, their
/// recovery file, then payout status and invoice fallback. Each entry retains its authorized
/// address; the profile address is a default for future entries only. `recovery_file` offers
/// the player's recovery file; `satchel`, the Satchel wallet's origin when configured, a
/// Lightning Address from it.
pub fn payouts_page(
    payouts: &[EligiblePayout],
    lightning_address: Option<&str>,
    display_name: Option<&str>,
    recovery_file: bool,
    satchel: Option<&str>,
) -> Markup {
    html! {
        div id="payouts" class="account-page wallet-page" {
            div {
                h1 class="title page-title" { "Wallet" }

                div class="wallet-cards" {
                    section class="wallet-card" {
                        h2 class="wallet-card-title" { "Winnings go to" }
                        (lightning_address_panel(lightning_address, satchel))
                    }
                    section class="wallet-card" {
                        h2 class="wallet-card-title" { "Leaderboard name" }
                        (display_name_panel(display_name))
                    }
                    @if recovery_file {
                        section class="wallet-card" {
                            h2 class="wallet-card-title" { "Account safety" }
                            (recovery_file_link())
                        }
                    }
                }

                h2 class="title wallet-section-title" { "Payouts" }
                @if payouts.is_empty() {
                    (no_payouts())
                } @else {
                    div class="table-container" {
                        table class="table is-fullwidth is-striped is-hoverable is-card-mobile" {
                            thead {
                                tr {
                                    th { "Contest" }
                                    th { "Entry" }
                                    th { "Amount" }
                                    th { "Status" }
                                    th { "Action" }
                                }
                            }
                            tbody {
                                @for payout in payouts {
                                    tr {
                                        td data-label="Contest" {
                                            a href=(format!("/competitions/{}/leaderboard", payout.competition_id))
                                              hx-get=(format!("/competitions/{}/leaderboard", payout.competition_id))
                                              hx-target="#main-content" hx-push-url="true" { "Leaderboard" }
                                        }
                                        td data-label="Entry" { (copyable_id(&payout.entry_id.to_string())) }
                                        td data-label="Amount" { (sats(payout.amount_sats)) }
                                        td data-label="Status" { (payout.status) }
                                        td data-label="Action" {
                                            div class="payout-actions" {
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
                                                } @else if payout.allow_invoice_fallback {
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
                }

                div id="payoutsError" class="notification is-danger hidden" {}
            }
        }
    }
}

/// The account's payout address with an inline form to change it.
fn lightning_address_panel(lightning_address: Option<&str>, satchel: Option<&str>) -> Markup {
    html! {
        // Telemetry ignores the address and its form (shared/telemetry.js).
        div class="content" id="payoutAddressPanel" data-telemetry="off" {
            @match lightning_address {
                Some(address) => {
                    p id="payoutAddress" {
                        "Default address for new entries: " strong { (address) } " "
                        button type="button" class="button is-text is-small" data-payout-action="edit-address" aria-controls="lightningAddressForm" aria-expanded="false" { "Change" }
                    }
                }
                None => {
                    p id="payoutAddress" class="has-text-danger" {
                        "Add a Lightning Address to receive automatic payouts on new entries. "
                        button type="button" class="button is-text is-small" data-payout-action="edit-address" aria-controls="lightningAddressForm" aria-expanded="false" { "Add" }
                    }
                }
            }
            p class="help" {
                "Automatic payouts run after finalization; you do not need to click Claim or keep this page open. "
                "An address change applies to future entries. Existing entries keep the address you authorized."
            }
            div id="lightningAddressForm" class="is-hidden" {
                label class="label" for="payoutLightningAddress" { "Lightning Address" }
                div class="field has-addons" {
                    div class="control is-expanded" {
                        input class="input" type="text" id="payoutLightningAddress"
                              placeholder="you@cash.app" value=[lightning_address]
                              autocomplete="off" spellcheck="false" aria-describedby="lightningAddressError";
                    }
                    div class="control" {
                        button type="button" class="button is-primary" id="saveLightningAddress"
                               data-payout-action="save-address" { "Save" }
                    }
                }
                @if let Some(satchel) = satchel {
                    (satchel_address(satchel))
                }
            }
            p class="help is-danger" id="lightningAddressError" role="alert" {}
        }
    }
}

/// The name the player shows on leaderboards, with a field to set or clear it (`payouts.js`).
/// Without one they show as their short npub.
fn display_name_panel(display_name: Option<&str>) -> Markup {
    html! {
        div id="displayNamePanel" data-telemetry="off" {
            p id="displayName" {
                @match display_name {
                    Some(name) => { "You show as " strong { (name) } " on leaderboards." }
                    None => { "You show as your short npub on leaderboards." }
                }
            }
            div class="field has-addons" {
                div class="control is-expanded" {
                    label class="is-sr-only" for="displayNameInput" { "Leaderboard name" }
                    input class="input" type="text" id="displayNameInput" maxlength="24"
                          placeholder="Storm Chaser" value=[display_name] autocomplete="off"
                          spellcheck="false" aria-describedby="displayNameError";
                }
                div class="control" {
                    button type="button" class="button is-primary" id="saveDisplayName"
                           data-payout-action="save-display-name" { "Save" }
                }
            }
            p class="help" {
                "3 to 24 letters, numbers or spaces. Leave it empty to stay anonymous. "
                "Your login username stays private, so pick something else."
            }
            p class="help is-danger" id="displayNameError" role="alert" {}
        }
    }
}

/// One line under the address field: a link that signs the player in to Satchel for a
/// Lightning Address there. `shared/satchel.js` replaces it with a button that fills in the
/// address once Satchel says the player has one.
fn satchel_address(satchel: &str) -> Markup {
    html! {
        p class="help" id="satchelAddress" {
            a href=(format!("{satchel}/wallet")) target="_blank" rel="noopener" data-satchel-next="/wallet" {
                "Get a Lightning Address with Satchel"
            }
        }
    }
}

/// The recovery file, downloaded with the player's signature (`payouts.js`).
fn recovery_file_link() -> Markup {
    html! {
        p class="help" id="recoveryFile" {
            a href="/api/v1/recovery/kit" data-payout-action="recovery-file" { "Download recovery file" }
            ". With your recovery key (nsec), it is what you need to recover your funds with our recovery tool if this site is ever gone."
        }
    }
}

/// No payouts available message
pub fn no_payouts() -> Markup {
    html! {
        div id="noPayoutsMessage" class="empty-state-box" {
            "Nothing to collect right now. Winnings and pot returns appear here after the competition settles."
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
        let html =
            payouts_page(&[payout], Some("freya@lnurl.example"), None, false, None).into_string();
        // The page's CSP allows no inline script, handlers included.
        assert!(!html.contains("onclick"));
        assert!(html.contains(r#"data-payout-action="invoice""#));
        assert!(html.contains(r#"data-payout-action="edit-address""#));
        assert!(html.contains(r#"data-payout-action="save-address""#));
        assert!(html.contains("10,500 sats"));
    }

    #[test]
    fn the_recovery_file_link_says_what_it_and_the_key_are_for() {
        let html = recovery_file_link().into_string();
        assert!(html.contains("recovery key (nsec)"));
        assert!(html.contains("recovery tool"));
        assert!(!html.contains("all you need"));
    }

    #[test]
    fn closed_legacy_payouts_do_not_offer_recovery() {
        let mut payout = EligiblePayout {
            competition_id: uuid::Uuid::now_v7(),
            entry_id: uuid::Uuid::now_v7(),
            status: "On-chain settlement".into(),
            amount_sats: 1_000,
            automatic_lightning_address: None,
            allow_invoice_fallback: false,
            escrow_enabled: false,
        };
        let closed = payouts_page(&[payout.clone()], None, None, false, None).into_string();
        assert!(!closed.contains("Legacy recovery"));
        payout.status = "Awaiting invoice".into();
        payout.allow_invoice_fallback = true;
        assert!(payouts_page(&[payout], None, None, false, None)
            .into_string()
            .contains("Legacy recovery"));
    }

    /// With Satchel configured, one line under the address field offers it; without, nothing.
    #[test]
    fn satchel_is_offered_under_the_address_field_only_when_configured() {
        for address in [None, Some("freya@lnurl.example")] {
            let html = payouts_page(&[], address, None, false, None).into_string();
            assert!(!html.contains("Satchel"), "{address:?}");

            let html = payouts_page(
                &[],
                address,
                None,
                false,
                Some("https://wallet.5day4cast.com"),
            )
            .into_string();
            let field = html.find(r#"id="payoutLightningAddress""#).unwrap();
            let line = html.find(r#"id="satchelAddress""#).unwrap();
            assert!(line > field, "{address:?}");
            assert!(html[line..].contains(
                r#"href="https://wallet.5day4cast.com/wallet" target="_blank" rel="noopener" data-satchel-next="/wallet""#
            ));
            assert_eq!(
                html.matches("Get a Lightning Address with Satchel").count(),
                1
            );
            assert!(!html.contains("network fee"));
        }
    }
}
