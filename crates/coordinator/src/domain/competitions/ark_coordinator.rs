//! The Coordinator's Arkade path: escrow swaps at entry, and a pool kickoff in an Arkade batch.
//!
//! A ticket's buy-in waits in an escrow VTXO whose player key is the entry key.
//! `ark-swapd` pays the escrow from its own Ark wallet while holding the player's Lightning
//! payment, and settles with the ticket preimage, so paying still reveals it to the player.
//! See `ark_kickoff.rs` and `docs/QUEUED_COMPETITIONS.md`.

use super::*;
use crate::domain::competitions::{
    admission, ArkCommitment, Arkade, ArkadeHealth, KeymeldArkPool, TicketArkEscrow, TicketPrice,
};
use coordinator_escrow::authorization::ArkEscrowPolicy;

impl Coordinator {
    /// Fund new competitions from Arkade escrows.
    pub fn with_ark(mut self, ark: Option<Arkade>) -> Result<Self, anyhow::Error> {
        if ark.is_some() && !self.automatic_payouts {
            return Err(anyhow!(
                "Arkade funding needs Keymeld with automatic payouts"
            ));
        }
        self.ark = ark.map(Arc::new);
        Ok(self)
    }

    /// How long a failure of the Arkade server pauses entries when nothing succeeds after it.
    pub fn with_arkade_outage_secs(mut self, outage_secs: u64) -> Self {
        self.arkade_health = Arc::new(ArkadeHealth::new(outage_secs));
        self
    }

    pub fn arkade_health(&self) -> Arc<ArkadeHealth> {
        self.arkade_health.clone()
    }

    /// Learn from ark-swapd how its boards last went, since the Arkade server failing those is
    /// an outage too while the coordinator runs no batch step of its own, and export its
    /// balances as metrics. A failed or slow read is ignored.
    pub async fn read_ark_swap_boards(&self) {
        let Some(ark) = self.ark() else {
            return;
        };
        let wallet = match tokio::time::timeout(ARK_SWAP_WALLET_TIMEOUT, ark.swaps.wallet()).await {
            Ok(Ok(wallet)) => wallet,
            Ok(Err(error)) => {
                debug!("Could not read ark-swapd's boards: {error:#}");
                return;
            }
            Err(_) => {
                debug!(
                    "Could not read ark-swapd's boards: no answer within {}s",
                    ARK_SWAP_WALLET_TIMEOUT.as_secs()
                );
                return;
            }
        };
        crate::metrics::record_ark_wallet(&wallet, OffsetDateTime::now_utc());
        let at = |seconds: i64| OffsetDateTime::from_unix_timestamp(seconds).ok();
        let failure = wallet
            .last_board_failure
            .and_then(|failure| Some((at(failure.at)?, format!("ark-swapd {}", failure.message))));
        self.arkade_health.observed(
            failure
                .as_ref()
                .map(|(at, message)| (*at, message.as_str())),
            wallet.last_board_success_at.and_then(at),
            OffsetDateTime::now_utc(),
        );
    }

    pub fn ark(&self) -> Option<&Arkade> {
        self.ark.as_deref()
    }

    /// The Arkade services, if `competition_id` is funded from Arkade escrows.
    pub(super) async fn ark_for(&self, competition_id: Uuid) -> Result<Option<&Arkade>, Error> {
        if !self.competition_store.is_ark_funded(competition_id).await? {
            return Ok(None);
        }
        self.ark().map(Some).ok_or_else(|| {
            Error::BadRequest(
                "This competition is funded from Arkade, which is not configured".into(),
            )
        })
    }

