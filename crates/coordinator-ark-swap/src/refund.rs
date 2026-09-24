//! Refunding an entry escrow whose competition never kicked off.
//!
//! 1. The coordinator resolves the player's Lightning Address and asks for a swap that commits to
//!    that invoice's payment hash. `mint` returns the swap's script and address.
//! 2. The coordinator refunds the escrow into that swap, with Keymeld signing as the player.
//! 3. The coordinator pays the invoice and reports the preimage the payment revealed.
//! 4. `tick` claims the swap into this service's wallet, once the refund has paid it.
//!
//! The service can only take the swap by revealing that preimage, so it is paid for the coins it
//! claims. If it never claims, the player takes the swap back after the deadline, which costs the
//! service the payment it already made, so a stalled claim is an operator's problem.

use anyhow::Context;
use bitcoin::{Amount, XOnlyPublicKey};
use coordinator_ark_escrow::{RefundSwap, SwapTerms, VtxoScript};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::store::{Refund, RefundState};
use crate::swap::{unix_now, Swapper};

impl Swapper {
    /// Mint the swap an escrow's refund pays, or return the one already minted for this invoice.
    pub async fn mint_refund(
        &self,
        payment_hash: [u8; 32],
        amount_sat: u64,
        player: XOnlyPublicKey,
        deadline: u32,
    ) -> anyhow::Result<Refund> {
        let amount = Amount::from_sat(amount_sat);
        anyhow::ensure!(amount >= self.wallet.dust(), "the amount is below dust");
        let hash = hex::encode(payment_hash);
        if let Some(minted) = self.store.refund_for_hash(&hash).await? {
            anyhow::ensure!(
                minted.amount_sat == amount_sat && minted.player_key == player.to_string(),
                "a swap for this invoice was minted for a different amount or player"
            );
            return Ok(minted);
        }

        let now = unix_now();
        let created_at = u32::try_from(now).context("the clock is out of range")?;
        let locktime = bitcoin::absolute::LockTime::from_time(deadline)
            .map_err(|error| anyhow::anyhow!("the deadline is not a timestamp: {error}"))?;
        let exit_delay = self.wallet.exit_delay();
        let swap = RefundSwap::new(SwapTerms {
            player,
            swapper: self.wallet.swapper_key(),
            server: self.wallet.server_key(),
            payment_hash,
            deadline: locktime,
            exit_delay,
            unilateral_reclaim_delay: SwapTerms::unilateral_reclaim_delay_for(
                locktime, exit_delay, created_at,
            )?,
        })?;
        self.wallet.accepts(swap.vtxo_script())?;

        let refund = Refund {
            id: Uuid::now_v7(),
            payment_hash: hash,
            amount_sat,
            player_key: player.to_string(),
            deadline: i64::from(deadline),
            swap_tap_tree: hex::encode(swap.vtxo_script().encode_tap_tree()),
            swap_address: swap.address(self.wallet.hrp())?.encode(),
            state: RefundState::Minted,
            preimage: None,
            swap_vtxo: None,
            claim_txid: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        self.store.insert_refund(&refund).await?;
        log::info!(
            "refund {} minted a swap at {} for {amount_sat} sats",
            refund.id,
            refund.swap_address
        );
        Ok(refund)
    }

    /// Record the preimage the coordinator's payment revealed, and try to claim at once.
    pub async fn refund_paid(&self, id: Uuid, preimage: [u8; 32]) -> anyhow::Result<Refund> {
        let mut refund = self
            .store
            .refund(id)
            .await?
            .with_context(|| format!("no refund {id}"))?;
        let hash: [u8; 32] = Sha256::digest(preimage).into();
        anyhow::ensure!(
            hex::encode(hash) == refund.payment_hash,
            "this preimage does not settle the refund's invoice"
        );
        if refund.preimage.is_none() {
            refund.preimage = Some(hex::encode(preimage));
            self.transition_refund(&mut refund, RefundState::Paid, None)
                .await?;
        }
        if let Err(error) = self.claim_refund(&mut refund).await {
            log::warn!("refund {id}: {error:#}");
        }
        Ok(refund)
    }

    /// Claim every refund whose swap has been paid, and retire those the player may take back.
    pub async fn refund_tick(&self) {
        let refunds = match self.store.unclaimed_refunds().await {
            Ok(refunds) => refunds,
            Err(error) => {
                log::error!("list unclaimed refunds: {error:#}");
                return;
            }
        };
        for mut refund in refunds {
            let id = refund.id;
            if let Err(error) = self.advance_refund(&mut refund).await {
                log::warn!("refund {id}: {error:#}");
            }
        }
    }

    async fn advance_refund(&self, refund: &mut Refund) -> anyhow::Result<()> {
        // Past the deadline the player can take the swap back, so this service stops trying.
        if unix_now() >= refund.deadline {
            let unclaimed = refund.preimage.is_some();
            self.transition_refund(
                refund,
                RefundState::Reclaimable,
                unclaimed.then(|| "the deadline passed before the swap was claimed".to_string()),
            )
            .await?;
            if unclaimed {
                log::error!(
                    "refund {} paid the player but never claimed its swap",
                    refund.id
                );
            }
            return Ok(());
        }
        if refund.preimage.is_some() {
            self.claim_refund(refund).await?;
        }
        Ok(())
    }

    /// Claim the swap, once the refund has paid it.
    async fn claim_refund(&self, refund: &mut Refund) -> anyhow::Result<()> {
        let Some(preimage) = refund.preimage.as_deref() else {
            return Ok(());
        };
        let preimage = bytes32(preimage)?;
        let swap = self.refund_swap_of(refund)?;
        let amount = Amount::from_sat(refund.amount_sat);
        let Some(outpoint) = self
            .wallet
            .paid_vtxo(
                self.wallet.escrow_address(&refund.swap_address)?,
                amount,
                refund.created_at,
            )
            .await?
        else {
            log::debug!("refund {}: its swap is not funded yet", refund.id);
            return Ok(());
        };
        refund.swap_vtxo = Some(outpoint.to_string());
        let txid = self
            .wallet
            .claim_refund_swap(&swap, outpoint, amount, preimage)
            .await?;
        refund.claim_txid = Some(txid.to_string());
        self.transition_refund(refund, RefundState::Claimed, None)
            .await?;
        log::info!("refund {} claimed its swap in {txid}", refund.id);
        Ok(())
    }

    fn refund_swap_of(&self, refund: &Refund) -> anyhow::Result<RefundSwap> {
        let tap_tree = hex::decode(&refund.swap_tap_tree)?;
        let vtxo = VtxoScript::decode_tap_tree(&tap_tree)?;
        Ok(RefundSwap::from_vtxo_script(&vtxo)?)
    }

    async fn transition_refund(
        &self,
        refund: &mut Refund,
        state: RefundState,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        refund.state = state;
        refund.error = error;
        refund.updated_at = unix_now();
        self.store.update_refund(refund).await
    }
}

fn bytes32(hex_value: &str) -> anyhow::Result<[u8; 32]> {
    hex::decode(hex_value)?
        .try_into()
        .ok()
        .context("expected 32 bytes")
}
