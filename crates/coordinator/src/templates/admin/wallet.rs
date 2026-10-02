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

/// Operator view: service funds, their purpose, and the relevant management tools.
pub fn wallet_page(network: &str, data: &crate::domain::admin_wallet::WalletOverview) -> Markup {
    let node = data.node.as_ref();
    let channels = data.channels.as_ref();
    let ark = data.ark.as_ref();
    let now = time::OffsetDateTime::now_utc();
    html! {
        main.admin-workspace {
            header.page-heading {
                p.eyebrow { "Treasury operations · " (network) }
                h1 { "Node & wallets" }
                p { "Manage Lightning liquidity, on-chain fees, and the Ark wallet that funds swaps." }
                p.note { "Observed at " (now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default())
                    ". Service balances are separate from funds already committed to customer escrows and DLCs." }
                div.wallet-links {
                    a href="/admin/wallet" { "Refresh status" }
                    a href="/admin/funds" { "Find a customer's money →" }
                    a href="/admin/operations" { "Competition actions →" }
                }
            }
            div.wallet-grid {
                section.wallet-card {
                    p.eyebrow { "01 / LND" }
                    h2 { "Lightning" }
                    p { "Receives payments and sends payouts. Swaps also need liquidity on their configured LND node." }
                    @if node.is_none() {
                        p.notice { "Node status unavailable. Check LND connectivity and read permissions." }
                    }
                    dl.wallet-metrics {
                        (fact("Node", node.and_then(|n| n.alias.as_deref()).unwrap_or("Unavailable")))
                        (fact("Chain sync", sync(node.and_then(|n| n.synced_to_chain))))
                        (fact("Active / inactive channels", &match node {
                            Some(n) => format!("{} / {}", number(n.num_active_channels), number(n.num_inactive_channels)),
                            None => "Unknown".into(),
                        }))
                        (fact("Local channel balance", &channel_sats(channels.and_then(|c| c.local_balance.as_ref()))))
                        (fact("Remote channel balance", &channel_sats(channels.and_then(|c| c.remote_balance.as_ref()))))
                    }
                    p.note { "Local balance supports sending; remote balance supports receiving. Routes, reserves, and offline peers can limit both." }
                    details {
                        summary { "Manage channels & payments" }
                        p { "Use lncli on the node configured for this coordinator. Confirm its public key and network before making changes." }
                        pre { "lncli getinfo\nlncli channelbalance\nlncli listchannels\nlncli pendingchannels" }
                        dl.wallet-metrics {
                            (fact("Block height", &number(node.and_then(|n| n.block_height))))
                            (fact("Graph sync", sync(node.and_then(|n| n.synced_to_graph))))
                            (fact("Pending channels", &number(node.and_then(|n| n.num_pending_channels))))
                            (fact("Unsettled local", &channel_sats(channels.and_then(|c| c.unsettled_local_balance.as_ref()))))
                            (fact("Pending open local", &channel_sats(channels.and_then(|c| c.pending_open_local_balance.as_ref()))))
                            (fact("Public key", node.and_then(|n| n.identity_pubkey.as_deref()).unwrap_or("Unknown")))
                            (fact("Version", node.and_then(|n| n.version.as_deref()).unwrap_or("Unknown")))
                        }
                        ul {
                            li { "If payouts fail, inspect the payment hash and failure reason in the customer's service check. Then check sending liquidity and peers." }
                            li { "If receiving is constrained, inspect remote balances and arrange inbound liquidity." }
                            li { "For held entry invoices, use the ticket's service check and competition state before canceling or settling anything." }
                        }
                        a href="https://docs.lightning.engineering/the-lightning-network/liquidity/manage-liquidity" { "LND liquidity guide ↗" }
                    }
                }
                section.wallet-card {
                    p.eyebrow { "02 / LND" }
                    h2 { "On-chain" }
                    p { "The coordinator's LND wallet funds transaction fees and wallet inputs for competition transactions." }
                    @if data.onchain.is_none() {
                        p.notice { "Wallet balance unavailable. Check LND connectivity and wallet permissions." }
                    }
                    dl.wallet-metrics {
                        (fact("Confirmed", &sats(data.onchain.as_ref().map(|b| b.confirmed.to_sat()))))
                        (fact("Unconfirmed", &sats(data.onchain.as_ref().map(|b| b.unconfirmed.to_sat()))))
                        (fact("Locked / reserved", &sats(data.onchain.as_ref().map(|b| b.locked.to_sat()))))
                    }
                    p.note { "Reserved coins may back a signing session. Confirmed balance alone does not show what is free to spend." }
                    details {
                        summary { "Manage fees & stuck transactions" }
                        pre { "lncli walletbalance\nlncli listunspent\nlncli listchaintxns\nlncli wallet listleases\nlncli wallet pendingsweeps" }
                        ol {
                            li { "Open the competition's funding or payout transaction. Match its txid, inputs, and outputs to LND and the explorer." }
                            li { "Check confirmations, fee rate, reserved inputs, and whether LND owns an output that can pay for a child transaction." }
                            li { "For an LND-managed sweep or wallet output, inspect the available bump options below. Choose a fee budget in your node tooling." }
                            li { "DLC transactions commit to specific funding outputs. Use the competition's supported recovery flow before replacing funding or releasing its input leases." }
                            li { "After acting, refresh the transaction and the customer's service check to verify confirmation and payout status." }
                        }
                        pre { "lncli wallet bumpfee --help" }
                        p.note { "A fee bump may use a child transaction (CPFP) or replace a pending sweep (RBF). Eligibility depends on the output and LND version." }
                        a href="https://docs.lightning.engineering/lightning-network-tools/lnd/unconfirmed-bitcoin-transactions" { "LND fee-bump guide ↗" }
                    }
                }
                section.wallet-card {
                    p.eyebrow { "03 / ark-swapd" }
                    h2 { "Ark swap wallet" }
                    p { "Supplies Ark coins to entry escrows when players pay Lightning. Refund swaps return escrow funds through Lightning." }
                    @if ark.is_none() {
                        p.notice {
                            @if data.ark_configured { "Swap wallet unavailable. Check ark-swapd connectivity and its API token." }
                            @else { "Ark swaps are not configured for this coordinator." }
                        }
                    }
                    dl.wallet-metrics {
                        (fact("Can fund new escrows", &sats(ark.and_then(|w| w.payable_sat))))
                        (fact("Awaiting boarding", &sats(ark.and_then(|w| w.boarding_sat))))
                        (fact("Needs renewal", &sats(ark.and_then(|w| w.expiring_sat))))
                        (fact("Needs recovery", &sats(ark.and_then(|w| w.recoverable_sat))))
                        (fact("Earliest VTXO expiry", &at(ark.and_then(|w| w.earliest_expiry))))
                    }
                    @if let Some(failure) = ark.and_then(|w| w.last_board_failure.as_ref()) {
                        @if ark.and_then(|w| w.last_board_success_at).is_none_or(|success| failure.at > success) {
                            p.notice { "A boarding or renewal failure is newer than the last success. Check the swap service before opening more entries." }
                        }
                    }
                    details {
                        summary { "Top up, board & renew" }
                        dl.wallet-metrics {
                            (fact("Batch-confirmed", &sats(ark.and_then(|w| w.confirmed_sat))))
                            (fact("Pre-confirmed", &sats(ark.and_then(|w| w.pre_confirmed_sat))))
                            (fact("Boarding address · on-chain", ark.and_then(|w| w.boarding_address.as_deref()).unwrap_or("Unavailable")))
                            (fact("Ark address · off-chain", ark.and_then(|w| w.ark_address.as_deref()).unwrap_or("Unavailable")))
                            (fact("Last successful batch", &at(ark.and_then(|w| w.last_board_success_at))))
                        }
                        p.note { "Payable and expiring coins are portions of batch-confirmed plus pre-confirmed balances. Do not add them together." }
                        ol {
                            li { "If payable funds are low, check boarding, renewal, and recovery amounts before sending a top-up." }
                            li { "To add on-chain funds, verify the boarding address against the active swap service, then send using your wallet tooling." }
                            li { "Wait for confirmation and a successful Ark batch. The swap worker runs boarding and renewal automatically." }
                            li { "If confirmed funds remain awaiting boarding, inspect ark-swapd logs, its worker lease, and Arkade batch errors." }
                            li { "Verify that payable funds increase. A confirmed top-up is not yet spendable Ark liquidity." }
                        }
                        p { "The authenticated swap API exposes GET /v1/wallet. POST /v1/wallet/board requests a batch; it moves funds and can take minutes." }
                        @if let Some(failure) = ark.and_then(|w| w.last_board_failure.as_ref()) {
                            details {
                                summary { "Last batch failure · " (at(Some(failure.at))) }
                                pre { (failure.message) }
                            }
                        }
                        p.note { "Batch history is retained since the swap service started. Missing history is not proof of a successful batch." }
                    }
                }
            }
            section.wallet-support {
                h2 { "Where is a customer's money?" }
                p { "These wallets show service liquidity. Use the customer funds view to trace each competition, pool, entry, and ticket." }
                p.wallet-path { "Lightning payment → Ark escrow → DLC funding → on-chain or Lightning payout" }
                p.wallet-path { "Unused escrow → Ark refund swap → Lightning refund" }
                p.note { "For refunds, check the trigger, escrow opening time, Ark transfer, and Lightning payment separately. An opening time is not a payment receipt." }
                a href="/admin/funds" { "Trace a payment or refund →" }
            }
        }
    }
}

