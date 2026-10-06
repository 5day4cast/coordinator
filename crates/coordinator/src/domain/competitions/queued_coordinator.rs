//! The Coordinator's side of a queued competition's registration, and what its pools do
//! differently from a single competition.
//!
//! Registration: the admin creates the competition with its pool rules; its reference oracle
//! event freezes the lines, and its `QueuedTerms` are what every player consents to. A ticket is
//! made when a player asks for one, with the id of the entry it pays for. The player's browser
//! seals the entry key to a Keymeld enclave under the terms' deposit scope instead of a session,
//! and the coordinator has the enclave check that deposit before showing the invoice.
//!
//! Pools: each creates its oracle event from the reference event's lines, pays the places its
//! size gives from `queued::pool_payouts`, and gives Keymeld the oracle's signed statement of its event when it
//! binds the contract. See `queued.rs` and `queued_kickoff.rs`.

use super::*;
use crate::domain::competitions::{
    admission,
    queued::{self, CompetitionKind, CreateQueuedCompetition, PoolSummary, QueueSummary},
    queued_store::{QueueSettings, QueuedReservation},
};
use crate::infra::keymeld::DepositScopeRequest;
use coordinator_core::PayoutRegistrationRequest;
use coordinator_escrow::{
    authorization::PayoutPolicy,
    oracle_statement::SignedStatement,
    queued::{DepositEvidence, EntryConsent, QueuedEntryTerms},
};

impl Coordinator {
    /// Create a queued competition: its reference oracle event, which freezes the lines players
    /// pick against, and the terms every player consents to. No Keymeld session and no tickets
    /// exist until players ask for them.
    pub async fn create_queued_competition(
        &self,
        request: CreateQueuedCompetition,
    ) -> Result<Competition, Error> {
        if !self.automatic_payouts || self.ark.is_none() || !self.is_keymeld_enabled() {
            return Err(Error::BadRequest(
                "Queued competitions need Keymeld, automatic payouts and Arkade".into(),
            ));
        }
        let now = OffsetDateTime::now_utc();
        let (pool_rules, max_entries) = request.settings().map_err(Error::BadRequest)?;
        request.check_registration(now).map_err(Error::BadRequest)?;
        let create_event = request.reference_event().map_err(Error::BadRequest)?;
        create_event
            .validate_oracle_settings()
            .map_err(|reason| Error::BadRequest(reason.into()))?;
        // The largest pool pays the most places, so it is the largest contract to sign.
        coordinator_escrow::capacity::validate_competition_capacity(
            create_event.total_allowed_entries,
            request.largest_pool_places(),
        )
        .map_err(|reason| Error::BadRequest(reason.to_string()))?;
        let competition = Competition::new(&create_event);
        if now >= competition.ticket_deadline() {
            return Err(Error::BadRequest(admission::TICKETS_CLOSED.into()));
        }
        self.require_payout_capabilities(true).await?;

        let oracle_key = self.oracle_client.public_key().await?;
        let event = self
            .oracle_client
            .create_event(create_event.clone())
            .await?;
        let reference = self.oracle_client.get_event_terms(&create_event.id).await?;
        if reference.event.nonce_point != event.nonce_point
            || reference.event.event_announcement != event.event_announcement
        {
            return Err(Error::BadRequest(
                "The oracle reports a different reference event than it created".into(),
            ));
        }
        let terms = queued::build_terms(
            queued::TermsInputs {
                competition_id: create_event.id,
                network: self.bitcoin.get_network(),
                market_maker: self.public_key,
                oracle_key,
                pool_rules,
                stake_sats: create_event.entry_fee as u64,
                relative_locktime_block_delta: create_event
                    .relative_locktime_block_delta
                    .unwrap_or(self.relative_locktime_block_delta as u16),
                max_fee_rate: self.automatic_payout_max_fee_rate,
            },
            &reference,
        )
        .map_err(Error::BadRequest)?;
        let settings = QueueSettings {
            competition_id: create_event.id,
            pool_rules,
            stake_sats: terms.stake_sats,
            max_entries,
            terms_digest: terms
                .digest()
                .map_err(|e| Error::BadRequest(e.to_string()))?,
            terms,
        };

        let mut competition = competition;
        competition.kind = CompetitionKind::Queued;
        competition.event_announcement = Some(event.event_announcement);
        self.competition_store
            .add_queued_competition(&competition, &settings)
            .await
            .map_err(|e| {
                error!(
                    "Queued competition {} has its oracle event but was not saved: {e}",
                    competition.id
                );
                Error::from(e)
            })?;
        competition.queue = Some(queue_summary(&settings, 0, vec![]));
        info!(
            "Created queued competition {} with pools of {} to {} players",
            competition.id,
            pool_rules.min_players(),
            pool_rules.max_players()
        );
        self.wake_competition(competition.id);
        Ok(competition)
    }

