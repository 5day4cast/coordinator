//! Lightning into an escrow VTXO, with the service's own Ark liquidity.
//!
//! 1. `create` makes a hold invoice for a fresh preimage, for the escrow's amount.
//! 2. Once the payer's HTLC is held (`ACCEPTED`), the service pays the escrow from its Ark wallet.
//! 3. Once the escrow VTXO exists, it settles the invoice with the preimage.
//!
//! If the escrow cannot be paid, the invoice is cancelled and the payment fails back to the payer.
//! The HTLC is held for seconds, and the service fronts one escrow's value per swap in flight.
//!
//! Each escrow payment is recorded before it is sent. Before paying or cancelling, the service
//! asks Arkade whether the escrow was already paid, and after a payment whose outcome is unknown
//! it waits until Arkade would list it before concluding it never landed. A crash, or a send
//! that failed after arkd accepted it, therefore never pays twice or refunds a funded escrow.

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

/// How long after sending an escrow payment Arkade's indexer is given to list it. Until then
/// a payment whose outcome is unknown is only looked for, never sent again.
const PAYMENT_LISTING_GRACE_SECS: i64 = 60;

/// Escrow payments that Arkade showed never landed, before the swap fails and the payer's
/// invoice is cancelled.
const PAY_ATTEMPTS: u32 = 3;

/// The first wait before looking a settled swap's escrow VTXO up again. Each miss doubles it,
/// up to `VTXO_LOOKUP_MAX_WAIT_SECS`.
const VTXO_LOOKUP_FIRST_WAIT_SECS: i64 = 5;
const VTXO_LOOKUP_MAX_WAIT_SECS: i64 = 10 * 60;

/// Lookups of a settled swap's escrow VTXO before the service gives up, about two hours in all.
/// The indexer lists a payment within seconds, so this is far more than it needs.
const VTXO_LOOKUPS: u32 = 20;

pub struct Swapper {
    pub store: Store,
    pub lnd: Lnd,
    pub wallet: ArkWallet,
    pub invoice_expiry_secs: u64,
    pub invoice_cltv_expiry: u32,
    /// The last error each swap logged, so a lasting one is logged once, not every tick.
    pub errors: SwapErrors,
}

/// The last error logged for each swap.
#[derive(Default)]
pub struct SwapErrors(std::sync::Mutex<std::collections::HashMap<Uuid, String>>);

impl SwapErrors {
    /// Log `error` for `swap` at warn the first time, and at debug while it repeats.
    fn report(&self, swap: Uuid, error: &anyhow::Error) {
        let message = format!("{error:#}");
        let mut last = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if last.get(&swap) == Some(&message) {
            log::debug!("swap {swap}: {message}");
        } else {
            log::warn!("swap {swap}: {message}");
            last.insert(swap, message);
        }
    }