    /// Fix the ticket's escrow for its entry key, and return the consent the player signs with
    /// their payout policy. A repeated request for the same reservation gets the same escrow.
    pub(super) async fn ticket_ark_escrow_policy(
        &self,
        competition: &Competition,
        ticket: &Ticket,
        entry_pubkey: &BitcoinPublicKey,
    ) -> Result<Option<ArkEscrowPolicy>, Error> {
        let Some(ark) = self.ark_for(competition.id).await? else {
            return Ok(None);
        };
        let escrow_tap_tree = match self
            .competition_store
            .ticket_ark_escrow(ticket.id, &ticket.hash)
            .await?
        {
            Some(existing) => existing.escrow_tap_tree,
            None => {
                let now = OffsetDateTime::now_utc().unix_timestamp();
                let refund_at = escrow_refund_at(
                    competition.event_submission.start_observation_date,
                    ark.refund_after_start_secs,
                    competition
                        .event_announcement
                        .as_ref()
                        .and_then(|event| event.expiry),
                );
                let seconds = |value: i64| {
                    u32::try_from(value)
                        .map_err(|_| Error::BadRequest("Timestamp out of range".into()))
                };
                let terms = ark
                    .server
                    .escrow_terms(
                        entry_pubkey.inner.x_only_public_key().0,
                        dlctix::convert_point(self.public_key),
                        seconds(refund_at)?,
                        seconds(now)?,
                    )
                    .map_err(|e| {
                        Error::BadRequest(format!("Cannot build the entry escrow: {e}"))
                    })?;
                let escrow = ark.server.entry_escrow(terms).map_err(|e| {
                    Error::BadRequest(format!("Cannot build the entry escrow: {e}"))
                })?;
                let address = escrow
                    .address(ark.server.hrp())
                    .map_err(|e| Error::BadRequest(e.to_string()))?
                    .encode();
                self.competition_store
                    .store_ticket_ark_escrow(
                        ticket.id,
                        ticket.hash.clone(),
                        hex::encode(escrow.vtxo_script().encode_tap_tree()),
                        address,
                    )
                    .await?;
                // Another request may have fixed the escrow first.
                self.competition_store
                    .ticket_ark_escrow(ticket.id, &ticket.hash)
                    .await?
                    .ok_or_else(|| Error::BadRequest("Ticket reservation changed".into()))?
                    .escrow_tap_tree
            }
        };
        // The escrow holds the ticket's price: the stake, and the coordinator and network fees.
        let price = self.ticket_price(competition, ticket).await?;
        Ok(Some(ArkEscrowPolicy {
            escrow_tap_tree,
            max_fee_sats: price.escrow_fee_sats(),
            max_refund_fee_sats: ark.max_refund_fee_sats,
            // A refund's transactions pass through this server's checkpoint outputs, so the
            // player consents to the script that makes them.
            checkpoint_exit_script: hex::encode(ark.server.info().checkpoint_tapscript.as_bytes()),
        }))
    }

    /// The most a pool's kickoff may pay the coordinator: the verifier signs an escrow into a
    /// batch only if the fee output is at most that escrow's `max_fee_sats` times the escrows
    /// in it, so the lowest cap among the entries, times their number.
    pub(super) async fn pool_fee_cap(
        &self,
        entries: &[UserEntry],
    ) -> Result<Amount, anyhow::Error> {
        let mut lowest: Option<u64> = None;
        for entry in entries {
            let json = self
                .competition_store
                .entry_payout_policy(entry.id)
                .await?
                .ok_or_else(|| anyhow!("Entry {} has no payout policy", entry.id))?;
            let policy: coordinator_escrow::authorization::PayoutPolicy =
                serde_json::from_str(&json)?;
            let cap = policy
                .ark_escrow
                .ok_or_else(|| anyhow!("Entry {} consented to no Arkade escrow", entry.id))?
                .max_fee_sats;
            lowest = Some(lowest.map_or(cap, |lowest| lowest.min(cap)));
        }
        let players = entries.len() as u64;
        Ok(Amount::from_sat(
            lowest
                .unwrap_or(0)
                .checked_mul(players)
                .ok_or_else(|| anyhow!("Fee cap overflows"))?,
        ))
    }

    /// The swap invoice that pays the ticket's escrow, and when it expires.
    ///
    /// The invoice pays to the ticket's hash, so the player learns the ticket preimage by paying.
    pub(super) async fn ticket_ark_invoice(
        &self,
        ark: &Arkade,
        ticket: &Ticket,
        amount_sats: u64,
        deadline: OffsetDateTime,
    ) -> Result<(String, OffsetDateTime), Error> {
        let escrow: TicketArkEscrow = self
            .competition_store
            .ticket_ark_escrow(ticket.id, &ticket.hash)
            .await?
            .ok_or_else(|| Error::BadRequest("The ticket has no escrow yet".into()))?;
        let preimage = self
            .competition_store
            .ticket_preimage(ticket)
            .map_err(anyhow::Error::from)?;
        if !admission::before_deadline(Some(deadline)) {
            return Err(Error::BadRequest(admission::TICKETS_CLOSED.into()));
        }
        let swap = ark
            .swaps
            .create_swap(&escrow.escrow_address, amount_sats, &preimage)
            .await
            .map_err(|e| {
                error!(
                    "Failed to create the escrow swap for ticket {}: {e:#}",
                    ticket.id
                );
                swap_failure(&e)
            })?;
        if swap.payment_hash != ticket.hash {
            return Err(Error::BadRequest(
                "The swap invoice does not pay to the ticket's hash".into(),
            ));
        }
        self.competition_store
            .set_ticket_ark_swap(ticket.id, ticket.hash.clone(), swap.id)
            .await?;
        // The escrow subscription watches it from now, rather than from the next check of the
        // pending swaps.
        self.escrow_watch.watch(&escrow.escrow_address);
        let expires_at = OffsetDateTime::from_unix_timestamp(swap.expires_at)
            .map_err(|e| Error::BadRequest(e.to_string()))?;
        Ok((swap.invoice, expires_at))
    }