    /// A queued competition's settings; an error for any other competition.
    pub(super) async fn queue_settings(
        &self,
        competition_id: Uuid,
    ) -> Result<QueueSettings, Error> {
        self.competition_store
            .queue_settings(competition_id)
            .await?
            .ok_or_else(|| {
                Error::BadRequest(format!(
                    "Competition {competition_id} is not a queued competition"
                ))
            })
    }

    /// Fill in each queued competition's settings, entries and pools, for the API.
    pub(super) async fn attach_queue_details(
        &self,
        competitions: &mut [Competition],
    ) -> Result<(), Error> {
        for competition in competitions
            .iter_mut()
            .filter(|competition| competition.kind == CompetitionKind::Queued)
        {
            let Some(settings) = self
                .competition_store
                .queue_settings(competition.id)
                .await?
            else {
                continue;
            };
            let entries = self
                .competition_store
                .queued_entry_count(competition.id)
                .await?;
            let held = self
                .competition_store
                .queued_held_count(competition.id)
                .await?;
            let pools = self
                .competition_store
                .competition_pools(competition.id)
                .await?
                .into_iter()
                .map(|pool| PoolSummary {
                    competition_id: pool.competition_id,
                    pool_index: pool.pool_index,
                    players: pool.members.len(),
                })
                .collect();
            let mut summary = queue_summary(&settings, entries, pools);
            summary.held = held;
            competition.queue = Some(summary);
        }
        Ok(())
    }

    /// A ticket for a queued competition, made on demand.
    ///
    /// Its id is the id of the entry it pays for, which the player's payout choice names: the
    /// oracle ranks a pool's entries by id, and Keymeld binds the deposit to it. The payout
    /// policy consents to the competition's terms, not to a contract, and the Keymeld assignment
    /// names the deposit scope instead of a session.
    pub(super) async fn request_queued_ticket(
        &self,
        pubkey: String,
        competition: Competition,
        btc_pubkey: BitcoinPublicKey,
        payout: Option<PayoutRegistrationRequest>,
    ) -> Result<TicketResponse, Error> {
        let now = OffsetDateTime::now_utc();
        competition.require_ticket_admission(now)?;
        let choice = payout.ok_or_else(|| {
            Error::BadRequest(
                "This competition requires payout authorization before ticket payment; update \
                 your client"
                    .into(),
            )
        })?;
        Self::validate_ticket_payout_choice(&btc_pubkey, &choice)?;
        if choice.lightning_address.is_none() {
            return Err(Error::BadRequest(
                "A queued competition refunds to your Lightning Address if it does not start; \
                 add one before entering"
                    .into(),
            ));
        }
        queued::check_entry_id(choice.entry_id, now).map_err(Error::BadRequest)?;
        self.require_payout_capabilities(true).await?;
        let settings = self.queue_settings(competition.id).await?;
        let reserved =
            match self
                .competition_store
                .reserve_queued_ticket(
                    competition.id,
                    choice.entry_id,
                    &pubkey,
                    settings.max_entries,
                    competition.event_submission.max_entries_per_player,
                    competition.ticket_deadline(),
                )
                .await?
            {
                QueuedReservation::Reserved(reserved) => reserved,
                QueuedReservation::Closed => {
                    return Err(Error::BadRequest(admission::TICKETS_CLOSED.into()))
                }
                QueuedReservation::Full => return Err(Error::CompetitionFull),
                QueuedReservation::TooManyUnpaid => return Err(Error::BadRequest(
                    "You already hold unpaid tickets for this competition; pay one or wait for \
                     its invoice to expire"
                        .into(),
                )),
                QueuedReservation::EntryLimit => {
                    return Err(super::entry_limit_error(
                        competition.event_submission.max_entries_per_player,
                    ))
                }
                QueuedReservation::Taken => {
                    return Err(Error::BadRequest(
                        "This entry id is taken; start the entry again".into(),
                    ))
                }
            };
        let ticket = reserved.ticket;
        if let Some(old_hash) = reserved.superseded_payment_hash {
            self.cancel_superseded_invoice(ticket.id, old_hash).await;
        }
        let result = async {
            competition.require_ticket_admission(OffsetDateTime::now_utc())?;
            self.prepare_queued_ticket_policy(
                &competition,
                &settings,
                &ticket,
                &btc_pubkey,
                &choice,
            )
            .await?;
            self.create_ticket_response(ticket.clone(), btc_pubkey, competition)
                .await
        }
        .await;
        if result.is_err() {
            self.release_failed_reservation(&ticket).await;
        }
        result
    }

