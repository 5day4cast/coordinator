//! Lightning into an escrow VTXO, with the service's own Ark liquidity.
//!
//! 1. `create` makes a hold invoice for a fresh preimage, for the escrow's amount.
//! 2. Once the payer's HTLC is held (`ACCEPTED`), the service pays the escrow from its Ark wallet.
//! 3. Once the escrow VTXO exists, it settles the invoice with the preimage.
//!
//! If the escrow cannot be paid, the invoice is cancelled and the payment fails back to the payer.
//! The HTLC is held for seconds, and the service fronts one escrow's value per swap in flight.
//! Before paying or cancelling, the service checks whether the escrow was already paid.
//! A crash, or a send that failed after arkd accepted it, therefore never pays twice or refunds a funded escrow.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use bitcoin::Amount;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::lnd::{InvoiceState, Lnd};
use crate::store::{Store, Swap, SwapState};
use crate::wallet::ArkWallet;

/// How long an open invoice outlives its expiry before the service cancels it.
const EXPIRY_GRACE_SECS: i64 = 60;

pub struct Swapper {
    pub store: Store,
    pub lnd: Lnd,
    pub wallet: ArkWallet,
    pub invoice_expiry_secs: u64,
    pub invoice_cltv_expiry: u32,
}

impl Swapper {
    /// Start a swap into `escrow_address`, or return the open one for it.
    ///
    /// With `preimage`, the invoice pays to its hash, so the payer's proof of payment is a secret
    /// the caller chose, such as a competition ticket's. Without it, the service makes one.
    pub async fn create(
        &self,
        escrow_address: &str,
        amount_sat: u64,
        preimage: Option<[u8; 32]>,
    ) -> anyhow::Result<Swap> {
        let address = self.wallet.escrow_address(escrow_address)?;
        let amount = Amount::from_sat(amount_sat);
        anyhow::ensure!(amount >= self.wallet.dust(), "the amount is below dust");
        let escrow_address = address.encode();
        let requested_hash: Option<[u8; 32]> =
            preimage.map(|preimage| Sha256::digest(preimage).into());
        if let Some(open) = self.store.open_for_escrow(&escrow_address).await? {
            anyhow::ensure!(
                open.amount_sat == amount_sat
                    && requested_hash.is_none_or(|hash| hex::encode(hash) == open.payment_hash),
                "an open swap for this escrow has a different amount or payment hash"
            );
            return Ok(open);
        }

        let preimage = preimage.unwrap_or_else(rand08::random);
        let payment_hash: [u8; 32] = Sha256::digest(preimage).into();
        anyhow::ensure!(
            !self
                .store
                .payment_hash_used(&hex::encode(payment_hash))
                .await?,
            "this payment hash was already used; a new swap needs a new preimage"
        );
        let invoice = self
            .lnd
            .add_hold_invoice(
                &payment_hash,
                amount_sat,
                self.invoice_expiry_secs,
                self.invoice_cltv_expiry,
                &format!("Competition entry escrow {}", &escrow_address[..16]),
            )
            .await?;
        let now = unix_now();
        let swap = Swap {
            id: Uuid::now_v7(),
            escrow_address,
            amount_sat,
            payment_hash: hex::encode(payment_hash),
            preimage: hex::encode(preimage),
            invoice,
            state: SwapState::AwaitingPayment,
            escrow_vtxo: None,
            ark_txid: None,
            error: None,
            created_at: now,
            updated_at: now,
            expires_at: now + self.invoice_expiry_secs as i64,
        };
        self.store.insert(&swap).await?;
        log::info!(
            "swap {} awaits payment into {}",
            swap.id,
            swap.escrow_address
        );
        Ok(swap)
    }

    /// Board what has confirmed at the boarding address, so topping the wallet up takes only an
    /// on-chain send to it.
    pub async fn board_tick(&self) {
        let boarding = match self.wallet.confirmed_boarding_sat().await {
            Ok(sat) => sat,
            Err(error) => {
                log::warn!("check the boarding address: {error:#}");
                return;
            }
        };
        if boarding < self.wallet.dust().to_sat() {
            return;
        }
        match self.wallet.board().await {
            Ok(Some(txid)) => log::info!("boarded {boarding} sat in commitment {txid}"),
            Ok(None) => log::warn!("{boarding} sat await boarding, but no batch took them"),
            Err(error) => log::warn!("board {boarding} sat: {error:#}"),
        }
    }

    /// Advance every unfinished swap by one step.
    pub async fn tick(&self) {
        let swaps = match self.store.unfinished().await {
            Ok(swaps) => swaps,
            Err(error) => {
                log::error!("list unfinished swaps: {error:#}");
                return;
            }
        };
        for swap in swaps {
            let id = swap.id;
            if let Err(error) = self.advance(swap).await {
                log::warn!("swap {id}: {error:#}");
            }
        }
    }