    /// Advance every pending escrow swap. A ticket is paid once its player's payment settled
    /// and Arkade lists the escrow VTXO holding the ticket's price.
    ///
    /// ark-swapd is asked about every swap on each check. Arkade is not asked per swap: what the
    /// escrow subscription reported is checked first, and the swaps it has not settled are
    /// looked up together in one listing, at most every thirty seconds.
    pub async fn check_ark_swaps(&self) -> Result<(), Error> {
        let Some(ark) = self.ark() else {
            return Ok(());
        };
        let pending_swaps = self.competition_store.pending_ark_swaps().await?;
        // Swaps made by another coordinator process are watched from here.
        self.escrow_watch.replace(
            pending_swaps
                .iter()
                .map(|pending| pending.escrow_address.as_str()),
        );
        let mut unverified = Vec::new();
        for pending in pending_swaps {
            let swap = match ark.swaps.swap(pending.swap_id).await {
                Ok(swap) => swap,
                Err(e) => {
                    self.report_swap(&pending, format!("cannot be read from ark-swapd: {e:#}"));
                    continue;
                }
            };
            if swap.state.player_paid() {
                // Arkade is asked only once ark-swapd names what it paid.
                if swap.escrow_vtxo.is_none() && swap.ark_txid.is_none() {
                    continue;
                }
                let seen = self.escrow_watch.seen(&pending.escrow_address);
                if !seen.is_empty() {
                    if self.settle_paid_swap(ark, &pending, &swap, &seen).await? {
                        continue;
                    }
                    // What the subscription reported is not enough, so the listing decides.
                    self.escrow_watch.clear_seen(&pending.escrow_address);
                }
                unverified.push((pending, swap));
            } else if swap.state.ended_unpaid() {
                if swap.state == crate::infra::ark_swap::SwapState::Unsettled
                    && self
                        .reported
                        .is_new(SWAP_REPORTS, pending.swap_id, "unsettled")
                {
                    error!(
                        "Escrow swap {} paid the escrow of ticket {}, but could not settle the \
                         player's invoice, so their payment went back to them. The ticket is \
                         released; the escrow holds ark-swapd's coins and needs an operator.",
                        swap.id, pending.ticket_id
                    );
                }
                let ticket = self.competition_store.get_ticket(pending.ticket_id).await?;
                self.competition_store
                    .clear_ticket_reservation(&ticket)
                    .await?;
                self.escrow_watch.forget(&pending.escrow_address);
                info!(
                    "Escrow swap for ticket {} ended unpaid; the reservation is released",
                    pending.ticket_id
                );
            }
        }
        if unverified.is_empty() || !self.escrow_watch.sweep_due() {
            return Ok(());
        }
        let addresses = unverified
            .iter()
            .map(|(pending, _)| pending.escrow_address.clone())
            .collect();
        let listed = match ark.transport.vtxos(addresses).await {
            Ok(listed) => listed,
            Err(e) => {
                for (pending, _) in &unverified {
                    self.report_swap(pending, format!("cannot list its escrow on Arkade: {e}"));
                }
                return Ok(());
            }
        };
        for (pending, swap) in &unverified {
            self.settle_paid_swap(ark, pending, swap, &listed).await?;
        }
        Ok(())
    }