    /// Fix a queued ticket's payout policy: the player's consent to the competition's terms and
    /// their own entry, held in the ticket's Arkade escrow. Its refund time is capped at the
    /// terms' expiry, like any escrow's at its contract's.
    async fn prepare_queued_ticket_policy(
        &self,
        competition: &Competition,
        settings: &QueueSettings,
        ticket: &Ticket,
        entry_pubkey: &BitcoinPublicKey,
        choice: &PayoutRegistrationRequest,
    ) -> Result<(), Error> {
        if choice.entry_id != ticket.id {
            return Err(Error::BadRequest(
                "A queued entry's id is its ticket's id".into(),
            ));
        }
        let address = choice
            .lightning_address
            .as_deref()
            .map(LightningAddress::parse)
            .transpose()
            .map_err(|e| Error::BadRequest(e.to_string()))?
            .map(|address| address.to_string());
        let ark_escrow = self
            .ticket_ark_escrow_policy(competition, ticket, entry_pubkey)
            .await?
            .ok_or_else(|| {
                Error::BadRequest("A queued competition holds buy-ins in Arkade escrows".into())
            })?;
        let entry = QueuedEntryTerms {
            terms: settings.terms.clone(),
            entry_id: ticket.id,
            ticket_hash: parse_hash32(&ticket.hash).map_err(Error::Bitcoin)?,
            payout_hash: parse_hash32(&choice.payout_hash).map_err(Error::Bitcoin)?,
        };
        let policy = PayoutPolicy {
            automatic_lightning_address: address,
            allow_invoice_fallback: choice.allow_invoice_fallback,
            release_entry_key_after_payment: choice.release_entry_key_after_payment,
            contract_terms: String::new(),
            ark_escrow: Some(ark_escrow),
            queued_entry: Some(
                entry
                    .to_json()
                    .map_err(|e| Error::BadRequest(e.to_string()))?,
            ),
        };
        if QueuedEntryTerms::from_policy(&policy).map_err(|e| Error::BadRequest(e.to_string()))?
            != Some(entry)
        {
            return Err(Error::BadRequest(
                "The queued entry's policy does not round-trip".into(),
            ));
        }
        self.fix_ticket_payout_policy(ticket, entry_pubkey, &policy)
            .await
    }

    /// Where the player's browser deposits the entry key for a queued ticket: the terms' deposit
    /// scope in place of a session and manifest, and an enclave spread by ticket.
    pub(super) async fn queued_registration_assignment(
        &self,
        competition: &Competition,
        ticket_id: Uuid,
    ) -> Result<RegistrationAssignment, Error> {
        let settings = self.queue_settings(competition.id).await?;
        let (session_id, digest) = coordinator_escrow::queued::deposit_scope(&settings.terms)
            .map_err(|e| Error::BadRequest(e.to_string()))?;
        self.keymeld
            .deposit_assignment(session_id, digest, UserId::from(ticket_id))
            .await
            .map_err(|error| Error::Bitcoin(anyhow!(error)))
    }

    /// The deposit scope of a queued competition's registrations, with `evidence`.
    pub(super) fn deposit_scope_request(
        settings: &QueueSettings,
        evidence: &DepositEvidence,
    ) -> Result<DepositScopeRequest, Error> {
        let (deposit_session_id, deposit_digest) =
            coordinator_escrow::queued::deposit_scope(&settings.terms)
                .map_err(|e| Error::BadRequest(e.to_string()))?;
        Ok(DepositScopeRequest {
            deposit_session_id,
            deposit_digest,
            evidence: evidence
                .encode()
                .map_err(|e| Error::BadRequest(e.to_string()))?,
        })
    }

