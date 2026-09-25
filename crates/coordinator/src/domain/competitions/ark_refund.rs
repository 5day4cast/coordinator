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
//!
//! Keymeld signs with the entry key the player's browser sealed to its enclave with their entry.
//! Entries are registered with Keymeld when a competition fills, so a refund registers them first
//! for one that died before. For a competition that never filled, Keymeld is given only the
//! players who entered, and signs each refund with that player's own key; no key is formed from
//! the roster. Once Keymeld has a competition's roster it cannot change, so a ticket counted
//! after that, and a ticket that was paid but never used for an entry, need an operator
//! (`docs/ops/stuck-escrow-check.md`).

use std::time::Duration;

use anyhow::{anyhow, Context};
use coordinator_ark::{build_refund, RefundTransactions};
use coordinator_ark_escrow::{EntryEscrow, RefundSwap, VtxoScript};
use coordinator_escrow::ark::{psbt_hex, ArkEscrowSpend, RefundPurpose, MIN_REFUND_DEADLINE_SECS};
use dlctix::bitcoin::absolute::LockTime;
use dlctix::bitcoin::{Amount, OutPoint};
use log::{debug, info, warn};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ArkRefundState, Coordinator, TicketArkEscrow, TicketArkRefund};
use crate::domain::competitions::EntryStatus;
use crate::domain::PaymentStatus;
use crate::domain::{Error, UserEntry};
use crate::infra::keymeld::{DlcKeygenSession, KeymeldError};
use coordinator_escrow::authorization::PayoutPolicy;
use std::collections::BTreeSet;

/// How long a refund's swap waits for its payment before the player may take it back. The
/// verifier bounds this too, so a swap minted with anything wilder is refused.
const SWAP_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// A minted refund is replaced once its swap's deadline is this close beyond the soonest
/// deadline the verifier signs for, or its invoice this close to expiring: signing and paying
/// take a moment. Nothing was signed or paid for it yet.
const STALE_MARGIN: Duration = Duration::from_secs(5 * 60);
const INVOICE_MARGIN: Duration = Duration::from_secs(2 * 60);

/// A refund that keeps failing is minted again at most this often, so a player's Lightning
/// Address provider is not asked for a new invoice on every cleanup pass.
const REMINT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How long a refund's payment may take before this gives up and retries later.
const REFUND_PAYMENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Reports about a ticket's refund, by ticket.
const REFUND_REPORTS: &str = "escrow refund";

/// What a player is told about their refund.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TicketRefund {
    /// minted, submitting, submitted, paid, or settled.
    pub state: String,
    /// What the player is paid, after the swap service's fee.
    pub paid_sats: u64,
    /// The Arkade transaction that moved the escrow into the swap.
    pub ark_txid: Option<String>,
    /// The invoice the refund pays, from the player's Lightning Address, and its payment hash:
    /// what the player's wallet shows the refund as.
    pub invoice: String,
    pub payment_hash: String,
    /// UNIX seconds.
    pub updated_at: i64,
}

/// A funded escrow whose refund leaf is open.
struct Refundable {
    escrow: TicketArkEscrow,
    script: EntryEscrow,
    outpoint: OutPoint,
    sats: u64,
    refund: Option<TicketArkRefund>,
}

