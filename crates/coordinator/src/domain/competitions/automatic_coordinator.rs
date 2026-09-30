//! Browser-independent payout preparation and paid-claim recovery.
use super::*;
use crate::domain::{PaymentStatus, PayoutJob};
use coordinator_core::PayoutRegistrationRequest;
use coordinator_escrow::{
    authorization::PayoutPolicy,
    payout::{validate_invoice, ContractAuthorization, ContractCommitment},
    payout_protocol::{
        PayoutContractBoundResponse, PayoutMethod, PayoutPreparedResponse, PreparePayoutRequest,
        ReleasePayoutRequest,
    },
};

const TICKET_AUTHORIZATION_CONFLICT: &str =
    "Your ticket was already requested with a different entry key or payout choice; that \
     request has been cancelled, so request the ticket again";

/// Whether two payout policies carry the same choices by the player. The rest is derived by
/// the coordinator and may differ between requests without changing what the player chose.
fn same_player_choice(a: &PayoutPolicy, b: &PayoutPolicy) -> Result<bool, Error> {
    let consent = |policy| {
        coordinator_escrow::queued::EntryConsent::from_policy(policy)
            .map_err(|e| Error::BadRequest(e.to_string()))
    };
    let (a_terms, b_terms) = (consent(a)?, consent(b)?);
    Ok(
        a.automatic_lightning_address == b.automatic_lightning_address
            && a.allow_invoice_fallback == b.allow_invoice_fallback
            && a.release_entry_key_after_payment == b.release_entry_key_after_payment
            && a_terms.entry_id() == b_terms.entry_id()
            && a_terms.competition_id() == b_terms.competition_id()
            && a_terms.ticket_hash() == b_terms.ticket_hash()
            && a_terms.payout_hash() == b_terms.payout_hash(),
    )
}

#[derive(Serialize)]
pub struct PayoutAuthorizationInfo {
    pub keygen_session_id: String,
    pub user_id: Uuid,
    pub competition_id: Uuid,
    pub entry_id: Uuid,
    pub contract_digest: String,
    pub amount_msat: u64,
    pub automatic_lightning_address: Option<String>,
    pub allow_invoice_fallback: bool,
    pub status: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceFallbackRequest {
    pub invoice: String,
    pub authorization: coordinator_escrow::payout_protocol::SignedInvoiceAuthorization,
}

#[derive(Serialize)]
pub struct PayoutTermsQuote {
    pub enabled: bool,
    pub relative_locktime_block_delta: u16,
    pub max_fee_rate_sat_vb: u64,
    /// Buy-ins wait in Arkade escrows, refunded to the entry's Lightning Address if the pool never starts.
    pub arkade: bool,
}

impl Coordinator {
    pub async fn payout_terms_quote(
        &self,
        competition_id: Uuid,
    ) -> Result<PayoutTermsQuote, Error> {
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;
        Ok(PayoutTermsQuote {
            enabled: self
                .competition_store
                .has_automatic_payouts(competition_id)
                .await?,
            relative_locktime_block_delta: competition
                .event_submission
                .relative_locktime_block_delta
                .unwrap_or(self.relative_locktime_block_delta as u16),
            max_fee_rate_sat_vb: self.automatic_payout_max_fee_rate.to_sat_per_vb_floor(),
            arkade: self.competition_store.is_ark_funded(competition_id).await?,
        })
    }