    /// Check a queued ticket's key deposit before its invoice is shown: its context names the
    /// terms' deposit scope, this ticket and its key, and its policy consents to these terms
    /// for this ticket. Then the enclave it was sealed to checks the sealed envelope, under a
    /// manifest made only for that check.
    pub(super) async fn validate_queued_deposit(
        &self,
        competition: &Competition,
        ticket_id: Uuid,
        data: &ParticipantRegistrationData,
    ) -> Result<(), Error> {
        let settings = self.queue_settings(competition.id).await?;
        check_queued_registration(&settings, ticket_id, data)?;
        let scope = Self::deposit_scope_request(
            &settings,
            &DepositEvidence::Refund {
                competition_id: competition.id,
            },
        )?;
        self.keymeld
            .validate_deposit(scope, UserId::from(ticket_id), data)
            .await
            .map_err(|error| {
                warn!("Keymeld refused the key deposit of queued ticket {ticket_id}: {error}");
                Error::BadRequest("Keymeld refused the key deposit".into())
            })
    }

    /// A queued entry must carry the deposit its ticket was paid with, unchanged: that deposit is
    /// what its pool registers, and what its escrow is refunded with.
    pub(super) async fn check_queued_entry_registration(
        &self,
        competition_id: Uuid,
        ticket: &Ticket,
        entry: &AddEntry,
        data: &ParticipantRegistrationData,
    ) -> Result<(), Error> {
        if entry.id != ticket.id {
            return Err(Error::BadRequest(
                "A queued entry's id is its ticket's id".into(),
            ));
        }
        let settings = self.queue_settings(competition_id).await?;
        check_queued_registration(&settings, ticket.id, data)?;
        let stored = self
            .competition_store
            .ticket_registration(ticket.id, &ticket.hash)
            .await?
            .ok_or_else(|| {
                Error::BadRequest(
                    "The ticket's key deposit was never sent; a queued entry needs it".into(),
                )
            })?;
        let stored: TicketRegistration =
            serde_json::from_str(&stored).map_err(|e| Error::Bitcoin(e.into()))?;
        let sent = TicketRegistration {
            ephemeral_pubkey: entry.ephemeral_pubkey.clone(),
            encrypted_keymeld_private_key: data.encrypted_private_key.clone(),
            keymeld_auth_pubkey: data.auth_pubkey.clone(),
            keymeld_registration_context: data.context.clone(),
            keymeld_escrow_policy: data.escrow_policy.clone(),
        };
        if !stored.same_as(&sent) {
            return Err(Error::BadRequest(
                "The entry's Keymeld registration differs from the one sent for its ticket \
                 before paying"
                    .into(),
            ));
        }
        Ok(())
    }

    /// A pool's queued competition's settings, and how the pool was formed; `None` for any other
    /// competition.
    pub(super) async fn pool_of(
        &self,
        competition: &Competition,
    ) -> Result<Option<(QueueSettings, super::super::queued_store::PoolRecord)>, anyhow::Error>
    {
        if competition.kind != CompetitionKind::Pool {
            return Ok(None);
        }
        let record = self
            .competition_store
            .pool_record(competition.id)
            .await?
            .ok_or_else(|| anyhow!("Pool {} has no formation record", competition.id))?;
        let parent = competition
            .parent_id
            .filter(|parent| *parent == record.parent_id)
            .ok_or_else(|| anyhow!("Pool {} names another queued competition", competition.id))?;
        let settings = self
            .competition_store
            .queue_settings(parent)
            .await?
            .ok_or_else(|| anyhow!("Pool {}'s queued competition has no terms", competition.id))?;
        Ok(Some((settings, record)))
    }

    /// Create a pool's oracle event from its queued competition's reference event, copying the
    /// lines that event froze. An oracle that ignored the request froze its own lines, so the
    /// event is checked against the terms players consented to, and refused if it differs.
    ///
    /// The pool's id is its event's, so an event created before a restart is found and checked
    /// rather than created twice.
    pub(super) async fn create_pool_event(
        &self,
        competition: &Competition,
        settings: &QueueSettings,
    ) -> Result<Event, anyhow::Error> {
        let event = match self.oracle_client.get_event_terms(&competition.id).await {
            Ok(existing) => existing.event,
            Err(OracleError::NotFound(_)) => match self
                .oracle_client
                .create_event_from_lines(
                    competition.event_submission.clone(),
                    settings.competition_id,
                )
                .await
            {
                Ok(event) => event,
                Err(OracleError::NotFound(e)) => return Err(Error::NotFound(e).into()),
                Err(OracleError::BadRequest(e)) => return Err(Error::BadRequest(e).into()),
                Err(e) => return Err(Error::OracleFailed(e).into()),
            },
            Err(e) => return Err(Error::OracleFailed(e).into()),
        };
        let created = self.oracle_client.get_event_terms(&competition.id).await?;
        check_pool_event(competition, settings, &event, &created)?;
        Ok(event)
    }