/// Where Arkade says an escrow's VTXO went.
enum EscrowSpend {
    Unspent,
    /// Into this refund's swap, by a submission whose checkpoint was never kept, so this
    /// process cannot tell whether it was finalized.
    ByRefund(dlctix::bitcoin::Txid),
    /// By something other than this refund.
    Elsewhere,
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
            invoice: refund.invoice,
            payment_hash: refund.payment_hash,
            updated_at: refund.updated_at,
        }))
    }

    /// Refund every funded escrow of a competition that will never kick off.
    ///
    /// Runs from the cleanup queue, so a refund that cannot finish now is retried later: a
    /// provider that is down, an escrow whose locktime has not passed, or a payment that failed.
    /// Each lasting problem is logged once per ticket.
    pub(super) async fn refund_ark_escrows(&self, competition_id: Uuid) {
        let Some(ark) = self.ark() else {
            return;
        };
        let escrows = match self
            .competition_store
            .refundable_ark_escrows(competition_id)
            .await
        {
            Ok(escrows) => escrows,
            Err(e) => {
                warn!("Cannot list the escrows of competition {competition_id} to refund: {e}");
                return;
            }
        };
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut open = Vec::new();
        for escrow in escrows {
            let ticket_id = escrow.ticket_id;
            match self.refundable(escrow, now).await {
                Ok(Some(refundable)) => open.push(refundable),
                Ok(None) => {}
                Err(e) => self.report_refund(ticket_id, &e.to_string()),
            }
        }
        if open.is_empty() {
            return;
        }
        let entries = match self.refundable_entries(competition_id).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Cannot read the entries of competition {competition_id} to refund: {e}");
                return;
            }
        };
        let signing = open.iter().any(needs_signing);
        let (session, late) = if signing {
            match self.refund_session(competition_id, &entries).await {
                Ok((session, late)) => (Some(session), late),
                Err(e) => {
                    if self
                        .reported
                        .is_new(REFUND_REPORTS, competition_id, &e.to_string())
                    {
                        warn!("Cannot sign the refunds of competition {competition_id}: {e}");
                    }
                    (None, BTreeSet::new())
                }
            }
        } else {
            (None, BTreeSet::new())
        };
        for refundable in open {
            let ticket_id = refundable.escrow.ticket_id;
            let Some(entry) = entries.iter().find(|entry| entry.ticket_id == ticket_id) else {
                self.report_refund(
                    ticket_id,
                    &format!(
                        "its escrow {} holds {} sats, but the ticket was never used for an \
                         entry, so Keymeld has no entry key to sign its refund with; it needs \
                         an operator (docs/ops/stuck-escrow-check.md)",
                        refundable.outpoint, refundable.sats
                    ),
                );
                continue;
            };
            // Without Keymeld nothing can be signed, and a refund is not minted, asking the
            // player's provider for an invoice, until it can be. The reason was logged above.
            if needs_signing(&refundable) && session.is_none() {
                continue;
            }
            if needs_signing(&refundable) && late.contains(&ticket_id) {
                self.report_refund(
                    ticket_id,
                    &format!(
                        "its escrow {} holds {} sats, but its ticket was counted after Keymeld \
                         was given the competition's roster, which cannot change, so Keymeld \
                         cannot sign its refund; it needs an operator \
                         (docs/ops/stuck-escrow-check.md)",
                        refundable.outpoint, refundable.sats
                    ),
                );
                continue;
            }
            match self
                .refund_ark_escrow(ark, session.as_ref(), refundable, entry)
                .await
            {
                Ok(()) => {
                    self.reported.clear(REFUND_REPORTS, ticket_id);
                    info!("Refunded the escrow of ticket {ticket_id}");
                }
                Err(e) => self.report_refund(ticket_id, &e.to_string()),
            }
        }
    }

    /// Log why a ticket's refund cannot finish yet, once until the reason changes.
    fn report_refund(&self, ticket_id: Uuid, problem: &str) {
        if self.reported.is_new(REFUND_REPORTS, ticket_id, problem) {
            warn!("Cannot refund the escrow of ticket {ticket_id} yet: {problem}");
        } else {
            debug!("The escrow of ticket {ticket_id} is still not refunded: {problem}");
        }
    }

    /// The paid entries of a competition, whose tickets hold the escrows to refund.
    async fn refundable_entries(&self, competition_id: Uuid) -> Result<Vec<UserEntry>, Error> {
        Ok(self
            .competition_store
            .get_competition_entries(competition_id, vec![EntryStatus::Paid])
            .await?)
    }

    /// The competition's Keymeld session, with every paid entry registered in it, and the
    /// tickets that were counted too late to join it.
    ///
    /// Entries are registered when a competition's contract is built, so one that died before
    /// registered none. Registering is idempotent, so this repeats safely on every pass.
    ///
    /// Keymeld is sent the roster with the first refund it signs, and it cannot change after
    /// that. So no refund is signed while a ticket's invoice can still be paid: that player
    /// would be left out. A ticket counted after the roster was sent is returned, to be left for
    /// an operator; any other failure to register stops every refund until the next pass, so a
    /// passing fault cannot leave a player out.
    async fn refund_session(
        &self,
        competition_id: Uuid,
        entries: &[UserEntry],
    ) -> Result<(DlcKeygenSession, BTreeSet<Uuid>), Error> {
        let payable = self
            .competition_store
            .payable_ticket_count(competition_id)
            .await
            .map_err(|e| anyhow!("Cannot read the competition's unpaid tickets: {e}"))?;
        if payable > 0 {
            return Err(anyhow!(
                "{payable} of its tickets can still be paid, and Keymeld's roster cannot change \
                 once it signs a refund, so the refunds wait for those invoices to expire"
            )
            .into());
        }
        let stored = self
            .competition_store
            .get_keymeld_session(competition_id)
            .await
            .map_err(|e| anyhow!("Cannot load the competition's Keymeld session: {e}"))?
            .context("the competition has no Keymeld session")?;
        let session = self.restore_keymeld_session(&stored)?;
        let mut late = BTreeSet::new();
        for entry in entries {
            let registration = self.keymeld_registration(entry).await.map_err(|e| {
                anyhow!("entry {} cannot be registered with Keymeld: {e}", entry.id)
            })?;
            match self
                .keymeld
                .register_participant(
                    &session,
                    keymeld_sdk::UserId::from(entry.ticket_id),
                    &registration,
                )
                .await
            {
                Ok(()) => {}
                Err(KeymeldError::RosterFixed(_)) => {
                    late.insert(entry.ticket_id);
                }
                Err(e) => {
                    return Err(anyhow!("Keymeld will not register entry {}: {e}", entry.id).into())
                }
            }
        }
        Ok((session, late))
    }

    /// Carry one escrow's refund as far as it can go now.
    async fn refund_ark_escrow(
        &self,
        ark: &super::Arkade,
        session: Option<&DlcKeygenSession>,
        refundable: Refundable,
        entry: &UserEntry,
    ) -> Result<(), Error> {
        let Refundable {
            escrow,
            script,
            outpoint,
            sats,
            refund,
        } = refundable;
        let mut refund = match refund {
            Some(refund) => refund,
            None => {
                // Nothing was ever submitted for this escrow, so it must still be unspent.
                if self.listed_escrow(ark, &escrow, outpoint).await?.is_spent {
                    return Err(anyhow!(
                        "its escrow {outpoint} was spent by something other than a refund; \
                         it needs an operator"
                    )
                    .into());
                }
                let refund = self.mint_refund(ark, &escrow, entry, &script, sats).await?;
                self.competition_store
                    .store_ticket_ark_refund(refund.clone())
                    .await?;
                refund
            }
        };

        match refund.state {
            ArkRefundState::Minted => {
                let mut swap = self.refund_swap(ark, &refund).await?;
                let mut built = build(ark, &script, outpoint, sats, &swap)?;
                let listed = self.listed_escrow(ark, &escrow, outpoint).await?;
                match escrow_spend(&listed, &built) {
                    EscrowSpend::Unspent => {
                        if self.is_stale(&refund, &swap)? {
                            refund = self
                                .remint_refund(ark, &escrow, entry, &script, sats, &refund)
                                .await?;
                            swap = self.refund_swap(ark, &refund).await?;
                            built = build(ark, &script, outpoint, sats, &swap)?;
                        }
                        let session = session.context("the refund needs Keymeld, see above")?;
                        self.submit_refund(ark, session, &escrow, &script, &swap, &built, &refund)
                            .await?;
                    }
                    // Either way the player is not paid: paying for a swap the service may
                    // never be able to claim could pay them twice.
                    EscrowSpend::ByRefund(ark_txid) => {
                        return Err(anyhow!(
                            "its escrow {outpoint} went into its refund in {ark_txid}, but \
                             the refund's checkpoint was not kept, so whether Arkade \
                             finalized it is unknown; it needs an operator"
                        )
                        .into());
                    }
                    EscrowSpend::Elsewhere => {
                        return Err(anyhow!(
                            "its escrow {outpoint} was spent by something other than its \
                             refund; it needs an operator"
                        )
                        .into());
                    }
                }
            }
            ArkRefundState::Submitting => self.resume_refund(ark, &escrow, &refund).await?,
            ArkRefundState::Submitted | ArkRefundState::Paid | ArkRefundState::Settled => {}
        }
        if refund.state != ArkRefundState::Settled {
            self.pay_refund(ark, &refund, sats).await?;
        }
        Ok(())
    }

    /// The escrow's funded VTXO, once its refund leaf has opened; `None` until then.
    async fn refundable(
        &self,
        escrow: TicketArkEscrow,
        now: i64,
    ) -> Result<Option<Refundable>, Error> {
        let script = entry_escrow(&escrow)?;
        let LockTime::Seconds(open_at) = script.terms().refund_locktime else {
            return Err(anyhow!("The escrow's refund locktime is not a timestamp").into());
        };
        let open_at = i64::from(open_at.to_consensus_u32());
        if now < open_at {
            debug!(
                "The refund leaf of ticket {}'s escrow opens at {open_at}",
                escrow.ticket_id
            );
            return Ok(None);
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
        let refund = self
            .competition_store
            .ticket_ark_refund(escrow.ticket_id)
            .await
            .map_err(|e| anyhow!("Cannot read the refund: {e}"))?;
        Ok(Some(Refundable {
            escrow,
            script,
            outpoint,
            sats,
            refund,
        }))
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
            .context("the player gave no Lightning Address to refund to")?
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
        debug!(
            "Minted swap {} to refund the escrow of ticket {}",
            minted.id, escrow.ticket_id
        );
        Ok(TicketArkRefund {
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
        })
    }

    /// Whether a minted refund can no longer be signed: the verifier signs only for a swap
    /// whose deadline is at least `MIN_REFUND_DEADLINE_SECS` away, and an unexpired invoice.
    fn is_stale(&self, refund: &TicketArkRefund, swap: &RefundSwap) -> Result<bool, Error> {
        let now = OffsetDateTime::now_utc().unix_timestamp().max(0) as u64;
        let soonest_deadline = now + u64::from(MIN_REFUND_DEADLINE_SECS) + STALE_MARGIN.as_secs();
        let LockTime::Seconds(deadline) = swap.terms().deadline else {
            return Err(anyhow!("The refund's swap deadline is not a timestamp").into());
        };
        let invoice: lightning_invoice::Bolt11Invoice = refund
            .invoice
            .parse()
            .map_err(|e| anyhow!("The refund's invoice is invalid: {e}"))?;
        Ok(u64::from(deadline.to_consensus_u32()) < soonest_deadline
            || invoice.would_expire(Duration::from_secs(now + INVOICE_MARGIN.as_secs())))
    }

    /// Replace a stale minted refund with a new swap and invoice.
    ///
    /// Nothing was signed or paid for the stale one: a minted refund's escrow is unspent. Its
    /// swap at `ark-swapd` retires by itself at its deadline.
    async fn remint_refund(
        &self,
        ark: &super::Arkade,
        escrow: &TicketArkEscrow,
        entry: &UserEntry,
        escrow_script: &EntryEscrow,
        sats: u64,
        stale: &TicketArkRefund,
    ) -> Result<TicketArkRefund, Error> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        if now < stale.created_at + REMINT_INTERVAL.as_secs() as i64 {
            return Err(anyhow!(
                "its minted refund expired before it could be signed; it is minted again \
                 from {}",
                stale.created_at + REMINT_INTERVAL.as_secs() as i64
            )
            .into());
        }
        let fresh = self
            .mint_refund(ark, escrow, entry, escrow_script, sats)
            .await?;
        if !self
            .competition_store
            .replace_minted_ticket_ark_refund(stale.refund_id, fresh.clone())
            .await?
        {
            return Err(anyhow!("its refund moved on while it was minted again").into());
        }
        info!(
            "Minted the refund of ticket {} again: swap {} replaces {}, which expired unsigned",
            escrow.ticket_id, fresh.refund_id, stale.refund_id
        );
        Ok(fresh)
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
        session: &DlcKeygenSession,
        escrow: &TicketArkEscrow,
        escrow_script: &EntryEscrow,
        swap: &RefundSwap,
        built: &RefundTransactions,
        refund: &TicketArkRefund,
    ) -> Result<(), Error> {
        let user = keymeld_sdk::UserId::from(escrow.ticket_id);
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
                session,
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
                session,
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
    /// Its checkpoint is signed only by the escrow's owner and was kept with the Ark
    /// transaction's id, so the refund finishes from it rather than signing again.
    async fn resume_refund(
        &self,
        ark: &super::Arkade,
        escrow: &TicketArkEscrow,
        refund: &TicketArkRefund,
    ) -> Result<(), Error> {
        let (Some(txid), Some(psbt)) = (&refund.ark_txid, &refund.checkpoint_psbt) else {
            return Err(anyhow!("its refund was submitted without keeping its checkpoint").into());
        };
        let txid = txid
            .parse()
            .map_err(|e| anyhow!("The refund's Arkade transaction id is invalid: {e}"))?;
        let checkpoint = hex::decode(psbt)
            .ok()
            .and_then(|bytes| dlctix::bitcoin::Psbt::deserialize(&bytes).ok())
            .context("the refund's kept checkpoint is invalid")?;
        self.finalize_refund(ark, escrow.ticket_id, txid, checkpoint)
            .await
    }

    /// The escrow's VTXO, as Arkade lists it.
    async fn listed_escrow(
        &self,
        ark: &super::Arkade,
        escrow: &TicketArkEscrow,
        outpoint: OutPoint,
    ) -> Result<coordinator_ark::VirtualTxOutPoint, Error> {
        ark.transport
            .vtxos(vec![escrow.escrow_address.clone()])
            .await
            .map_err(|e| anyhow!("Arkade will not list the escrow's VTXOs: {e}"))?
            .into_iter()
            .find(|vtxo| vtxo.outpoint == outpoint)
            .ok_or_else(|| anyhow!("Arkade does not list its escrow {outpoint}").into())
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

/// Whether an escrow's refund still has to be signed: it was never minted, or was minted but
/// never submitted.
fn needs_signing(refundable: &Refundable) -> bool {
    refundable
        .refund
        .as_ref()
        .is_none_or(|refund| refund.state == ArkRefundState::Minted)
}

/// An escrow's script, from the tap tree recorded with it.
fn entry_escrow(escrow: &TicketArkEscrow) -> Result<EntryEscrow, Error> {
    let tap_tree = hex::decode(&escrow.escrow_tap_tree)
        .map_err(|e| anyhow!("The escrow's tap tree is not hex: {e}"))?;
    Ok(EntryEscrow::from_vtxo_script(
        &VtxoScript::decode_tap_tree(&tap_tree)
            .map_err(|e| anyhow!("The escrow's tap tree is invalid: {e}"))?,
    )
    .map_err(|e| anyhow!("The escrow's leaves are not an entry escrow: {e}"))?)
}

/// Where the escrow's VTXO went: nowhere yet, into `refund` (by its checkpoint or Ark
/// transaction), or somewhere else.
fn escrow_spend(
    vtxo: &coordinator_ark::VirtualTxOutPoint,
    refund: &RefundTransactions,
) -> EscrowSpend {
    if !vtxo.is_spent {
        return EscrowSpend::Unspent;
    }
    let ark_txid = refund.ark.unsigned_tx.compute_txid();
    let checkpoint_txid = refund.checkpoint.unsigned_tx.compute_txid();
    if vtxo.ark_txid == Some(ark_txid) || vtxo.spent_by == Some(checkpoint_txid) {
        EscrowSpend::ByRefund(ark_txid)
    } else {
        EscrowSpend::Elsewhere
    }
}

/// The transactions that refund the escrow into `swap`. They depend only on their inputs, so
/// they are the same each time a refund is resumed.
fn build(
    ark: &super::Arkade,
    script: &EntryEscrow,
    outpoint: OutPoint,
    sats: u64,
    swap: &RefundSwap,
) -> Result<RefundTransactions, Error> {
    Ok(build_refund(
        ark.server.info(),
        script,
        outpoint,
        Amount::from_sat(sats),
        swap,
    )
    .map_err(|e| anyhow!("Cannot build the refund: {e}"))?)
}