    async fn advance(&self, mut swap: Swap) -> anyhow::Result<()> {
        let payment_hash = bytes32(&swap.payment_hash)?;
        match swap.state {
            SwapState::AwaitingPayment => match self.lnd.invoice_state(&payment_hash).await? {
                InvoiceState::Open => {
                    if unix_now() > swap.expires_at + EXPIRY_GRACE_SECS {
                        self.lnd.cancel(&payment_hash).await?;
                        self.transition(&mut swap, SwapState::Expired, None).await?;
                    }
                }
                InvoiceState::Canceled => {
                    self.transition(&mut swap, SwapState::Expired, None).await?;
                }
                InvoiceState::Settled => {
                    self.transition(&mut swap, SwapState::Settled, None).await?;
                }
                InvoiceState::Accepted => {
                    self.transition(&mut swap, SwapState::PayingEscrow, None)
                        .await?;
                    self.pay_escrow(&mut swap).await?;
                }
            },
            // Resumed after a restart.
            SwapState::PayingEscrow => self.pay_escrow(&mut swap).await?,
            SwapState::EscrowPaid => self.settle(&mut swap).await?,
            _ => {}
        }
        Ok(())
    }

    async fn pay_escrow(&self, swap: &mut Swap) -> anyhow::Result<()> {
        let address = self.wallet.escrow_address(&swap.escrow_address)?;
        let amount = Amount::from_sat(swap.amount_sat);
        if let Some(vtxo) = self.already_paid(swap).await? {
            swap.escrow_vtxo = Some(vtxo);
            self.transition(swap, SwapState::EscrowPaid, None).await?;
            return self.settle(swap).await;
        }
        match self.wallet.pay(address, amount).await {
            Ok(txid) => {
                swap.ark_txid = Some(txid.to_string());
                swap.escrow_vtxo = self.already_paid(swap).await?;
                self.transition(swap, SwapState::EscrowPaid, None).await?;
                log::info!(
                    "swap {} paid escrow {} in {txid}",
                    swap.id,
                    swap.escrow_address
                );
                self.settle(swap).await
            }
            Err(error) => {
                // arkd may have taken the payment even though the send reported an error.
                if let Some(vtxo) = self.already_paid(swap).await? {
                    swap.escrow_vtxo = Some(vtxo);
                    self.transition(swap, SwapState::EscrowPaid, None).await?;
                    return self.settle(swap).await;
                }
                self.lnd.cancel(&bytes32(&swap.payment_hash)?).await?;
                self.transition(swap, SwapState::Failed, Some(format!("{error:#}")))
                    .await?;
                log::error!("swap {} failed and was cancelled: {error:#}", swap.id);
                Ok(())
            }
        }
    }

    async fn already_paid(&self, swap: &Swap) -> anyhow::Result<Option<String>> {
        let address = self.wallet.escrow_address(&swap.escrow_address)?;
        let paid = self
            .wallet
            .paid_vtxo(
                address,
                Amount::from_sat(swap.amount_sat),
                swap.created_at - EXPIRY_GRACE_SECS,
            )
            .await?;
        Ok(paid.map(|outpoint| outpoint.to_string()))
    }

    async fn settle(&self, swap: &mut Swap) -> anyhow::Result<()> {
        let preimage = bytes32(&swap.preimage)?;
        match self.lnd.settle(&preimage).await {
            Ok(()) => {
                self.transition(swap, SwapState::Settled, None).await?;
                log::info!("swap {} settled", swap.id);
                Ok(())
            }
            Err(error) => match self
                .lnd
                .invoice_state(&bytes32(&swap.payment_hash)?)
                .await?
            {
                InvoiceState::Settled => self.transition(swap, SwapState::Settled, None).await,
                InvoiceState::Canceled => {
                    log::error!(
                        "swap {} paid its escrow, but the invoice was cancelled",
                        swap.id
                    );
                    self.transition(swap, SwapState::Unsettled, Some(format!("{error:#}")))
                        .await
                }
                // A transient error: stay in EscrowPaid and retry on the next tick.
                _ => Err(error),
            },
        }
    }

    async fn transition(
        &self,
        swap: &mut Swap,
        state: SwapState,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        swap.state = state;
        swap.error = error;
        swap.updated_at = unix_now();
        self.store.update(swap).await
    }
}

fn bytes32(hex_value: &str) -> anyhow::Result<[u8; 32]> {
    hex::decode(hex_value)?
        .try_into()
        .ok()
        .context("expected 32 bytes")
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64
}
