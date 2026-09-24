//! Returning a funded escrow to its player, when the competition it paid for never kicked off.
//!
//! The player runs no Ark wallet, so the escrow is refunded into a swap that pays their Lightning
//! Address, and the swap service takes the VTXO in exchange:
//!
//! 1. Resolve the player's address for the escrow less the fee they capped, and ask `ark-swapd`
//!    for a swap committed to that invoice.
//! 2. Spend the escrow into the swap on Arkade, with Keymeld signing as the player. That takes
//!    two signatures with the server's co-signature between them.
//! 3. Pay the invoice, and give `ark-swapd` the preimage so it can claim the swap.
//!
//! Every step is recorded before the next begins, so an outage resumes rather than repeats. The
//! player is paid once, and a refunded escrow is never spent twice. Until the escrow's refund
//! locktime passes there is nothing to do: the refund leaf is not open yet.

use std::time::Duration;

use anyhow::{anyhow, Context};
use coordinator_ark::build_refund;
use coordinator_ark_escrow::{EntryEscrow, RefundSwap, VtxoScript};
use coordinator_escrow::ark::{psbt_hex, ArkEscrowSpend, RefundPurpose};
use dlctix::bitcoin::absolute::LockTime;
use dlctix::bitcoin::{Amount, OutPoint};
use log::{debug, error, info, warn};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ArkRefundState, Coordinator, TicketArkEscrow, TicketArkRefund};
use crate::domain::competitions::EntryStatus;
use crate::domain::PaymentStatus;
use crate::domain::{Error, UserEntry};
use coordinator_escrow::authorization::PayoutPolicy;

/// How long a refund's swap waits for its payment before the player may take it back. The
/// verifier bounds this too, so a swap minted with anything wilder is refused.
const SWAP_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// How long a refund's payment may take before this gives up and retries later.
const REFUND_PAYMENT_TIMEOUT: Duration = Duration::from_secs(60);

/// What a player is told about their refund.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TicketRefund {
    /// minted, submitting, submitted, paid, or settled.
    pub state: String,
    /// What the player is paid, after the swap service's fee.
    pub paid_sats: u64,
    /// The Arkade transaction that moved the escrow into the swap.
    pub ark_txid: Option<String>,
    /// UNIX seconds.
    pub updated_at: i64,
}