    /// Mark a paid swap's ticket paid, if `vtxos` hold its escrow VTXO as
    /// [`Coordinator::verified_escrow_vtxo`] requires. Whether it did.
    ///
    /// The periodic check and the escrow subscription both settle swaps through this.
    pub(super) async fn settle_paid_swap(
        &self,
        ark: &Arkade,
        pending: &crate::domain::competitions::PendingArkSwap,
        swap: &crate::infra::ark_swap::Swap,
        vtxos: &[coordinator_ark::VirtualTxOutPoint],
    ) -> Result<bool, Error> {
        // The subscription and the periodic check may come to the same swap at once.
        let _settling = self.escrow_watch.settling.lock().await;
        let funded = self
            .competition_store
            .ticket_ark_escrow(pending.ticket_id, &pending.ticket_hash)
            .await?
            .is_some_and(|escrow| escrow.vtxo_outpoint.is_some());
        if funded {
            self.escrow_watch.forget(&pending.escrow_address);
            return Ok(true);
        }
        let Some((vtxo, sats)) = self.verified_escrow_vtxo(ark, pending, swap, vtxos).await else {
            return Ok(false);
        };
        let paid = self
            .competition_store
            .mark_ticket_ark_paid(
                pending.ticket_id,
                pending.ticket_hash.clone(),
                pending.competition_id,
                vtxo.clone(),
                sats,
            )
            .await?;
        self.reported.clear(SWAP_REPORTS, pending.swap_id);
        self.escrow_watch.forget(&pending.escrow_address);
        if paid {
            info!("Ticket {} paid into its escrow {vtxo}", pending.ticket_id);
        } else {
            // Its escrow is recorded as funded, so cleanup can still refund it.
            warn!(
                "Escrow {vtxo} of ticket {} is funded, but the ticket is no longer \
                 reserved, so it was not marked paid",
                pending.ticket_id
            );
        }
        self.wake_competition(pending.competition_id);
        Ok(true)
    }

    /// Log a problem with a pending swap once, then at debug while it lasts: swaps are checked
    /// every few seconds.
    pub(super) fn report_swap(
        &self,
        pending: &crate::domain::competitions::PendingArkSwap,
        problem: String,
    ) {
        if self
            .reported
            .is_new(SWAP_REPORTS, pending.swap_id, &problem)
        {
            warn!(
                "Escrow swap {} for ticket {} {problem}",
                pending.swap_id, pending.ticket_id
            );
        } else {
            debug!("Escrow swap {} {problem}", pending.swap_id);
        }
    }

    /// The escrow VTXO holding a paid swap's buy-in, and its value, as Arkade lists it.
    ///
    /// ark-swapd reports the VTXO it paid, or at least the Ark transaction that paid it, and
    /// neither is taken on trust: the swap must pay the ticket's own escrow address its price,
    /// and Arkade must list an unspent VTXO there with that value, in `vtxos`, a listing or what
    /// the escrow subscription reported. Until it does the ticket stays unpaid.
    async fn verified_escrow_vtxo(
        &self,
        ark: &Arkade,
        pending: &crate::domain::competitions::PendingArkSwap,
        swap: &crate::infra::ark_swap::Swap,
        vtxos: &[coordinator_ark::VirtualTxOutPoint],
    ) -> Option<(String, u64)> {
        let escrow = match self
            .competition_store
            .ticket_ark_escrow(pending.ticket_id, &pending.ticket_hash)
            .await
        {
            Ok(Some(escrow)) => escrow,
            Ok(None) => {
                self.report_swap(pending, "funds a ticket that has no escrow".into());
                return None;
            }
            Err(e) => {
                self.report_swap(pending, format!("cannot find its ticket's escrow: {e}"));
                return None;
            }
        };
        let competition = match self
            .competition_store
            .get_competition(pending.competition_id)
            .await
        {
            Ok(competition) => competition,
            Err(e) => {
                self.report_swap(pending, format!("cannot read its competition: {e}"));
                return None;
            }
        };
        // The ticket's own price: its network fee was fixed before its swap was made. A ticket
        // swapped before network fees existed has none.
        let network_fee = match self
            .competition_store
            .fixed_ticket_network_fee(pending.ticket_id, &pending.ticket_hash)
            .await
        {
            Ok(fee) => fee.unwrap_or(0),
            Err(e) => {
                self.report_swap(
                    pending,
                    format!("cannot read its ticket's network fee: {e}"),
                );
                return None;
            }
        };
        let price = match TicketPrice::new(&competition, network_fee) {
            Ok(price) => price.ticket_price_sats,
            Err(e) => {
                self.report_swap(pending, format!("cannot price its ticket: {e}"));
                return None;
            }
        };
        if swap.escrow_address != escrow.escrow_address {
            self.report_swap(
                pending,
                format!(
                    "pays {}, not the ticket's escrow {}",
                    swap.escrow_address, escrow.escrow_address
                ),
            );
            return None;
        }
        if swap.amount_sat != price {
            self.report_swap(
                pending,
                format!(
                    "pays {} sats, not the ticket's price of {price}",
                    swap.amount_sat
                ),
            );
            return None;
        }
        // The refund leaf opens at the escrow's locktime, and an expired VTXO cannot be spent
        // offchain. So the coin ark-swapd paid with must outlive the locktime by a margin.
        let live_until =
            crate::domain::competitions::ark_refund::refund_opens_at(&escrow.escrow_tap_tree)
                .map(|opens| opens.unix_timestamp() + ark.escrow_expiry_margin_secs as i64);
        match paid_escrow_vtxo(vtxos, &escrow.escrow_address, swap, price, live_until) {
            Ok(outpoint) => Some((outpoint.to_string(), price)),
            Err(problem) => {
                self.report_swap(pending, problem);
                None
            }
        }
    }