    pub async fn payout_authorization_info(
        &self,
        owner: &str,
        competition_id: Uuid,
        entry_id: Uuid,
    ) -> Result<PayoutAuthorizationInfo, Error> {
        let entry = self
            .competition_store
            .get_entry_by_id(entry_id)
            .await?
            .filter(|e| e.pubkey == owner && e.event_id == competition_id)
            .ok_or_else(|| Error::NotFound("Entry not found".into()))?;
        let json = self
            .competition_store
            .entry_payout_policy(entry_id)
            .await?
            .ok_or_else(|| Error::NotFound("Entry uses the legacy payout protocol".into()))?;
        let policy: PayoutPolicy =
            serde_json::from_str(&json).map_err(|e| Error::Bitcoin(e.into()))?;
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;
        let params = competition
            .contract_parameters
            .clone()
            .ok_or_else(|| Error::BadRequest("Contract is not ready".into()))?;
        let outcome = competition.get_current_outcome().map_err(Error::Bitcoin)?;
        let amount = winner_payout_sats(
            &params,
            &outcome,
            &entry
                .ephemeral_pubkey
                .parse()
                .map_err(|e| Error::Bitcoin(anyhow!("Invalid entry key: {e}")))?,
        )
        .map_err(|e| Error::BadRequest(e.to_string()))?;
        let context = entry
            .keymeld_registration_context
            .ok_or_else(|| Error::BadRequest("Missing Keymeld registration".into()))?;
        let contract = ContractCommitment {
            contract_parameters: params,
            funding_outpoint: competition
                .funding_outpoint
                .ok_or_else(|| Error::BadRequest("Missing funding outpoint".into()))?,
        };
        let window_closed = self
            .competition_store
            .payout_window_is_closed(competition_id)
            .await?
            || competition.delta_broadcasted_at.is_some()
            || competition.completed_at.is_some()
            || competition.cancelled_at.is_some();
        let status = self
            .competition_store
            .payout_job_status(entry_id)
            .await?
            .unwrap_or_else(|| {
                if window_closed {
                    "On-chain settlement".into()
                } else {
                    "Awaiting payout".into()
                }
            });
        Ok(PayoutAuthorizationInfo {
            keygen_session_id: context.keygen_session_id.to_string(),
            user_id: entry.ticket_id,
            competition_id,
            entry_id,
            contract_digest: coordinator_escrow::payout::contract_digest(&contract)
                .map_err(|e| Error::BadRequest(e.to_string()))?,
            amount_msat: amount
                .checked_mul(1000)
                .ok_or_else(|| Error::BadRequest("Payout amount overflow".into()))?,
            automatic_lightning_address: policy.automatic_lightning_address,
            allow_invoice_fallback: policy.allow_invoice_fallback
                && !window_closed
                && entry.paid_out_at.is_none(),
            status,
        })
    }

    pub async fn submit_invoice_fallback(
        &self,
        owner: &str,
        competition_id: Uuid,
        entry_id: Uuid,
        request: InvoiceFallbackRequest,
    ) -> Result<Uuid, Error> {
        let info = self
            .payout_authorization_info(owner, competition_id, entry_id)
            .await?;
        if !info.allow_invoice_fallback {
            return Err(Error::BadRequest(
                "Invoice fallback is not available for this entry".into(),
            ));
        }
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;
        if competition.delta_broadcasted_at.is_some()
            || competition.completed_at.is_some()
            || competition.cancelled_at.is_some()
        {
            return Err(Error::BadRequest(
                "On-chain settlement has already started".into(),
            ));
        }
        let now = OffsetDateTime::now_utc().unix_timestamp() as u64;
        validate_invoice(
            &request.invoice,
            info.amount_msat / 1000,
            self.bitcoin.get_network(),
            now,
        )
        .map_err(|e| Error::BadRequest(e.to_string()))?;
        let actual = &request.authorization.context;
        let expected = coordinator_escrow::payout_protocol::InvoiceAuthorizationContext {
            keygen_session_id: keymeld_sdk::SessionId::new(&info.keygen_session_id),
            user_id: UserId::from(info.user_id),
            claim_id: actual.claim_id,
            competition_id,
            entry_id,
            contract_digest: info.contract_digest,
            invoice_digest: coordinator_escrow::payout::invoice_digest(&request.invoice),
            amount_msat: info.amount_msat,
            expires_at: actual.expires_at,
        };
        if expected.expires_at > now.saturating_add(600) {
            return Err(Error::BadRequest(
                "Invoice authorization must expire within ten minutes".into(),
            ));
        }
        let entry = self
            .competition_store
            .get_entry_by_id(entry_id)
            .await?
            .ok_or_else(|| Error::NotFound("Entry not found".into()))?;
        let public_key =
            hex::decode(&entry.ephemeral_pubkey).map_err(|e| Error::Bitcoin(e.into()))?;
        request
            .authorization
            .verify(&public_key, &expected, now)
            .map_err(|e| Error::BadRequest(e.to_string()))?;
        let method = PayoutMethod::Invoice {
            invoice: request.invoice,
            authorization: request.authorization,
        };
        self.competition_store
            .queue_invoice_fallback(
                expected.claim_id,
                entry_id,
                serde_json::to_string(&method).map_err(|e| Error::Bitcoin(e.into()))?,
            )
            .await
            .map_err(Error::from)
    }

