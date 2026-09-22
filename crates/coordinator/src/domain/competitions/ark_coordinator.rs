//! The Coordinator's Arkade path: escrow swaps at entry, and a pool kickoff in an Arkade batch.
//!
//! A ticket's buy-in waits in an escrow VTXO whose player key is the entry key.
//! `ark-swapd` pays the escrow from its own Ark wallet while holding the player's Lightning
//! payment, and settles with the ticket preimage, so paying still reveals it to the player.
//! See `ark_kickoff.rs` and `docs/QUEUED_COMPETITIONS.md`.

use super::*;
use crate::domain::competitions::{ArkCommitment, Arkade, KeymeldArkPool, TicketArkEscrow};
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
                let refund_at = competition
                    .event_submission
                    .start_observation_date
                    .unix_timestamp()
                    + ark.refund_after_start_secs as i64;
                // Players' wallets refuse an escrow that outlasts the contract's expiry.
                let refund_at = match competition
                    .event_announcement
                    .as_ref()
                    .and_then(|event| event.expiry)
                {
                    Some(expiry) => refund_at.min(i64::from(expiry)),
                    None => refund_at,
                };
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
        let fee =
            competition.calculate_invoice_amount() - competition.event_submission.entry_fee as u64;
        Ok(Some(ArkEscrowPolicy {
            escrow_tap_tree,
            max_fee_sats: fee,
            max_refund_fee_sats: ark.max_refund_fee_sats,
            // A refund's transactions pass through this server's checkpoint outputs, so the
            // player consents to the script that makes them.
            checkpoint_exit_script: hex::encode(
                ark.server.info().checkpoint_tapscript.as_bytes(),
            ),
        }))
    }

    /// The swap invoice that pays the ticket's escrow, and when it expires.
    ///
    /// The invoice pays to the ticket's hash, so the player learns the ticket preimage by paying.
    pub(super) async fn ticket_ark_invoice(
        &self,
        ark: &Arkade,
        ticket: &Ticket,
        amount_sats: u64,
    ) -> Result<(String, OffsetDateTime), Error> {
        let escrow: TicketArkEscrow = self
            .competition_store
            .ticket_ark_escrow(ticket.id, &ticket.hash)
            .await?
            .ok_or_else(|| Error::BadRequest("The ticket has no escrow yet".into()))?;
        let preimage: [u8; 32] = hex::decode(&ticket.encrypted_preimage)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| Error::BadRequest("Invalid ticket preimage".into()))?;
        let swap = ark
            .swaps
            .create_swap(&escrow.escrow_address, amount_sats, &preimage)
            .await
            .map_err(|e| {
                error!(
                    "Failed to create the escrow swap for ticket {}: {e:#}",
                    ticket.id
                );
                Error::BadRequest("Failed to create invoice".into())
            })?;
        if swap.payment_hash != ticket.hash {
            return Err(Error::BadRequest(
                "The swap invoice does not pay to the ticket's hash".into(),
            ));
        }
        self.competition_store
            .set_ticket_ark_swap(ticket.id, ticket.hash.clone(), swap.id)
            .await?;
        let expires_at = OffsetDateTime::from_unix_timestamp(swap.expires_at)
            .map_err(|e| Error::BadRequest(e.to_string()))?;
        Ok((swap.invoice, expires_at))
    }

    /// Advance every pending escrow swap. A ticket is paid once its escrow holds the buy-in.
    pub async fn check_ark_swaps(&self) -> Result<(), Error> {
        let Some(ark) = self.ark() else {
            return Ok(());
        };
        for pending in self.competition_store.pending_ark_swaps().await? {
            let swap = match ark.swaps.swap(pending.swap_id).await {
                Ok(swap) => swap,
                Err(e) => {
                    warn!(
                        "Escrow swap {} for ticket {}: {e:#}",
                        pending.swap_id, pending.ticket_id
                    );
                    continue;
                }
            };
            if swap.state.escrow_funded() {
                let Some(vtxo) = swap.escrow_vtxo.clone() else {
                    warn!("Escrow swap {} reports funding without a VTXO", swap.id);
                    continue;
                };
                self.competition_store
                    .mark_ticket_ark_funded(
                        pending.ticket_id,
                        pending.ticket_hash.clone(),
                        vtxo,
                        swap.amount_sat,
                    )
                    .await?;
                self.competition_store
                    .mark_ticket_paid(&pending.ticket_hash, pending.competition_id)
                    .await?;
                // The swap service settles the player's invoice itself once the escrow is funded,
                // so the coordinator never settles an Arkade ticket's invoice.
                self.competition_store
                    .mark_ticket_settled(pending.ticket_id)
                    .await?;
                info!("Ticket {} paid into its escrow", pending.ticket_id);
                self.wake_competition(pending.competition_id);
            } else if swap.state.abandoned() {
                let ticket = self.competition_store.get_ticket(pending.ticket_id).await?;
                self.competition_store
                    .clear_ticket_reservation(&ticket)
                    .await?;
                info!(
                    "Escrow swap for ticket {} ended unpaid; the reservation is released",
                    pending.ticket_id
                );
            }
        }
        Ok(())
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
        .await?;
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