fn fact(label: &str, value: &str) -> Markup {
    html! { div { dt { (label) } dd { (value) } } }
}

fn sats(value: Option<u64>) -> String {
    value
        .map(|n| format!("{n} sats"))
        .unwrap_or_else(|| "Unknown".into())
}

fn channel_sats(amount: Option<&crate::infra::lightning::ChannelAmount>) -> String {
    sats(amount.and_then(|v| v.sat.parse().ok()))
}

fn number(value: Option<u32>) -> String {
    value
        .map(|n| n.to_string())
        .unwrap_or_else(|| "Unknown".into())
}

fn sync(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "Synced",
        Some(false) => "Catching up",
        None => "Unknown",
    }
}

fn at(value: Option<i64>) -> String {
    value
        .and_then(|v| time::OffsetDateTime::from_unix_timestamp(v).ok())
        .and_then(|v| {
            v.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "Not reported".into())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::admin_wallet::WalletOverview, infra::ark_swap::SwapWallet};

    #[test]
    fn admin_wallet_distinguishes_missing_services_from_empty_wallets() {
        let missing = wallet_page("signet", &WalletOverview::default()).into_string();
        assert!(missing.contains("Wallet balance unavailable"));
        assert!(missing.contains("Ark swaps are not configured"));
        assert!(!missing.contains("0 sats"));
        assert!(!missing.contains("hx-post"));
        assert!(!missing.contains("/admin/wallet/address"));
        let available = WalletOverview {
            ark_configured: true,
            ark: Some(SwapWallet {
                payable_sat: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let rendered = wallet_page("signet", &available).into_string();
        assert!(rendered.contains("0 sats"));
        assert!(!rendered.contains("Ark swaps are not configured"));
        assert!(!rendered.contains("Swap wallet unavailable"));
    }

    #[test]
    fn admin_wallet_channel_amounts_reject_invalid_or_missing_values() {
        let amounts: crate::infra::lightning::ChannelBalance = serde_json::from_str(
            r#"{"local_balance":{"sat":"42000"},"remote_balance":{"sat":"-1"}}"#,
        )
        .unwrap();
        assert_eq!(channel_sats(amounts.local_balance.as_ref()), "42000 sats");
        assert_eq!(channel_sats(amounts.remote_balance.as_ref()), "Unknown");
        assert_eq!(
            channel_sats(amounts.unsettled_local_balance.as_ref()),
            "Unknown"
        );
    }
}