    /// What a pool needs before its lifecycle can run: its Keymeld session and its oracle event.
    /// Both are made after the pool formed, so a pool that cannot get them in time fails, and
    /// its escrows are refunded; until then each step tries again.
    pub(super) async fn prepare_pool(
        &self,
        competition: &mut Competition,
    ) -> Result<(), anyhow::Error> {
        self.ensure_pool_session(competition).await?;
        if competition.event_announcement.is_none() {
            let (settings, _) = self
                .pool_of(competition)
                .await?
                .ok_or_else(|| anyhow!("Competition {} is not a pool", competition.id))?;
            let event = self.create_pool_event(competition, &settings).await?;
            competition.event_announcement = Some(event.event_announcement);
        }
        Ok(())
    }

    /// The oracle's signed statement of a pool's event, checked against what the coordinator
    /// already trusts: the terms, the pool's members, and the event's announced locking points.
    pub(super) async fn pool_statement(
        &self,
        competition: &Competition,
        settings: &QueueSettings,
        members: &[Uuid],
    ) -> Result<SignedStatement, anyhow::Error> {
        let event = self.oracle_client.get_event_terms(&competition.id).await?;
        let statement = event.statement.ok_or_else(|| {
            anyhow!(
                "The oracle has no signed statement of pool {} yet; it needs oracle 2.4.0 and \
                 every entry",
                competition.id
            )
        })?;
        check_pool_statement(competition, settings, members, &statement)?;
        Ok(statement)
    }

    /// A pool's payout table: its players consented to `queued::pool_payouts` for whatever pool
    /// they were placed in. The entries must be exactly the pool's members, and each a queued
    /// entry of this pool's competition, with its terms exactly.
    pub(super) async fn accepted_pool_payouts(
        &self,
        competition: &Competition,
        settings: &QueueSettings,
        members: &[Uuid],
        entries: &[UserEntry],
    ) -> Result<BTreeMap<Outcome, PayoutWeights>, anyhow::Error> {
        let mut roster: Vec<Uuid> = entries.iter().map(|entry| entry.ticket_id).collect();
        roster.sort_unstable();
        if entries.len() != competition.event_submission.total_allowed_entries || roster != members
        {
            return Err(anyhow!(
                "Pool {} has {} entries, not its {} members",
                competition.id,
                entries.len(),
                competition.event_submission.total_allowed_entries
            ));
        }
        if Some(competition.event_submission.total_competition_pool as u64)
            != settings.stake_sats.checked_mul(entries.len() as u64)
        {
            return Err(anyhow!(
                "Pool {}'s funding value is not its players' stakes",
                competition.id
            ));
        }
        for entry in entries {
            let json = self
                .competition_store
                .entry_payout_policy(entry.id)
                .await?
                .ok_or_else(|| anyhow!("Entry {} has no accepted payout policy", entry.id))?;
            let policy: PayoutPolicy = serde_json::from_str(&json)?;
            let consent = QueuedEntryTerms::from_policy(&policy)?.ok_or_else(|| {
                anyhow!("Entry {} did not consent to a queued competition", entry.id)
            })?;
            if consent.terms != settings.terms
                || consent.terms.digest()? != settings.terms_digest
                || consent.entry_id != entry.id
                || entry.ticket_id != entry.id
                || entry.event_id != competition.id
            {
                return Err(anyhow!(
                    "Entry {} consented to other terms than pool {}'s competition",
                    entry.id,
                    competition.id
                ));
            }
        }
        Ok(coordinator_escrow::queued::pool_payouts(
            entries.len(),
            settings.terms.pool_places(entries.len()) as usize,
        )?)
    }
}

fn queue_summary(settings: &QueueSettings, entries: u64, pools: Vec<PoolSummary>) -> QueueSummary {
    QueueSummary {
        pool_rules: settings.pool_rules,
        entries,
        max_entries: settings.max_entries,
        held: entries,
        stake_sats: settings.stake_sats,
        terms_digest: hex::encode(settings.terms_digest),
        pools,
    }
}