    /// Fund the pool in an Arkade batch, signing its contract inside the batch.
    ///
    /// The contract is bound first, with a null funding outpoint, because Keymeld signs each
    /// escrow's intent proof before the batch exists. The batch then spends every ticket's escrow
    /// into the funding output and the coordinator's fee. Keymeld signs the contract, expiry
    /// transaction included, before any escrow is forfeited.
    pub(super) async fn ark_kickoff(
        &self,
        competition: &mut Competition,
    ) -> Result<(), anyhow::Error> {
        self.competition_store
            .with_arkade_kickoff(self.ark_kickoff_admitted(competition))
            .await?
            .unwrap_or_else(|| {
                Err(anyhow!(
                    "Waiting for another pool's Arkade kickoff to finish"
                ))
            })
    }

    async fn ark_kickoff_admitted(
        &self,
        competition: &mut Competition,
    ) -> Result<(), anyhow::Error> {
        use coordinator_ark::{
            fund_pool, DlcKickoff, EscrowInput, KeypairSigner, KickoffConfig, PoolFunding,
        };
        use coordinator_ark_escrow::{EntryEscrow, VtxoScript};

        let ark = self
            .ark()
            .ok_or_else(|| anyhow!("Arkade is not configured"))?;
        let params = competition
            .contract_parameters
            .clone()
            .ok_or_else(|| anyhow!("The contract is not built yet"))?;
        let stored = self
            .competition_store
            .get_keymeld_session(competition.id)
            .await?
            .ok_or_else(|| anyhow!("No Keymeld session for competition {}", competition.id))?;
        let session = self.restore_keymeld_session(&stored)?;
        self.verify_keymeld_competition(competition, &session)
            .await?;
        let mut entries = self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await?;
        entries.sort_by_key(|entry| entry.ticket_id);
        self.bind_automatic_contract(competition, &session, &entries, &params, OutPoint::null())
            .await?;
        let players = entries
            .iter()
            .map(|entry| {
                let key = BitcoinPublicKey::from_str(&entry.ephemeral_pubkey)?;
                Ok((
                    key.inner.x_only_public_key().0,
                    UserId::from(entry.ticket_id),
                ))
            })
            .collect::<Result<Vec<_>, anyhow::Error>>()?;
        let keymeld_pool = Arc::new(KeymeldArkPool::new(self.keymeld.clone(), session, players));
        if let Some(done) = self
            .competition_store
            .ark_commitment(competition.id)
            .await?
        {
            // A batch already funded the pool; only the contract signatures were lost.
            return self
                .resume_ark_kickoff(competition, params, keymeld_pool.as_ref(), done)
                .await;
        }

        // Refunds run only for a competition that will never kick off, so none should be under
        // way here. If one is, its escrow is on its way back to its player, and the pool must
        // not be funded without it.
        let refunding = self
            .competition_store
            .ark_refunds_started(competition.id)
            .await?;
        if refunding > 0 {
            return Err(anyhow!(
                "{refunding} of its escrows are being refunded, so its pool cannot be funded"
            ));
        }
        let escrows = self
            .competition_store
            .funded_ark_escrows(competition.id)
            .await?;
        if escrows.len() != entries.len() {
            return Err(anyhow!(
                "{} entries but {} funded escrows",
                entries.len(),
                escrows.len()
            ));
        }
        let inputs = escrows
            .iter()
            .map(|escrow| {
                let tap_tree = hex::decode(&escrow.escrow_tap_tree)?;
                let vtxo = VtxoScript::decode_tap_tree(&tap_tree)?;
                Ok(EscrowInput {
                    escrow: EntryEscrow::from_vtxo_script(&vtxo)?,
                    outpoint: escrow
                        .vtxo_outpoint
                        .as_deref()
                        .ok_or_else(|| {
                            anyhow!("Escrow for ticket {} is unfunded", escrow.ticket_id)
                        })?
                        .parse()?,
                    amount: Amount::from_sat(escrow.vtxo_sats.ok_or_else(|| {
                        anyhow!("Escrow for ticket {} has no amount", escrow.ticket_id)
                    })?),
                })
            })
            .collect::<Result<Vec<_>, anyhow::Error>>()?;

        let hooks = DlcKickoff::new(params.clone(), keymeld_pool.clone())?;
        let info = ark.server.info();
        let escrowed: Amount = inputs.iter().map(|input| input.amount).sum();
        let fee = escrowed
            .checked_sub(hooks.funding_output().value)
            .ok_or_else(|| anyhow!("The escrows hold less than the pool"))?;
        // Each escrow's signer takes a fee output of at most its own cap per escrow, and tickets
        // issued at different fee rates have different caps, so the lowest binds them all.
        let cap = self.pool_fee_cap(&entries).await?;
        let fee = if fee > cap {
            warn!(
                "Competition {}: the escrows hold {fee} beyond the pool, but their lowest fee cap \
                 allows {cap}; the rest goes to the Arkade server",
                competition.id
            );
            cap
        } else {
            fee
        };
        let mut pool = PoolFunding::new(
            inputs,
            hooks.funding_output().clone(),
            ark.server.rules(),
            info.dust,
        )?;
        if fee >= info.dust {
            let address = self.bitcoin.get_next_address().await?;
            pool = pool.with_coordinator_fee(
                TxOut {
                    value: fee,
                    script_pubkey: address.script_pubkey(),
                },
                info.dust,
            )?;
        } else if fee > Amount::ZERO {
            warn!("Coordinator fee {fee} is below dust; the Arkade server keeps it");
        }
        let secret = bitcoin::secp256k1::SecretKey::from_slice(&self.private_key.serialize())?;
        let coordinator_key =
            bitcoin::key::Keypair::from_secret_key(&bitcoin::secp256k1::Secp256k1::new(), &secret);

        info!(
            "Kicking off competition {} in an Arkade batch: {} escrows, {} into the pool",
            competition.id,
            pool.inputs().len(),
            hooks.funding_output().value
        );
        let kickoff = fund_pool(
            ark.server.client(),
            info,
            &pool,
            keymeld_pool.as_ref(),
            &KeypairSigner::new([coordinator_key]),
            &hooks,
            &KickoffConfig::for_server(info),
        )
        .await;
        self.arkade_health.record(&kickoff);
        let kickoff = kickoff?;
        let signed = hooks
            .signed_contract()
            .ok_or_else(|| anyhow!("The batch finished without a signed contract"))?;
        let commitment = keymeld_pool
            .commitment_tx()
            .ok_or_else(|| anyhow!("The batch finished without its commitment transaction"))?;
        self.competition_store
            .store_ark_commitment(
                competition.id,
                ArkCommitment {
                    batch_id: kickoff.batch_id.clone(),
                    commitment_tx: bitcoin::consensus::encode::serialize_hex(&commitment),
                    funding_vout: kickoff.funding.vout,
                },
            )
            .await?;
        info!(
            "Competition {} funded in batch {}: commitment {}",
            competition.id, kickoff.batch_id, kickoff.commitment_txid
        );
        let now = OffsetDateTime::now_utc();
        competition.funding_outpoint = Some(kickoff.funding);
        competition.funding_transaction = Some(commitment);
        competition.signed_contract = Some(signed);
        competition.signed_at = Some(now);
        competition.funding_broadcasted_at = Some(now);
        competition.errors = vec![];
        Ok(())
    }

