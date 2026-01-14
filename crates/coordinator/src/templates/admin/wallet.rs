use maud::{html, Markup};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Wallet balance information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletBalance {
    pub confirmed: u64,
    pub unconfirmed: u64,
}

/// Wallet output (UTXO)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletOutput {
    pub outpoint: String,
    pub txout: TxOut,
    pub is_spent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxOut {
    pub value: u64,
    pub script_pubkey: Option<String>,
}

/// Admin wallet management page
pub fn wallet_page(esplora_url: &str, balance: &WalletBalance, address: &str) -> Markup {
    html! {
        section class="section" {
            div class="container" {
                // Header with explorer link
                div class="level" {
                    div class="level-left" {
                        div class="level-item" {
                            h1 class="title" { "Bitcoin Wallet" }
                        }
                    }
                    div class="level-right" {
                        div class="level-item" {
                            a href=(esplora_url) target="_blank"
                              class="button is-small is-info is-light" {
                                span { "Open Explorer" }
                            }
                        }
                    }
                }

                // Balance section with auto-refresh
                div class="box has-background-light"
                     hx-get="/admin/wallet/balance"
                     hx-trigger="every 30s"
                     hx-swap="innerHTML" {
                    (wallet_balance_section(balance))
                }

                // Address section
                div class="box has-background-light" {
                    h2 class="subtitle has-text-weight-bold" {
                        span class="icon-text" {
                            span { "Bitcoin Address" }
                        }
                    }
                    div id="address-display" class="notification is-light" {
                        p class="heading" { "Current Address" }
                        p class="is-family-monospace has-text-weight-bold" id="current-address" {
                            (address)
                        }
                    }
                    button class="button is-info is-outlined is-fullwidth"
                           hx-get="/admin/wallet/address"
                           hx-target="#current-address"
                           hx-swap="innerHTML" {
                        span { "Generate New Address" }
                    }
                }

                // Send Bitcoin form
                div class="box has-background-light" {
                    h2 class="subtitle has-text-weight-bold" {
                        span class="icon-text" {
                            span { "Send Bitcoin" }
                        }
                    }
                    div class="send-form" {
                        form hx-post="/admin/wallet/send"
                             hx-target="#send-result"
                             hx-swap="innerHTML" {
                            div class="field" {
                                label class="label" { "Destination Address" }
                                div class="control" {
                                    input class="input" type="text" name="address_to"
                                          placeholder="Bitcoin address" required;
                                }
                            }

                            div class="columns is-mobile" {
                                div class="column" {
                                    div class="field" {
                                        label class="label" { "Amount (sats)" }
                                        div class="control" {
                                            input class="input" type="number" name="amount"
                                                  placeholder="Amount";
                                        }
                                    }
                                }

                                div class="column" {
                                    div class="field" {
                                        label class="label" { "Max Fee (sats)" }
                                        div class="control" {
                                            input class="input" type="number" name="max_fee"
                                                  placeholder="Max fee";
                                        }
                                    }
                                }
                            }

                            div class="field" {
                                div class="control" {
                                    button class="button is-primary is-fullwidth" type="submit" {
                                        span { "Send Bitcoin" }
                                    }
                                }
                            }
                        }
                        div id="send-result" class="mt-3" {}
                    }
                }

                // Fee estimates table
                div class="box has-background-light" {
                    h2 class="subtitle has-text-weight-bold" {
                        span class="icon-text" {
                            span { "Estimated Fee Rates" }
                        }
                    }
                    div class="table-container" {
                        table class="table is-striped is-fullwidth is-hoverable" {
                            thead {
                                tr {
                                    th { "Target Blocks" }
                                    th { "Fee Rate (sats/vB)" }
                                }
                            }
                            tbody hx-get="/admin/wallet/fees"
                                  hx-trigger="load, every 60s"
                                  hx-swap="innerHTML" {
                                // Loading indicator
                                tr {
                                    td colspan="2" class="has-text-centered" {
                                        "Loading..."
                                    }
                                }
                            }
                        }
                    }
                    button class="button is-info is-outlined is-fullwidth mt-3"
                           hx-get="/admin/wallet/fees"
                           hx-target="table tbody"
                           hx-swap="innerHTML" {
                        span { "Refresh Fee Estimates" }
                    }
                }

                // Wallet outputs table
                div class="box has-background-light" {
                    h2 class="subtitle has-text-weight-bold" {
                        span class="icon-text" {
                            span { "Wallet Outputs" }
                        }
                    }
                    div class="table-container" {
                        table class="table is-striped is-fullwidth is-hoverable" {
                            thead {
                                tr {
                                    th { "TxID" }
                                    th { "Amount (sats)" }
                                    th { "Address" }
                                    th { "Status" }
                                }
                            }
                            tbody hx-get="/admin/wallet/outputs"
                                  hx-trigger="load, every 60s"
                                  hx-swap="innerHTML" {
                                // Loading indicator
                                tr {
                                    td colspan="4" class="has-text-centered" {
                                        "Loading..."
                                    }
                                }
                            }
                        }
                    }
                    button class="button is-info is-outlined is-fullwidth mt-3"
                           hx-get="/admin/wallet/outputs"
                           hx-target="table tbody"
                           hx-swap="innerHTML" {
                        span { "Refresh Outputs" }
                    }
                }
            }
        }
    }
}