/// A queued ticket's registration names the terms' deposit scope, this ticket and its key, and
/// its payout policy consents to these terms for this ticket.
fn check_queued_registration(
    settings: &QueueSettings,
    ticket_id: Uuid,
    data: &ParticipantRegistrationData,
) -> Result<(), Error> {
    let (session_id, digest) = coordinator_escrow::queued::deposit_scope(&settings.terms)
        .map_err(|e| Error::BadRequest(e.to_string()))?;
    let context = &data.context;
    let key = hex::decode(&data.public_key)
        .map_err(|_| Error::BadRequest("Invalid registration key".into()))?;
    let auth = hex::decode(&data.auth_pubkey)
        .map_err(|_| Error::BadRequest("Invalid registration authentication key".into()))?;
    if context.keygen_session_id != session_id
        || context.manifest_hash != digest
        || context.user_id != UserId::from(ticket_id)
        || context.public_key != key
        || context.auth_pubkey != auth
        || context.require_signing_approval
    {
        return Err(Error::BadRequest(
            "The Keymeld registration is not a deposit for this ticket under the competition's \
             terms"
                .into(),
        ));
    }
    let policy = data
        .payout_policy
        .as_ref()
        .ok_or_else(|| Error::BadRequest("Missing pre-payment payout authorization".into()))?;
    match EntryConsent::from_policy(policy).map_err(|e| Error::BadRequest(e.to_string()))? {
        EntryConsent::Queued(entry)
            if entry.terms == settings.terms && entry.entry_id == ticket_id =>
        {
            Ok(())
        }
        _ => Err(Error::BadRequest(
            "The ticket's payout policy does not consent to this competition's terms".into(),
        )),
    }
}

/// A pool's oracle event must be the one its players consented to: the pool's own id and seat
/// count, the places its size pays, and the terms' signing date, expiry and observation terms, lines included.
fn check_pool_event(
    competition: &Competition,
    settings: &QueueSettings,
    created: &Event,
    event: &crate::infra::oracle::OracleEventTerms,
) -> Result<(), anyhow::Error> {
    let terms = &settings.terms;
    if created.id != competition.id
        || event.event.id != competition.id
        || event.event.nonce_point != created.nonce_point
        || event.event.event_announcement != created.event_announcement
    {
        return Err(anyhow!(
            "The oracle reports a different event for pool {}",
            competition.id
        ));
    }
    if event.total_allowed_entries != competition.event_submission.total_allowed_entries
        || event.number_of_places_win
            != terms.pool_places(competition.event_submission.total_allowed_entries)
        || event.signing_date.unix_timestamp() != terms.signing_date
        || created.event_announcement.expiry != Some(terms.expiry)
    {
        return Err(anyhow!(
            "Pool {}'s oracle event has another seat count, winner count, signing date or expiry \
             than its terms",
            competition.id
        ));
    }
    if !event.observation()?.same_as(&terms.observation) {
        return Err(anyhow!(
            "Pool {}'s oracle event froze other lines or terms than its queued competition's \
             reference event; the oracle may not copy lines yet (it needs 2.5.0)",
            competition.id
        ));
    }
    Ok(())
}

/// A pool's signed statement must be the oracle's, of this pool's event, with the terms, the
/// pool's members as entries, and the locking points the pool's event announced.
fn check_pool_statement(
    competition: &Competition,
    settings: &QueueSettings,
    members: &[Uuid],
    signed: &SignedStatement,
) -> Result<(), anyhow::Error> {
    use coordinator_escrow::oracle_statement::{Outcomes, Terms};
    let terms = &settings.terms;
    let oracle = terms.oracle_key()?;
    signed.verify(&oracle)?;
    let statement = &signed.statement;
    let announced = competition
        .event_announcement
        .as_ref()
        .ok_or_else(|| anyhow!("Pool {} has no oracle announcement", competition.id))?;
    let Outcomes::Ranking(ranking) = &statement.outcomes;
    let Terms::Observation(observation) = &statement.terms;
    let mut sorted = members.to_vec();
    sorted.sort_unstable();
    if statement.event_id != competition.id
        || statement.signing_date != terms.signing_date
        || statement.expiry != terms.expiry
        || announced.expiry != Some(statement.expiry)
        || ranking.number_of_places_win != terms.pool_places(sorted.len())
        || ranking.entry_ids != sorted
        || !observation.same_as(&terms.observation)
        || statement.locking_points(terms.oracle_point()?) != announced.locking_points
    {
        return Err(anyhow!(
            "The oracle's statement of pool {} differs from its event, entries or terms",
            competition.id
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "queued_coordinator_tests.rs"]
mod tests;