impl Coordinator {
    /// The refund of a player's own ticket, once their competition has one.
    pub async fn get_ticket_refund(
        &self,
        user_pubkey: String,
        competition_id: Uuid,
        ticket_id: Uuid,
    ) -> Result<Option<TicketRefund>, Error> {
        let ticket = self
            .competition_store
            .get_ticket(ticket_id)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => Error::NotFound("Ticket not found".into()),
                e => Error::from(e),
            })?;
        if ticket.competition_id != competition_id {
            return Err(Error::BadRequest(
                "Ticket does not belong to this competition".into(),
            ));
        }
        if ticket.reserved_by.as_deref() != Some(&user_pubkey) {
            return Err(Error::BadRequest("Ticket not reserved by this user".into()));
        }
        let Some(refund) = self.competition_store.ticket_ark_refund(ticket_id).await? else {
            return Ok(None);
        };
        let escrow = self
            .competition_store
            .ticket_ark_escrow(ticket_id, &ticket.hash)
            .await?;
        let paid_sats = escrow
            .and_then(|escrow| escrow.vtxo_sats)
            .unwrap_or_default()
            .saturating_sub(refund.fee_sats);
        Ok(Some(TicketRefund {
            state: refund.state.as_str().into(),
            paid_sats,
            ark_txid: refund.ark_txid,
            updated_at: refund.updated_at,
        }))
    }

    /// Refund every funded escrow of a competition that will never kick off.
    ///
    /// Runs from the cleanup queue, so a refund that cannot finish now is retried later: a
    /// provider that is down, an escrow whose locktime has not passed, or a payment that failed.
    pub(super) async fn refund_ark_escrows(&self, competition_id: Uuid) {
        let Some(ark) = self.ark() else {
            return;
        };
        let escrows = match self
            .competition_store
            .funded_ark_escrows(competition_id)
            .await
        {
            Ok(escrows) => escrows,
            Err(e) => {
                error!("Failed to list the escrows of competition {competition_id}: {e}");
                return;
            }
        };
        if escrows.is_empty() {
            return;
        }
        let entries = match self.refundable_entries(competition_id).await {
            Ok(entries) => entries,
            Err(e) => {
                error!("Cannot refund competition {competition_id}: {e}");
                return;
            }
        };
        for escrow in escrows {
            let ticket_id = escrow.ticket_id;
            let Some(entry) = entries.iter().find(|entry| entry.ticket_id == ticket_id) else {
                warn!("Ticket {ticket_id} has an escrow but no paid entry to refund");
                continue;
            };
            if let Err(e) = self
                .refund_ark_escrow(ark, competition_id, escrow, entry)
                .await
            {
                warn!("Cannot refund the escrow of ticket {ticket_id} yet: {e}");
            }
        }
    }

    /// The paid entries of a competition, whose tickets hold the escrows to refund.
    async fn refundable_entries(&self, competition_id: Uuid) -> Result<Vec<UserEntry>, Error> {
        Ok(self
            .competition_store
            .get_competition_entries(competition_id, vec![EntryStatus::Paid])
            .await?)
    }

    async fn refund_ark_escrow(
        &self,
        ark: &super::Arkade,
        competition_id: Uuid,
        escrow: TicketArkEscrow,
        entry: &UserEntry,
    ) -> Result<(), Error> {
        let ticket_id = escrow.ticket_id;
        let refund = self
            .competition_store
            .ticket_ark_refund(ticket_id)
            .await
            .map_err(|e| anyhow!("Cannot read the refund of ticket {ticket_id}: {e}"))?;
        if refund
            .as_ref()
            .is_some_and(|refund| refund.state == ArkRefundState::Settled)
        {
            return Ok(());
        }

        let (escrow_script, outpoint, sats) = self.refundable(&escrow)?;
        let refund = match refund {
            Some(refund) => refund,
            None => {
                self.mint_refund(ark, &escrow, entry, &escrow_script, sats)
                    .await?
            }
        };
        let swap = self.refund_swap(ark, &refund).await?;

        if refund.state == ArkRefundState::Submitting
            && !self.resume_refund(ark, &escrow, &refund).await?
        {
            // Arkade never took it, so it is built and signed again.
            self.submit_refund(
                ark,
                competition_id,
                &escrow,
                &escrow_script,
                entry,
                &swap,
                outpoint,
                sats,
                &refund,
            )
            .await?;
        } else if refund.state == ArkRefundState::Minted {
            self.submit_refund(
                ark,
                competition_id,
                &escrow,
                &escrow_script,
                entry,
                &swap,
                outpoint,
                sats,
                &refund,
            )
            .await?;
        }
        if matches!(
            refund.state,
            ArkRefundState::Minted | ArkRefundState::Submitting | ArkRefundState::Submitted
        ) {
            self.pay_refund(ark, &refund, sats).await?;
        }
        info!("Refunded the escrow of ticket {ticket_id}");
        Ok(())
    }

    /// The escrow's funded VTXO, once its refund leaf has opened.
    fn refundable(&self, escrow: &TicketArkEscrow) -> Result<(EntryEscrow, OutPoint, u64), Error> {
        let tap_tree = hex::decode(&escrow.escrow_tap_tree)
            .map_err(|e| anyhow!("The escrow's tap tree is not hex: {e}"))?;
        let entry = EntryEscrow::from_vtxo_script(
            &VtxoScript::decode_tap_tree(&tap_tree)
                .map_err(|e| anyhow!("The escrow's tap tree is invalid: {e}"))?,
        )
        .map_err(|e| anyhow!("The escrow's leaves are not an entry escrow: {e}"))?;
        let LockTime::Seconds(open_at) = entry.terms().refund_locktime else {
            return Err(anyhow!("The escrow's refund locktime is not a timestamp").into());
        };
        let now = OffsetDateTime::now_utc().unix_timestamp();
        if now < i64::from(open_at.to_consensus_u32()) {
            return Err(anyhow!("its refund leaf opens at {}", open_at.to_consensus_u32()).into());
        }
        let outpoint = escrow
            .vtxo_outpoint
            .as_deref()
            .context("the escrow has no funded VTXO")?
            .parse()
            .map_err(|e| anyhow!("The escrow's VTXO outpoint is invalid: {e}"))?;
        let sats = escrow
            .vtxo_sats
            .context("the escrow has no recorded value")?;
        Ok((entry, outpoint, sats))
    }

    /// Resolve the player's Lightning Address, and mint the swap that pays it.
    async fn mint_refund(
        &self,
        ark: &super::Arkade,
        escrow: &TicketArkEscrow,
        entry: &UserEntry,
        escrow_script: &EntryEscrow,
        sats: u64,
    ) -> Result<TicketArkRefund, Error> {
        let policy = self
            .competition_store
            .entry_payout_policy(entry.id)
            .await
            .map_err(|e| anyhow!("Cannot read the entry's payout policy: {e}"))?
            .context("the entry has no payout policy")?;
        let policy: PayoutPolicy = serde_json::from_str(&policy)
            .map_err(|e| anyhow!("The entry's payout policy is invalid: {e}"))?;
        let address = policy
            .automatic_lightning_address
            .context("the player has no Lightning Address to refund")?
            .parse()
            .map_err(|e| anyhow!("The player's Lightning Address is invalid: {e}"))?;
        let fee_sats = ark.max_refund_fee_sats.min(sats.saturating_sub(1));
        let owed_msat = sats
            .checked_sub(fee_sats)
            .filter(|owed| *owed > 0)
            .and_then(|owed| owed.checked_mul(1000))
            .context("the refund fee leaves the player nothing")?;
        let request = self
            .lnurl
            .resolve(&address)
            .await
            .map_err(|e| anyhow!("Cannot resolve the player's Lightning Address: {e}"))?;
        let invoice = self
            .lnurl
            .request_invoice(&request, owed_msat)
            .await
            .map_err(|e| anyhow!("Cannot get an invoice for the player: {e}"))?;
        let payment_hash = hex::encode(invoice.payment_hash().as_ref() as &[u8]);
        let deadline = u32::try_from(
            OffsetDateTime::now_utc().unix_timestamp() + SWAP_DEADLINE.as_secs() as i64,
        )
        .map_err(|_| anyhow!("The clock is out of range"))?;
        let minted = ark
            .swaps
            .mint_refund(
                &payment_hash,
                sats,
                &escrow_script.terms().player.to_string(),
                deadline,
            )
            .await
            .map_err(|e| anyhow!("ark-swapd cannot mint the refund's swap: {e}"))?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let refund = TicketArkRefund {
            ticket_id: escrow.ticket_id,
            refund_id: minted.id,
            invoice: invoice.to_string(),
            payment_hash,
            fee_sats,
            state: ArkRefundState::Minted,
            ark_txid: None,
            checkpoint_psbt: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        self.competition_store
            .store_ticket_ark_refund(refund.clone())
            .await?;
        debug!(
            "Minted swap {} to refund the escrow of ticket {}",
            minted.id, escrow.ticket_id
        );
        Ok(refund)
    }

    /// The swap a refund pays, as `ark-swapd` minted it.
    async fn refund_swap(
        &self,
        ark: &super::Arkade,
        refund: &TicketArkRefund,
    ) -> Result<RefundSwap, Error> {
        let minted = ark
            .swaps
            .refund(refund.refund_id)
            .await
            .map_err(|e| anyhow!("ark-swapd cannot recall the refund's swap: {e}"))?;
        let tap_tree = hex::decode(&minted.swap_tap_tree)
            .map_err(|e| anyhow!("The swap's tap tree is not hex: {e}"))?;
        RefundSwap::from_vtxo_script(
            &VtxoScript::decode_tap_tree(&tap_tree)
                .map_err(|e| anyhow!("The swap's tap tree is invalid: {e}"))?,
        )
        .map_err(|e| anyhow!("The swap's leaves are not a refund swap: {e}").into())
    }

    /// Spend the escrow into its swap, with Keymeld signing as the player.
    #[allow(clippy::too_many_arguments)]
    async fn submit_refund(
        &self,
        ark: &super::Arkade,
        competition_id: Uuid,
        escrow: &TicketArkEscrow,
        escrow_script: &EntryEscrow,
        entry: &UserEntry,
        swap: &RefundSwap,
        outpoint: OutPoint,
        sats: u64,
        refund: &TicketArkRefund,
    ) -> Result<(), Error> {
        let stored = self
            .competition_store
            .get_keymeld_session(competition_id)
            .await
            .map_err(|e| anyhow!("Cannot load the competition's Keymeld session: {e}"))?
            .context("the competition has no Keymeld session")?;
        let session = self.restore_keymeld_session(&stored)?;
        let user = keymeld_sdk::UserId::from(entry.ticket_id);
        let built = build_refund(
            ark.server.info(),
            escrow_script,
            outpoint,
            Amount::from_sat(sats),
            swap,
        )
        .map_err(|e| anyhow!("Cannot build the refund: {e}"))?;
        let spend = |purpose| ArkEscrowSpend::Refund {
            purpose,
            ark_psbt: psbt_hex(&built.ark),
            checkpoint_psbt: psbt_hex(&built.checkpoint),
            swap_tap_tree: hex::encode(swap.vtxo_script().encode_tap_tree()),
        };

        // The server co-signs between the two signatures, so the Ark transaction goes first.
        let mut ark_tx = built.ark.clone();
        let signature = self
            .keymeld
            .sign_ark_refund(
                &session,
                user.clone(),
                spend(RefundPurpose::ArkTransaction),
                refund.invoice.clone(),
                refund.fee_sats,
            )
            .await
            .map_err(|e| anyhow!("Keymeld will not sign the refund: {e}"))?;
        coordinator_ark::sign_refund_ark_tx(&mut ark_tx, escrow_script.terms().player, signature)
            .map_err(|e| anyhow!("Cannot place the refund's signature: {e}"))?;
        let submitted = ark
            .transport
            .submit_offchain(ark_tx, vec![built.checkpoint.clone()])
            .await
            .map_err(|e| anyhow!("Arkade will not take the refund: {e}"))?;

        let mut checkpoint = submitted
            .checkpoints
            .first()
            .context("Arkade returned no checkpoint for the refund")?
            .clone();
        let signature = self
            .keymeld
            .sign_ark_refund(
                &session,
                user,
                spend(RefundPurpose::Checkpoint),
                refund.invoice.clone(),
                refund.fee_sats,
            )
            .await
            .map_err(|e| anyhow!("Keymeld will not sign the refund's checkpoint: {e}"))?;
        coordinator_ark::sign_refund_checkpoint(
            &mut checkpoint,
            escrow_script.terms().player,
            signature,
        )
        .map_err(|e| anyhow!("Cannot place the checkpoint's signature: {e}"))?;
        let ark_txid = submitted.ark_tx.unsigned_tx.compute_txid();
        // Only the escrow's owner can sign this checkpoint, so it is kept before finalizing:
        // an interruption here finishes from it rather than signing the escrow again.
        self.competition_store
            .advance_ticket_ark_refund(
                escrow.ticket_id,
                ArkRefundState::Submitting,
                Some(ark_txid.to_string()),
                Some(psbt_hex(&checkpoint)),
                None,
            )
            .await?;
        self.finalize_refund(ark, escrow.ticket_id, ark_txid, checkpoint)
            .await?;
        info!(
            "Refunded the escrow of ticket {} into its swap in {ark_txid}",
            escrow.ticket_id
        );
        Ok(())
    }

    /// Finish a submitted refund, and record that its escrow is spent.
    async fn finalize_refund(
        &self,
        ark: &super::Arkade,
        ticket_id: Uuid,
        ark_txid: dlctix::bitcoin::Txid,
        checkpoint: dlctix::bitcoin::Psbt,
    ) -> Result<(), Error> {
        ark.transport
            .finalize_offchain(ark_txid, vec![checkpoint])
            .await
            .map_err(|e| anyhow!("Arkade will not finalize the refund: {e}"))?;
        self.competition_store
            .advance_ticket_ark_refund(ticket_id, ArkRefundState::Submitted, None, None, None)
            .await?;
        Ok(())
    }

    /// Carry on a refund that was interrupted while Arkade had it.
    ///
    /// The Ark transaction may or may not have reached the server. Its checkpoint is signed only
    /// by the escrow's owner, so if one was kept the refund finishes from it; otherwise the
    /// escrow decides, since a spent one means the refund is already through.
    async fn resume_refund(
        &self,
        ark: &super::Arkade,
        escrow: &TicketArkEscrow,
        refund: &TicketArkRefund,
    ) -> Result<bool, Error> {
        if let (Some(txid), Some(psbt)) = (&refund.ark_txid, &refund.checkpoint_psbt) {
            let txid = txid
                .parse()
                .map_err(|e| anyhow!("The refund's Arkade transaction id is invalid: {e}"))?;
            let checkpoint = hex::decode(psbt)
                .ok()
                .and_then(|bytes| dlctix::bitcoin::Psbt::deserialize(&bytes).ok())
                .context("the refund's kept checkpoint is invalid")?;
            self.finalize_refund(ark, escrow.ticket_id, txid, checkpoint)
                .await?;
            return Ok(true);
        }
        let spent = self
            .escrow_spent(ark, escrow)
            .await
            .map_err(|e| anyhow!("Cannot tell whether the escrow is already refunded: {e}"))?;
        if spent {
            warn!(
                "The escrow of ticket {} is spent, so its refund went through",
                escrow.ticket_id
            );
            self.competition_store
                .advance_ticket_ark_refund(
                    escrow.ticket_id,
                    ArkRefundState::Submitted,
                    None,
                    None,
                    None,
                )
                .await?;
        }
        Ok(spent)
    }

    /// Whether Arkade has already spent this escrow's VTXO.
    async fn escrow_spent(
        &self,
        ark: &super::Arkade,
        escrow: &TicketArkEscrow,
    ) -> Result<bool, Error> {
        let outpoint: OutPoint = escrow
            .vtxo_outpoint
            .as_deref()
            .context("the escrow has no funded VTXO")?
            .parse()
            .map_err(|e| anyhow!("The escrow's VTXO outpoint is invalid: {e}"))?;
        let tap_tree = hex::decode(&escrow.escrow_tap_tree)
            .map_err(|e| anyhow!("The escrow's tap tree is not hex: {e}"))?;
        let entry = EntryEscrow::from_vtxo_script(
            &VtxoScript::decode_tap_tree(&tap_tree)
                .map_err(|e| anyhow!("The escrow's tap tree is invalid: {e}"))?,
        )
        .map_err(|e| anyhow!("The escrow's leaves are not an entry escrow: {e}"))?;
        let address = entry
            .address(ark.server.hrp())
            .map_err(|e| anyhow!("The escrow has no Ark address: {e}"))?
            .encode();
        Ok(ark
            .transport
            .vtxos(vec![address])
            .await
            .map_err(|e| anyhow!("Arkade will not list the escrow's VTXOs: {e}"))?
            .into_iter()
            .any(|vtxo| vtxo.outpoint == outpoint && vtxo.is_spent))
    }

    /// Pay the player, and give `ark-swapd` the preimage that claims the swap.
    ///
    /// The payment is looked up rather than trusted to the send, so a refund resumed after a
    /// restart finds the proof of a payment already made instead of paying again.
    async fn pay_refund(
        &self,
        ark: &super::Arkade,
        refund: &TicketArkRefund,
        sats: u64,
    ) -> Result<(), Error> {
        let hash = crate::infra::lightning::extract_payment_hash_from_invoice(&refund.invoice)
            .map_err(|e| anyhow!("The refund's invoice has no payment hash: {e}"))?;
        let owed = sats.saturating_sub(refund.fee_sats);
        if !matches!(
            self.ln.lookup_payment(&hash).await.map(|paid| paid.status),
            Ok(PaymentStatus::Succeeded)
        ) {
            self.ln
                .send_payment(
                    refund.invoice.clone(),
                    owed,
                    REFUND_PAYMENT_TIMEOUT.as_secs(),
                    refund.fee_sats,
                )
                .await
                .map_err(|e| anyhow!("Cannot pay the player's refund: {e}"))?;
        }
        let settled = self
            .ln
            .lookup_payment(&hash)
            .await
            .map_err(|e| anyhow!("Cannot look up the refund's payment: {e}"))?;
        if settled.status != PaymentStatus::Succeeded {
            return Err(anyhow!("the refund's payment has not settled").into());
        }
        let preimage = settled
            .payment_preimage
            .context("LND returned no proof of the refund's payment")?;
        self.competition_store
            .advance_ticket_ark_refund(refund.ticket_id, ArkRefundState::Paid, None, None, None)
            .await?;
        let preimage: [u8; 32] = hex::decode(&preimage)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .context("the payment proof is not 32 bytes")?;
        ark.swaps
            .refund_paid(refund.refund_id, &preimage)
            .await
            .map_err(|e| anyhow!("ark-swapd cannot claim the refund's swap: {e}"))?;
        self.competition_store
            .advance_ticket_ark_refund(refund.ticket_id, ArkRefundState::Settled, None, None, None)
            .await?;
        Ok(())
    }
}