    /// Finish a kickoff whose batch funded the pool before the competition was saved.
    ///
    /// Keymeld signs the bound contract again at the recorded funding outpoint.
    async fn resume_ark_kickoff(
        &self,
        competition: &mut Competition,
        params: ContractParameters,
        signer: &KeymeldArkPool,
        done: ArkCommitment,
    ) -> Result<(), anyhow::Error> {
        use coordinator_ark::ContractSigner;

        let commitment: Transaction =
            bitcoin::consensus::encode::deserialize_hex(&done.commitment_tx)?;
        let funding = OutPoint::new(commitment.compute_txid(), done.funding_vout);
        let dlc = TicketedDLC::new(params, funding)?;
        let psbt = Psbt::from_unsigned_tx(commitment.clone())?;
        let signatures = signer
            .sign_contract(&dlc, &psbt)
            .await
            .map_err(|e| anyhow!("Keymeld could not sign the funded contract again: {e}"))?;
        let market_maker = dlc.params().market_maker.pubkey;
        let signed = dlc
            .into_signed_contract(market_maker, signatures)
            .map_err(|e| anyhow!("Keymeld produced an invalid contract signature set: {e}"))?;
        info!(
            "Competition {} resumed from batch {}: commitment {}",
            competition.id,
            done.batch_id,
            commitment.compute_txid()
        );
        let now = OffsetDateTime::now_utc();
        competition.funding_outpoint = Some(funding);
        competition.funding_transaction = Some(commitment);
        competition.signed_contract = Some(signed);
        competition.signed_at = Some(now);
        competition.funding_broadcasted_at = Some(now);
        competition.errors = vec![];
        Ok(())
    }
}

