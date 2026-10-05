//! Bounded, read-only wallet observations. Missing observations never become zero balances.
use crate::infra::{
    ark_swap::SwapWallet,
    bitcoin::{ScriptFunds, WalletBalance},
    lightning::{ChannelBalance, NodeInfo},
};

/// How long the wallet page waits for a first read of the settled contracts' outputs.
const SETTLED_OUTPUTS_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Default)]
pub struct WalletOverview {
    pub node: Option<NodeInfo>,
    pub channels: Option<ChannelBalance>,
    pub onchain: Option<WalletBalance>,
    pub ark_configured: bool,
    pub ark: Option<SwapWallet>,
    /// The address of the coordinator's own key, where settled contracts' closing
    /// transactions pay.
    pub settled_address: Option<String>,
    /// The unspent outputs there at the last read, and when that was.
    pub settled: Option<(ScriptFunds, time::OffsetDateTime)>,
}

async fn observe<T>(future: impl std::future::Future<Output = anyhow::Result<T>>) -> Option<T> {
    tokio::time::timeout(std::time::Duration::from_secs(4), future)
        .await
        .ok()?
        .ok()
}

impl super::Coordinator {
    pub async fn admin_wallet_overview(&self) -> WalletOverview {
        let ark = self.ark();
        let (node, channels, onchain, wallet, settled) = tokio::join!(
            observe(self.ln.node_info()),
            observe(self.ln.channel_balance()),
            observe(self.bitcoin.get_balance()),
            async {
                match ark {
                    Some(ark) => observe(ark.swaps.wallet()).await,
                    None => None,
                }
            },
            self.settled_outputs(SETTLED_OUTPUTS_WAIT)
        );
        WalletOverview {
            node,
            channels,
            onchain,
            ark_configured: ark.is_some(),
            ark: wallet,
            settled_address: self
                .settled_outputs_address()
                .map(|address| address.to_string()),
            settled: settled
                .latest
                .as_ref()
                .and_then(|read| read.value.map(|funds| (funds, read.fetched_at))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn admin_wallet_reads_end_on_timeout_and_preserve_failures() {
        let never = std::future::pending::<anyhow::Result<u64>>();
        assert_eq!(observe(never).await, None);
        assert_eq!(
            observe(async { anyhow::bail!("offline") }).await,
            None::<u64>
        );
        assert_eq!(observe(async { Ok(0_u64) }).await, Some(0));
    }

    #[test]
    fn admin_wallet_old_ark_responses_do_not_invent_zero_balances() {
        let wallet: SwapWallet = serde_json::from_str("{}").unwrap();
        assert_eq!(wallet.payable_sat, None);
        let wallet: SwapWallet = serde_json::from_str(
            r#"{"payable_sat":0,"boarding_sat":12000,"last_board_success_at":123}"#,
        )
        .unwrap();
        assert_eq!(wallet.payable_sat, Some(0));
        assert_eq!(wallet.boarding_sat, Some(12000));
        assert_eq!(wallet.confirmed_sat, None);
    }
}