    fn clear(&self, swap: Uuid) {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&swap);
    }
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
            vtxo_lookups: 0,
            vtxo_lookup_after: None,
            vtxo_lookup_gave_up_at: None,
            pay_attempted_at: None,
            pay_attempts: 0,
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

    /// Advance every unfinished swap by one step, those holding a payer's HTLC first.
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
            match self.advance(swap).await {
                Ok(()) => self.errors.clear(id),
                Err(error) => self.errors.report(id, &error),
            }
        }
    }

    /// Look up the escrow VTXO of each settled swap that is due, apart from the live swaps and
    /// on a backoff, until it is found or the service gives up.
    pub async fn lookup_tick(&self) {
        let now = unix_now();
        let swaps = match self.store.vtxo_lookups_due(now).await {
            Ok(swaps) => swaps,
            Err(error) => {
                log::error!("list swaps whose escrow VTXO is unknown: {error:#}");
                return;
            }
        };
        for mut swap in swaps {
            let id = swap.id;
            if let Err(error) = self.record_escrow_vtxo(&mut swap, now).await {
                log::error!("swap {id}: record its escrow VTXO lookup: {error:#}");
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

    /// Look up the escrow VTXO of a swap that paid it before the indexer listed it, and record
    /// it or when to look again. The coordinator counts the ticket as paid only once it knows
    /// which VTXO holds the buy-in. Misses log at debug; giving up logs once.
    async fn record_escrow_vtxo(&self, swap: &mut Swap, now: i64) -> anyhow::Result<()> {
        let missed = match self.already_paid(swap).await {
            Ok(Some(vtxo)) => {
                log::info!("swap {} escrow VTXO is {vtxo}", swap.id);
                swap.escrow_vtxo = Some(vtxo);
                swap.updated_at = now;
                return self.store.update(swap).await;
            }
            Ok(None) => "it is not listed yet".to_string(),
            Err(error) => format!("{error:#}"),
        };
        swap.vtxo_lookups += 1;
        swap.updated_at = now;
        if swap.vtxo_lookups >= VTXO_LOOKUPS {
            swap.vtxo_lookup_gave_up_at = Some(now);
            swap.error = Some(format!(
                "its escrow VTXO was not found in {} lookups: {missed}",
                swap.vtxo_lookups
            ));
            log::warn!(
                "swap {} paid escrow {} in {} but its VTXO was not found in {} lookups ({missed}); \
                 the coordinator looks for it on Arkade itself, and an operator may need to",
                swap.id,
                swap.escrow_address,
                swap.ark_txid.as_deref().unwrap_or("an unknown transaction"),
                swap.vtxo_lookups
            );
        } else {
            let wait = vtxo_lookup_wait(swap.vtxo_lookups);
            swap.vtxo_lookup_after = Some(now + wait);
            log::debug!(
                "swap {}: its escrow VTXO lookup {} missed ({missed}); again in {wait}s",
                swap.id,
                swap.vtxo_lookups
            );
        }
        self.store.update(swap).await
    }

    /// Pay the escrow while the payer's HTLC is held, then settle.
    ///
    /// Each payment is recorded before it is sent, so a payment whose outcome was lost (to a
    /// crash, or to a send that reported an error after arkd took it) is looked for on Arkade
    /// until the indexer would list it. Only then is it sent again, and only after
    /// `PAY_ATTEMPTS` such payments does the swap fail and cancel the payer's invoice.
    async fn pay_escrow(&self, swap: &mut Swap) -> anyhow::Result<()> {
        let now = unix_now();
        let listed = self.already_paid(swap).await?;
        match payment_step(swap, listed.is_some(), now) {
            PaymentStep::Record => {
                swap.escrow_vtxo = listed;
                self.transition(swap, SwapState::EscrowPaid, None).await?;
                self.settle(swap).await
            }
            PaymentStep::Wait => {
                log::debug!(
                    "swap {}: waiting for Arkade to list the escrow payment sent at {:?}",
                    swap.id,
                    swap.pay_attempted_at
                );
                Ok(())
            }
            PaymentStep::GiveUp => {
                self.lnd.cancel(&bytes32(&swap.payment_hash)?).await?;
                let error = format!(
                    "{} escrow payments never reached Arkade; the last: {}",
                    swap.pay_attempts,
                    swap.error.as_deref().unwrap_or("no error was reported")
                );
                self.transition(swap, SwapState::Failed, Some(error.clone()))
                    .await?;
                log::error!("swap {} failed and was cancelled: {error}", swap.id);
                Ok(())
            }
            PaymentStep::Pay => {
                if let Some(attempted) = swap.pay_attempted_at {
                    log::warn!(
                        "swap {}: the escrow payment sent at {attempted} never reached Arkade; \
                         paying again",
                        swap.id
                    );
                }
                // Recorded before sending: whatever happens next, a later pass looks for this
                // payment before it sends another.
                swap.pay_attempted_at = Some(now);
                swap.pay_attempts += 1;
                swap.updated_at = now;
                self.store.update(swap).await?;
                let address = self.wallet.escrow_address(&swap.escrow_address)?;
                match self
                    .wallet
                    .pay(address, Amount::from_sat(swap.amount_sat))
                    .await
                {
                    Ok(txid) => {
                        swap.ark_txid = Some(txid.to_string());
                        // The indexer may not list the new VTXO yet. Record the payment
                        // regardless; `lookup_tick` finds the VTXO later.
                        swap.escrow_vtxo = match self.already_paid(swap).await {
                            Ok(vtxo) => vtxo,
                            Err(error) => {
                                log::warn!("swap {}: look up its escrow VTXO: {error:#}", swap.id);
                                None
                            }
                        };
                        self.transition(swap, SwapState::EscrowPaid, None).await?;
                        log::info!(
                            "swap {} paid escrow {} in {txid}",
                            swap.id,
                            swap.escrow_address
                        );
                        self.settle(swap).await
                    }
                    Err(error) => {
                        // arkd may have taken the payment anyway, so the invoice stays held:
                        // the next passes look for it on Arkade before anything else.
                        swap.error = Some(format!("{error:#}"));
                        swap.updated_at = unix_now();
                        self.store.update(swap).await?;
                        Err(error.context(
                            "the escrow payment reported an error; Arkade is checked for it \
                             before any retry",
                        ))
                    }
                }
            }
        }
    }

    async fn already_paid(&self, swap: &Swap) -> anyhow::Result<Option<String>> {
        let address = self.wallet.escrow_address(&swap.escrow_address)?;
        let paid_in = swap
            .ark_txid
            .as_deref()
            .map(str::parse::<bitcoin::Txid>)
            .transpose()
            .context("the swap's Ark transaction id")?;
        let paid = self
            .wallet
            .paid_vtxo(
                address,
                Amount::from_sat(swap.amount_sat),
                swap.created_at - EXPIRY_GRACE_SECS,
                paid_in,
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

/// What a swap paying its escrow does next.
#[derive(Debug, PartialEq, Eq)]
enum PaymentStep {
    /// Arkade lists the payment: record it and settle.
    Record,
    /// Send a payment: none was sent, or the last never landed.
    Pay,
    /// A payment was sent too recently to tell whether it landed.
    Wait,
    /// No payment landed after `PAY_ATTEMPTS`: fail, and give the payer's money back.
    GiveUp,
}

/// What a swap paying its escrow does next at `now`, given whether Arkade lists its payment.
fn payment_step(swap: &Swap, listed: bool, now: i64) -> PaymentStep {
    if listed {
        return PaymentStep::Record;
    }
    match swap.pay_attempted_at {
        None => PaymentStep::Pay,
        Some(attempted) if now < attempted + PAYMENT_LISTING_GRACE_SECS => PaymentStep::Wait,
        Some(_) if swap.pay_attempts >= PAY_ATTEMPTS => PaymentStep::GiveUp,
        Some(_) => PaymentStep::Pay,
    }
}

/// How long to wait after the `lookups`th missed lookup of a settled swap's escrow VTXO.
fn vtxo_lookup_wait(lookups: u32) -> i64 {
    VTXO_LOOKUP_FIRST_WAIT_SECS
        .saturating_mul(1i64 << lookups.saturating_sub(1).min(16))
        .min(VTXO_LOOKUP_MAX_WAIT_SECS)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn paying(pay_attempted_at: Option<i64>, pay_attempts: u32) -> Swap {
        Swap {
            id: Uuid::now_v7(),
            escrow_address: "tark1escrow".into(),
            amount_sat: 6_300,
            payment_hash: "ab".repeat(32),
            preimage: "cd".repeat(32),
            invoice: "lntb1".into(),
            state: SwapState::PayingEscrow,
            escrow_vtxo: None,
            ark_txid: None,
            error: None,
            created_at: 1_790_000_000,
            updated_at: 1_790_000_000,
            expires_at: 1_790_000_600,
            vtxo_lookups: 0,
            vtxo_lookup_after: None,
            vtxo_lookup_gave_up_at: None,
            pay_attempted_at,
            pay_attempts,
        }
    }

    #[test]
    fn an_escrow_is_paid_again_only_once_arkade_shows_the_last_payment_never_landed() {
        let sent = 1_790_000_010;
        assert_eq!(payment_step(&paying(None, 0), false, sent), PaymentStep::Pay);
        // Sent, then a crash or an error: however the send ended, it is looked for first.
        assert_eq!(
            payment_step(&paying(Some(sent), 1), false, sent + 1),
            PaymentStep::Wait
        );
        assert_eq!(
            payment_step(&paying(Some(sent), 1), false, sent + PAYMENT_LISTING_GRACE_SECS - 1),
            PaymentStep::Wait
        );
        assert_eq!(
            payment_step(&paying(Some(sent), 1), true, sent + 1),
            PaymentStep::Record,
            "a payment that landed is recorded, never sent again"
        );
        assert_eq!(
            payment_step(&paying(Some(sent), 1), false, sent + PAYMENT_LISTING_GRACE_SECS),
            PaymentStep::Pay,
            "Arkade would list it by now, so it never landed"
        );
        assert_eq!(
            payment_step(
                &paying(Some(sent), PAY_ATTEMPTS),
                false,
                sent + PAYMENT_LISTING_GRACE_SECS
            ),
            PaymentStep::GiveUp,
            "the payer's invoice is cancelled only once no payment landed"
        );
        assert_eq!(
            payment_step(&paying(Some(sent), PAY_ATTEMPTS), true, sent + 10_000),
            PaymentStep::Record
        );
    }

    #[test]
    fn escrow_vtxo_lookups_back_off_and_give_up_within_hours() {
        assert_eq!(vtxo_lookup_wait(1), 5);
        assert_eq!(vtxo_lookup_wait(2), 10);
        assert_eq!(vtxo_lookup_wait(3), 20);
        assert_eq!(vtxo_lookup_wait(8), 600, "capped");
        assert_eq!(vtxo_lookup_wait(40), 600);
        let total: i64 = (1..VTXO_LOOKUPS).map(vtxo_lookup_wait).sum();
        assert!(
            (3_600..4 * 3_600).contains(&total),
            "a swap is looked up for {total}s before the service gives up"
        );
    }
}