/// When an escrow's refund leaf opens, in UNIX seconds: `after_start_secs` after the observation
/// window starts, and no later than the contract's `expiry`, since players' wallets refuse an
/// escrow that outlasts it.
///
/// The locktime is part of the escrow's script, so an escrow already issued keeps the one it
/// was issued with.
fn escrow_refund_at(start: OffsetDateTime, after_start_secs: u64, expiry: Option<u32>) -> i64 {
    let refund_at = start
        .unix_timestamp()
        .saturating_add(i64::try_from(after_start_secs).unwrap_or(i64::MAX));
    match expiry {
        Some(expiry) => refund_at.min(i64::from(expiry)),
        None => refund_at,
    }
}

/// What a player is told when ark-swapd would not make a ticket's swap. A wallet that cannot fund
/// it now may later, so that is retryable; anything else is a refusal.
fn swap_failure(error: &anyhow::Error) -> Error {
    if error.is::<crate::infra::ark_swap::SwapsUnavailable>() {
        Error::SwapsUnavailable
    } else {
        Error::BadRequest("Failed to create invoice".into())
    }
}

/// The message a player sees when ark-swapd cannot fund a swap right now.
pub const SWAPS_UNAVAILABLE: &str =
    "Lightning payments are unavailable for a moment, so no invoice was made; try again in a moment";

/// Reports about pending escrow swaps, by swap.
const SWAP_REPORTS: &str = "escrow swap";

/// How often ark-swapd's boards are read, and how long it has to answer.
pub const ARK_SWAP_BOARDS_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
const ARK_SWAP_WALLET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The VTXO among `vtxos` that a paid swap put in the escrow at `address`.
///
/// It is the one the swap names, or else the output of the swap's Ark transaction at
/// `address`, since `vtxos` may list other escrows the same transaction paid. It must be
/// at `address`, unspent, and worth `price`. With `live_until` (UNIX seconds), it must not
/// expire on Arkade before then: a VTXO inherits the expiry of the coins that paid it, and an
/// expired one cannot be refunded offchain. The error says what is missing or wrong.
fn paid_escrow_vtxo(
    vtxos: &[coordinator_ark::VirtualTxOutPoint],
    address: &str,
    swap: &crate::infra::ark_swap::Swap,
    price: u64,
    live_until: Option<i64>,
) -> Result<OutPoint, String> {
    let script = coordinator_ark::ArkAddress::decode(address)
        .map_err(|e| format!("pays an escrow whose address is invalid: {e}"))?
        .to_p2tr_script_pubkey();
    let named: Option<OutPoint> = swap
        .escrow_vtxo
        .as_deref()
        .map(|vtxo| {
            vtxo.parse()
                .map_err(|e| format!("names an invalid escrow VTXO {vtxo}: {e}"))
        })
        .transpose()?;
    let paid_in: Option<bitcoin::Txid> = swap
        .ark_txid
        .as_deref()
        .map(|txid| {
            txid.parse()
                .map_err(|e| format!("names an invalid Ark transaction {txid}: {e}"))
        })
        .transpose()?;
    if let (Some(named), Some(paid_in)) = (named, paid_in) {
        if named.txid != paid_in {
            return Err(format!(
                "names escrow VTXO {named}, which its Ark transaction {paid_in} did not create"
            ));
        }
    }
    let vtxo = match (named, paid_in) {
        (Some(named), _) => vtxos.iter().find(|vtxo| vtxo.outpoint == named),
        (None, Some(paid_in)) => vtxos
            .iter()
            .filter(|vtxo| vtxo.outpoint.txid == paid_in)
            .min_by_key(|vtxo| vtxo.script != script),
        (None, None) => {
            return Err(
                "reports its player paid, but neither its escrow VTXO nor the Ark \
                        transaction that paid it"
                    .into(),
            )
        }
    };
    let Some(vtxo) = vtxo else {
        return Err(format!(
            "reports its player paid; waiting for Arkade to list the escrow VTXO at {address}"
        ));
    };
    if vtxo.script != script {
        return Err(format!("paid {}, which is not at {address}", vtxo.outpoint));
    }
    if vtxo.amount.to_sat() != price {
        return Err(format!(
            "paid {} with {} sats, not the ticket's price of {price}",
            vtxo.outpoint,
            vtxo.amount.to_sat()
        ));
    }
    if vtxo.is_spent {
        return Err(format!(
            "paid {}, which Arkade lists as already spent",
            vtxo.outpoint
        ));
    }
    // A server that lists no expiry gives nothing to judge by.
    if let Some(live_until) = live_until.filter(|_| vtxo.expires_at > 0) {
        if vtxo.is_swept || vtxo.expires_at < live_until {
            let at = |seconds: i64| {
                OffsetDateTime::from_unix_timestamp(seconds)
                    .map(|time| time.to_string())
                    .unwrap_or_else(|_| seconds.to_string())
            };
            return Err(format!(
                "paid {} with a coin that expires on Arkade at {}, but its escrow must live \
                 until {} for a refund to finish; the ticket is not counted, and its escrow \
                 needs an operator (docs/ops/stuck-escrow-check.md)",
                vtxo.outpoint,
                at(vtxo.expires_at),
                at(live_until),
            ));
        }
    }
    Ok(vtxo.outpoint)
}