    pub fn with_automatic_payouts(
        mut self,
        enabled: bool,
        max_fee_rate_sat_vb: u64,
    ) -> Result<Self, anyhow::Error> {
        if enabled && !self.is_keymeld_enabled() {
            return Err(anyhow!("Automatic payouts require Keymeld"));
        }
        self.automatic_payout_max_fee_rate = FeeRate::from_sat_per_vb(max_fee_rate_sat_vb)
            .filter(|rate| *rate > FeeRate::ZERO)
            .ok_or_else(|| anyhow!("Invalid payout contract fee ceiling"))?;
        self.automatic_payouts = enabled;
        Ok(self)
    }

    pub(super) async fn require_payout_capabilities(&self, lnurl: bool) -> Result<(), Error> {
        if !self.is_keymeld_enabled() {
            return Err(Error::BadRequest("Payout escrow requires Keymeld".into()));
        }
        let caps = self
            .keymeld
            .payout_capabilities()
            .await
            .map_err(|e| Error::Bitcoin(anyhow!(e)))?;
        if !caps.payout || (lnurl && !caps.lnurl) {
            return Err(Error::BadRequest(
                "This deployment does not support the requested payout method".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_ticket_payout_choice(
        entry_pubkey: &BitcoinPublicKey,
        choice: &PayoutRegistrationRequest,
    ) -> Result<(), Error> {
        if choice.entry_id.get_version_num() != 7 || !entry_pubkey.compressed {
            return Err(Error::BadRequest(
                "Payout registration requires a UUIDv7 entry and compressed entry public key"
                    .into(),
            ));
        }
        parse_hash32(&choice.payout_hash).map_err(|e| Error::BadRequest(e.to_string()))?;
        if !choice.release_entry_key_after_payment
            || (!choice.allow_invoice_fallback && choice.lightning_address.is_none())
        {
            return Err(Error::BadRequest(
                "Explicit payout method and payment-before-key-release authorization are required"
                    .into(),
            ));
        }
        if let Some(address) = &choice.lightning_address {
            LightningAddress::parse(address).map_err(|e| Error::BadRequest(e.to_string()))?;
        }
        Ok(())
    }

    pub(super) async fn prepare_ticket_payout_policy(
        &self,
        competition: &Competition,
        ticket: &Ticket,
        entry_pubkey: &BitcoinPublicKey,
        choice: &PayoutRegistrationRequest,
    ) -> Result<(), Error> {
        Self::validate_ticket_payout_choice(entry_pubkey, choice)?;
        let address = choice
            .lightning_address
            .as_deref()
            .map(LightningAddress::parse)
            .transpose()
            .map_err(|e| Error::BadRequest(e.to_string()))?
            .map(|v| v.to_string());
        let tickets = self.competition_store.ticket_ids(competition.id).await?;
        let player_index = tickets
            .iter()
            .position(|id| *id == ticket.id)
            .ok_or_else(|| Error::BadRequest("Ticket is absent from the competition".into()))?;
        let event = competition.event_announcement.clone().ok_or_else(|| {
            Error::BadRequest("Oracle announcement is not ready; retry before paying".into())
        })?;
        let terms = ContractAuthorization {
            competition_id: competition.id,
            entry_id: choice.entry_id,
            network: self.bitcoin.get_network(),
            player_index,
            player_count: tickets.len(),
            ticket_hash: parse_hash32(&ticket.hash).map_err(Error::Bitcoin)?,
            payout_hash: parse_hash32(&choice.payout_hash).map_err(Error::Bitcoin)?,
            market_maker: dlctix::MarketMaker {
                pubkey: self.public_key,
            },
            event,
            outcome_payouts: slot_payouts(
                tickets.len(),
                competition.event_submission.number_of_places_win,
            )?,
            funding_value: Amount::from_sat(
                competition.event_submission.total_competition_pool as u64,
            ),
            relative_locktime_block_delta: competition
                .event_submission
                .relative_locktime_block_delta
                .unwrap_or(self.relative_locktime_block_delta as u16),
            max_fee_rate: self.automatic_payout_max_fee_rate,
        };
        let policy = PayoutPolicy {
            queued_entry: None,
            automatic_lightning_address: address,
            allow_invoice_fallback: choice.allow_invoice_fallback,
            release_entry_key_after_payment: choice.release_entry_key_after_payment,
            contract_terms: serde_json::to_string(&terms).map_err(|e| Error::Bitcoin(e.into()))?,
            ark_escrow: self
                .ticket_ark_escrow_policy(competition, ticket, entry_pubkey)
                .await?,
        };
        ContractAuthorization::from_policy(&policy)
            .map_err(|e| Error::BadRequest(e.to_string()))?;
        self.fix_ticket_payout_policy(ticket, entry_pubkey, &policy)
            .await
    }

    /// Fix `policy` as the ticket's payout authorization. A retried request whose response was
    /// lost is handed the same reserved ticket; it keeps the authorization fixed first unless
    /// the player chose differently, since the player's registration is sealed against it.
    pub(super) async fn fix_ticket_payout_policy(
        &self,
        ticket: &Ticket,
        entry_pubkey: &BitcoinPublicKey,
        policy: &PayoutPolicy,
    ) -> Result<(), Error> {
        let Some(fixed) = self
            .competition_store
            .store_ticket_payout_policy(
                ticket.id,
                ticket.hash.clone(),
                entry_pubkey.to_string(),
                serde_json::to_string(policy).map_err(|e| Error::Bitcoin(e.into()))?,
            )
            .await?
        else {
            return Ok(());
        };
        let stored: PayoutPolicy =
            serde_json::from_str(&fixed.policy_json).map_err(|e| Error::Bitcoin(e.into()))?;
        if fixed.entry_pubkey == entry_pubkey.to_string() && same_player_choice(&stored, policy)? {
            return Ok(());
        }
        Err(Error::Conflict(TICKET_AUTHORIZATION_CONFLICT.into()))
    }

    pub(super) async fn validate_entry_payout_policy(
        &self,
        entry: &AddEntry,
        ticket: &Ticket,
    ) -> Result<Option<String>, Error> {
        if !self
            .competition_store
            .has_automatic_payouts(entry.event_id)
            .await?
        {
            return Ok(None);
        }
        let json = self
            .competition_store
            .ticket_payout_policy(ticket.id, &ticket.hash)
            .await?
            .ok_or_else(|| Error::BadRequest("Missing pre-payment payout authorization".into()))?;
        let policy: PayoutPolicy =
            serde_json::from_str(&json).map_err(|e| Error::Bitcoin(e.into()))?;
        let registered_key = self
            .competition_store
            .ticket_payout_public_key(ticket.id, &ticket.hash)
            .await?;
        if registered_key != entry.ephemeral_pubkey {
            return Err(Error::BadRequest(
                "Entry key differs from its pre-payment payout authorization".into(),
            ));
        }
        // A queued entry consents to its competition's terms, not a contract: only the fields
        // every entry has are checked here.
        let terms = coordinator_escrow::queued::EntryConsent::from_policy(&policy)
            .map_err(|e| Error::BadRequest(e.to_string()))?;
        if terms.entry_id() != entry.id
            || terms.competition_id() != entry.event_id
            || terms.payout_hash() != parse_hash32(&entry.payout_hash).map_err(Error::Bitcoin)?
        {
            return Err(Error::BadRequest(
                "Entry differs from its payout authorization".into(),
            ));
        }
        self.require_payout_capabilities(policy.automatic_lightning_address.is_some())
            .await?;
        Ok(Some(json))
    }

    /// Entries authorize their payout table before paying. Preserve those
    /// exact weights across upgrades; changing them would invalidate consent
    /// and prevent the enclave from binding the funded contract.
    pub(super) async fn accepted_outcome_payouts(
        &self,
        competition_id: Uuid,
        entries: &[UserEntry],
    ) -> Result<Option<BTreeMap<Outcome, PayoutWeights>>, anyhow::Error> {
        if !self
            .competition_store
            .has_automatic_payouts(competition_id)
            .await?
        {
            return Ok(None);
        }
        if entries.is_empty() {
            return Err(anyhow!("Automatic competition has no paid entry policies"));
        }
        let mut accepted = None;
        for entry in entries {
            let json = self
                .competition_store
                .entry_payout_policy(entry.id)
                .await?
                .ok_or_else(|| anyhow!("Entry {} has no accepted payout policy", entry.id))?;
            let policy: PayoutPolicy = serde_json::from_str(&json)?;
            let terms = ContractAuthorization::from_policy(&policy)?;
            if entry.event_id != competition_id
                || terms.competition_id != competition_id
                || terms.entry_id != entry.id
                || terms.player_count != entries.len()
            {
                return Err(anyhow!(
                    "Entry {} payout authorization differs from the competition roster",
                    entry.id
                ));
            }
            if let Some(weights) = &accepted {
                if *weights != terms.outcome_payouts {
                    return Err(anyhow!(
                        "Competition entries authorized different payout tables"
                    ));
                }
            } else {
                accepted = Some(terms.outcome_payouts);
            }
        }
        Ok(accepted)
    }

    pub(super) async fn bind_automatic_contract(
        &self,
        competition: &Competition,
        session: &DlcKeygenSession,
        entries: &[UserEntry],
        params: &ContractParameters,
        funding_outpoint: OutPoint,
    ) -> Result<(), anyhow::Error> {
        if !self
            .competition_store
            .has_automatic_payouts(competition.id)
            .await?
        {
            return Ok(());
        }
        let contract = ContractCommitment {
            contract_parameters: params.clone(),
            funding_outpoint,
        };
        let mut expected_policies = BTreeMap::new();
        let mut signed_digests = BTreeMap::new();
        let mut registrations = BTreeMap::new();
        for entry in entries {
            let json = self
                .competition_store
                .entry_payout_policy(entry.id)
                .await?
                .ok_or_else(|| anyhow!("Entry {} has no accepted payout policy", entry.id))?;
            let policy: PayoutPolicy = serde_json::from_str(&json)?;
            let context = entry.keymeld_registration_context.as_ref().ok_or_else(|| {
                anyhow!("Entry {} has no participant registration context", entry.id)
            })?;
            let signed = entry.keymeld_escrow_policy.as_ref().ok_or_else(|| {
                anyhow!("Entry {} requires fresh generic escrow consent", entry.id)
            })?;
            coordinator_core::keymeld::verify_registration_policy(
                context,
                Some(&policy),
                Some(signed),
            )?;
            let user = UserId::from(entry.ticket_id);
            if expected_policies.insert(user.clone(), policy).is_some() {
                return Err(anyhow!("Duplicate payout participant {}", entry.ticket_id));
            }
            signed_digests.insert(user.clone(), signed.policy.digest()?);
            registrations.insert(user, context);
        }
        // A pool's players consented to terms, not this contract: Keymeld derives each one's
        // contract from the oracle's signed statement of the pool's event.
        let statement = match self.pool_of(competition).await? {
            Some((settings, record)) => Some(
                self.pool_statement(competition, &settings, &record.members)
                    .await?,
            ),
            None => None,
        };
        let bindings = match statement {
            Some(statement) => {
                self.keymeld
                    .bind_payout_contract_with_statement(
                        session,
                        &contract,
                        &expected_policies,
                        statement,
                    )
                    .await?
            }
            None => {
                self.keymeld
                    .bind_payout_contract(session, &contract, &expected_policies)
                    .await?
            }
        };
        let expected_set =
            coordinator_escrow::payout_protocol::accepted_policy_set_digest(&signed_digests)?;
        let expected_contract = coordinator_escrow::payout::contract_digest(&contract)?;
        // A pool's policies name its queue's deposit scope; each command names the pool's session.
        let (scope_session_id, _) = session.registration_scope()?;
        let mut seen = std::collections::BTreeSet::new();
        for binding in &bindings {
            let registration = registrations
                .get(&binding.user_id)
                .ok_or_else(|| anyhow!("Binding names an unexpected participant"))?;
            let key = session
                .recipient_authorization
                .recipient_public_keys
                .get(&registration.enclave_id)
                .ok_or_else(|| anyhow!("Binding enclave has no pinned recipient key"))?;
            binding.verify(key)?;
            if !seen.insert(binding.user_id.clone())
                || binding.keygen_session_id != scope_session_id
                || binding.response.context.request.session_id() != &session.session_id
                || binding.enclave_id != registration.enclave_id
                || binding.enclave_key_epoch != registration.enclave_key_epoch
                || binding.policy_set_digest != expected_set
                || binding.contract_digest != expected_contract
                || binding.response.context.request.policy_digest
                    != signed_digests[&binding.user_id]
            {
                return Err(anyhow!(
                    "Binding differs from the complete accepted participant policy set"
                ));
            }
        }
        if seen != expected_policies.keys().cloned().collect() || seen.is_empty() {
            return Err(anyhow!(
                "Every participant requires its own authenticated contract binding"
            ));
        }
        self.competition_store
            .store_payout_contract_binding(competition.id, serde_json::to_string(&bindings)?)
            .await?;
        Ok(())
    }

    /// Runs independently of browser sessions and of the Lightning sender.
    pub async fn automatic_payout_tick(&self) -> Result<(), anyhow::Error> {
        self.competition_store
            .index_existing_payment_hashes()
            .await?;
        for competition in self.competition_store.get_competitions(false).await? {
            if let Err(error) = self.queue_automatic_competition(&competition).await {
                warn!(
                    "Cannot discover payouts for competition {}: {}",
                    competition.id, error
                );
            }
        }
        // Include paid claims from completed competitions: escrow release is
        // recovery, and must remain possible after the invoice has expired.
        for job in self.competition_store.due_payout_jobs().await? {
            if let Err(error) = self.process_payout_job(&job).await {
                warn!("Payout job {} will retry: {}", job.id, error);
                self.competition_store
                    .retry_payout_job(job.id, job.attempts, error.to_string())
                    .await?;
            }
        }
        Ok(())
    }

    async fn queue_automatic_competition(
        &self,
        competition: &Competition,
    ) -> Result<(), anyhow::Error> {
        if competition.kind == crate::domain::competitions::CompetitionKind::Queued
            || (competition.attestation.is_none() && !competition.settled_by_expiry())
            || competition.signed_contract.is_none()
            || competition.funding_confirmed_at.is_none()
            || competition.delta_broadcasted_at.is_some()
            || competition.cancelled_at.is_some()
            || competition.completed_at.is_some()
            || competition.failed_at.is_some()
            || self
                .competition_store
                .payout_window_is_closed(competition.id)
                .await?
            || !self
                .competition_store
                .has_automatic_payouts(competition.id)
                .await?
        {
            return Ok(());
        }
        let outcome = competition.get_current_outcome()?;
        let params = competition
            .contract_parameters
            .as_ref()
            .ok_or_else(|| anyhow!("Attested competition has no contract"))?;
        for entry in self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await?
        {
            if entry.paid_out_at.is_some()
                || self.competition_store.has_live_payout_job(entry.id).await?
            {
                continue;
            }
            let Some(json) = self.competition_store.entry_payout_policy(entry.id).await? else {
                continue;
            };
            let policy: PayoutPolicy = serde_json::from_str(&json)?;
            if policy.automatic_lightning_address.is_none()
                || winner_payout_sats(params, &outcome, &entry.ephemeral_pubkey.parse()?).is_err()
            {
                continue;
            }
            // Only the unique insert winner may create the durable intent.
            if let Err(e) = self
                .competition_store
                .create_payout_job(entry.id, serde_json::to_string(&PayoutMethod::Automatic)?)
                .await
            {
                if !matches!(&e, DatabaseWriteError::Sqlx(sqlx::Error::Database(db)) if db.is_unique_violation())
                {
                    warn!("Cannot queue payout for entry {}: {}", entry.id, e);
                }
            }
        }
        Ok(())
    }

    async fn process_payout_job(&self, job: &PayoutJob) -> Result<(), anyhow::Error> {
        let entry = self
            .competition_store
            .get_entry_by_id(job.entry_id)
            .await?
            .ok_or_else(|| anyhow!("Payout entry disappeared"))?;
        let competition = self
            .competition_store
            .get_competition(entry.event_id)
            .await?;
        let stored = self
            .competition_store
            .get_keymeld_session(entry.event_id)
            .await?
            .ok_or_else(|| anyhow!("Missing Keymeld session"))?;
        let session = self.restore_keymeld_session(&stored)?;
        let user_id = UserId::from(entry.ticket_id);
        if let Some(payout_id) = job.payout_id {
            let mut payout = self
                .competition_store
                .get_payout(payout_id)
                .await?
                .ok_or_else(|| anyhow!("Payout outbox item disappeared"))?;
            if payout.failed_at.is_some() {
                self.competition_store.fail_payout_job(job.id).await?;
                return Ok(());
            }
            if payout.succeed_at.is_none() {
                self.competition_store.schedule_payout_poll(job.id).await?;
                return Ok(());
            }
            if payout.payment_preimage.is_none() {
                let hash = crate::infra::lightning::extract_payment_hash_from_invoice(
                    &payout.payout_payment_request,
                )?;
                let settled = self.ln.lookup_payment(&hash).await?;
                if settled.status != PaymentStatus::Succeeded {
                    return Err(anyhow!("Awaiting settled payment proof"));
                }
                let proof = settled
                    .payment_preimage
                    .ok_or_else(|| anyhow!("LND has not returned a payment proof"))?;
                self.competition_store
                    .mark_payout_succeeded(
                        payout_id,
                        OffsetDateTime::now_utc(),
                        Some(proof.clone()),
                    )
                    .await?;
                payout.payment_preimage = Some(proof);
            }
            let prepared: PayoutPreparedResponse = serde_json::from_str(
                job.prepared_json
                    .as_deref()
                    .ok_or_else(|| anyhow!("Missing prepared claim receipt"))?,
            )?;
            let hash = crate::infra::lightning::extract_payment_hash_from_invoice(
                &payout.payout_payment_request,
            )?;
            let proof = verify_payout_preimage(
                payout
                    .payment_preimage
                    .as_deref()
                    .expect("payment proof recovered"),
                &hash,
            )?;
            let secrets = self
                .keymeld
                .release_payout(
                    &session,
                    user_id,
                    ReleasePayoutRequest {
                        claim_id: job.id,
                        state_receipt: prepared.state_receipt,
                        payment_preimage: hex::encode(proof),
                    },
                )
                .await?;
            self.competition_store
                .complete_payout_job(
                    job.id,
                    secrets.entry_private_key.clone(),
                    secrets.payout_preimage.clone(),
                )
                .await?;
            return Ok(());
        }
        if competition.delta_broadcasted_at.is_some()
            || competition.completed_at.is_some()
            || competition.cancelled_at.is_some()
        {
            return Err(anyhow!(
                "Competition has entered on-chain settlement; payout preparation is stopped"
            ));
        }
        let bindings: Vec<PayoutContractBoundResponse> = serde_json::from_str(
            &self
                .competition_store
                .payout_contract_binding(entry.event_id)
                .await?
                .ok_or_else(|| anyhow!("Contract was not bound for payout"))?,
        )?;
        let enclave_id = entry
            .keymeld_registration_context
            .as_ref()
            .ok_or_else(|| anyhow!("Missing participant context"))?
            .enclave_id;
        let binding = bindings
            .into_iter()
            .find(|binding| binding.enclave_id == enclave_id && binding.user_id == user_id)
            .ok_or_else(|| anyhow!("Missing participant enclave binding"))?;
        let contract = competition
            .signed_contract
            .as_ref()
            .ok_or_else(|| anyhow!("Competition not signed"))?;
        let outcome = competition.get_current_outcome()?;
        // The verifier settles an expiry refund on the expiry outcome when no attestation is
        // sent, and only once the contract has expired.
        let attestation = match outcome {
            Outcome::Expiry => None,
            Outcome::Attestation(_) => Some(
                competition
                    .attestation
                    .ok_or_else(|| anyhow!("Oracle has not attested"))?,
            ),
        };
        let params = competition
            .contract_parameters
            .as_ref()
            .ok_or_else(|| anyhow!("Missing contract parameters"))?;
        let owed = winner_payout_sats(params, &outcome, &entry.ephemeral_pubkey.parse()?)?;
        let ark_funding = self
            .competition_store
            .ark_commitment(entry.event_id)
            .await?
            .map(|done| coordinator_escrow::ark::ArkFunding {
                commitment_tx: done.commitment_tx,
                vout: done.funding_vout,
            });
        let prepared = self
            .keymeld
            .prepare_payout(
                &session,
                user_id.clone(),
                PreparePayoutRequest {
                    claim_id: job.id,
                    binding_receipt: binding.binding_receipt,
                    contract_signatures: serde_json::to_string(contract.all_signatures())?,
                    attestation: attestation.map(|scalar| hex::encode(scalar.serialize())),
                    method: serde_json::from_str(&job.request_json)?,
                    ark_funding,
                },
            )
            .await?;
        if prepared.claim_id != job.id || prepared.user_id != user_id || prepared.owed_sats != owed
        {
            return Err(anyhow!("Prepared payout does not match the durable claim"));
        }
        // The enclave may return a cached claim after its invoice expired while
        // this process was offline. Persist its authenticated receipt and hash
        // so the sender can reconcile any payment, or conclusively retire an
        // expired unsent invoice before a new claim is created.
        coordinator_escrow::payout::validate_prepared_invoice(
            &prepared.invoice,
            owed,
            self.bitcoin.get_network(),
        )?;
        self.competition_store
            .store_prepared_payout(
                job.id,
                prepared.invoice.clone(),
                owed,
                serde_json::to_string(&prepared)?,
            )
            .await?;
        Ok(())
    }
}

/// Ticket UUID order is also NOAA's oracle entry order for new competitions.
fn slot_payouts(players: usize, places: usize) -> Result<BTreeMap<Outcome, PayoutWeights>, Error> {
    if players == 0 || players > 100 || places == 0 || places > players {
        return Err(Error::BadRequest(
            "Invalid payout player or winner count".into(),
        ));
    }
    // Weights are ratios, not percentages. Equal stakes must have exactly equal shares.
    let equal: PayoutWeights = (0..players).map(|i| (i, 1)).collect();
    let percentages = get_percentage_weights(places);
    let mut payouts = BTreeMap::new();
    for (index, winners) in generate_ranking_permutations(players, places)
        .into_iter()
        .enumerate()
    {
        let weights = if winners.len() == players {
            equal.clone()
        } else {
            winners
                .into_iter()
                .enumerate()
                .map(|(rank, player)| (player, percentages[rank]))
                .collect()
        };
        payouts.insert(Outcome::Attestation(index), weights);
    }
    payouts.insert(Outcome::Expiry, equal);
    Ok(payouts)
}

#[cfg(test)]
#[path = "automatic_tests.rs"]
mod tests;

#[cfg(test)]
mod enrollment_validation_tests {
    use super::*;

    #[test]
    fn malformed_payout_choice_is_rejected_before_reserving_a_ticket() {
        let key = BitcoinPublicKey::new(
            bitcoin::secp256k1::SecretKey::from_slice(&[3; 32])
                .unwrap()
                .public_key(&bitcoin::secp256k1::Secp256k1::new()),
        );
        let choice = PayoutRegistrationRequest {
            entry_id: Uuid::now_v7(),
            payout_hash: hex::encode([4; 32]),
            lightning_address: Some("winner@example.org".into()),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
        };
        Coordinator::validate_ticket_payout_choice(&key, &choice).unwrap();
        for mutate in [
            |c: &mut PayoutRegistrationRequest| c.entry_id = Uuid::nil(),
            |c: &mut PayoutRegistrationRequest| c.payout_hash = "not-a-hash".into(),
            |c: &mut PayoutRegistrationRequest| c.lightning_address = Some("invalid".into()),
            |c: &mut PayoutRegistrationRequest| c.release_entry_key_after_payment = false,
            |c: &mut PayoutRegistrationRequest| {
                c.lightning_address = None;
                c.allow_invoice_fallback = false;
            },
        ] {
            let mut invalid = choice.clone();
            mutate(&mut invalid);
            assert!(Coordinator::validate_ticket_payout_choice(&key, &invalid).is_err());
        }
        assert!(Coordinator::validate_ticket_payout_choice(
            &BitcoinPublicKey::new_uncompressed(key.inner),
            &choice,
        )
        .is_err());
    }
}