/// Balance section fragment (for HTMX refresh)
pub fn wallet_balance_section(balance: &WalletBalance) -> Markup {
    html! {
        h2 class="subtitle has-text-weight-bold" {
            span class="icon-text" {
                span { "Wallet Balance" }
            }
        }
        div id="balance-display" class="content" {
            div class="columns is-mobile" {
                div class="column" {
                    div class="notification is-info is-light" {
                        p class="heading" { "Confirmed Balance" }
                        p class="title" id="confirmed-balance" { (balance.confirmed) }
                        p class="subtitle is-6" { "sats" }
                    }
                }
                div class="column" {
                    div class="notification is-warning is-light" {
                        p class="heading" { "Unconfirmed Balance" }
                        p class="title" id="unconfirmed-balance" { (balance.unconfirmed) }
                        p class="subtitle is-6" { "sats" }
                    }
                }
            }
        }
        button class="button is-info is-outlined is-fullwidth mt-3"
               hx-get="/admin/wallet/balance"
               hx-target="closest .box"
               hx-swap="innerHTML" {
            span { "Refresh Balance" }
        }
    }
}

/// Fee estimates rows fragment
pub fn fee_estimates_rows(estimates: &HashMap<u16, f64>) -> Markup {
    let mut sorted: Vec<_> = estimates.iter().collect();
    sorted.sort_by_key(|(blocks, _)| *blocks);

    html! {
        @for (blocks, fee_rate) in sorted {
            tr {
                td { (blocks) }
                td { (format!("{:.1}", fee_rate)) }
            }
        }
    }
}

/// Wallet outputs rows fragment
pub fn wallet_outputs_rows(outputs: &[WalletOutput]) -> Markup {
    html! {
        @for output in outputs {
            tr {
                td {
                    code {
                        (output.outpoint.split(':').next().unwrap_or(&output.outpoint))
                    }
                }
                td { (output.txout.value) }
                td {
                    code {
                        (output.txout.script_pubkey.as_deref().unwrap_or("-"))
                    }
                }
                td {
                    @if output.is_spent {
                        span class="tag is-warning" { "Spent" }
                    } @else {
                        span class="tag is-success" { "Unspent" }
                    }
                }
            }
        }
    }
}

/// Send result success fragment
pub fn send_success(txid: &str) -> Markup {
    html! {
        div class="notification is-info is-light" {
            p { "Transaction sent successfully!" }
            pre class="has-background-white" {
                code { (txid) }
            }
        }
    }
}

/// Send result error fragment
pub fn send_error(message: &str) -> Markup {
    html! {
        div class="notification is-danger is-light" {
            p { "Failed to send: " (message) }
        }
    }
}