/// The output script, hex, of the escrow at the encoded Ark `address`: what the escrow
/// subscription watches, and what Arkade lists the escrow's VTXOs under.
pub(super) fn escrow_script(address: &str) -> Option<String> {
    use bitcoin::hex::DisplayHex;
    coordinator_ark::ArkAddress::decode(address)
        .ok()
        .map(|address| {
            address
                .to_p2tr_script_pubkey()
                .as_bytes()
                .to_lower_hex_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::infra::ark_swap::SwapsUnavailable;

    /// A refund must open while the escrow's VTXO is alive. A VTXO lives seven days from the
    /// batch its coins descend from, and ark-swapd's coins may be days old, so a day after the
    /// start was too late: escrows paid from a coin with a day left had expired by then.
    #[test]
    fn an_escrows_refund_opens_soon_after_the_start_and_by_the_contracts_expiry() {
        use crate::config::DEFAULT_REFUND_AFTER_START_SECS;
        use coordinator_ark::testing::{keypair, mock_info, xonly};

        let start = OffsetDateTime::from_unix_timestamp(1_790_650_000).unwrap();
        let opens = escrow_refund_at(start, DEFAULT_REFUND_AFTER_START_SECS, None);
        assert_eq!(opens, start.unix_timestamp() + 45 * 60);
        assert!(
            opens - start.unix_timestamp() < 60 * 60,
            "well inside the life of any coin worth paying an escrow from"
        );

        // Players' wallets refuse an escrow that outlasts the contract's expiry.
        let expiry = start.unix_timestamp() as u32 + 600;
        assert_eq!(
            escrow_refund_at(start, DEFAULT_REFUND_AFTER_START_SECS, Some(expiry)),
            i64::from(expiry)
        );
        let later = start.unix_timestamp() as u32 + 7 * 86_400;
        assert_eq!(
            escrow_refund_at(start, DEFAULT_REFUND_AFTER_START_SECS, Some(later)),
            opens
        );

        // The escrow's script carries that time as its refund leaf's locktime.
        let rules = coordinator_ark::server_rules(&mock_info(&keypair(7))).unwrap();
        let terms = coordinator_ark::escrow_terms(
            &rules,
            xonly(&keypair(21)),
            xonly(&keypair(18)),
            opens as u32,
            start.unix_timestamp() as u32 - 3_600,
        )
        .unwrap();
        assert_eq!(
            terms.refund_locktime,
            bitcoin::absolute::LockTime::from_consensus(opens as u32)
        );
    }

    #[test]
    fn an_unfunded_swap_service_is_retryable_and_other_failures_are_not() {
        let unavailable = anyhow::Error::new(SwapsUnavailable(
            "ark-swapd answered 503 Service Unavailable: {}".into(),
        ));
        assert!(matches!(
            swap_failure(&unavailable),
            Error::SwapsUnavailable
        ));
        assert!(matches!(
            swap_failure(&anyhow!("ark-swapd answered 400 Bad Request: dust")),
            Error::BadRequest(_)
        ));
        assert!(matches!(
            swap_failure(&anyhow!("error sending request")),
            Error::BadRequest(_)
        ));
    }
}
