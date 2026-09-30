//! The Coordinator's competition lifecycle, one step at a time, as `CompetitionRunners` drive it.
//! See `runners.rs`.

use super::*;
use crate::domain::competitions::{
    CompetitionSteps, CompetitionWakes, Lease, Pacing, Step, StepError, Wait, WorkerLeases,
};

/// Failed competitions stay visible for this long, then are cancelled so cleanup can run.
const FAILED_EXPIRY: time::Duration = time::Duration::hours(1);

impl Coordinator {
    /// Wake competitions' runners when their events happen.
    pub fn with_wakes(mut self, wakes: CompetitionWakes) -> Self {
        self.wakes = wakes;
        self
    }

    /// Name this process in worker leases, so singleton workers run in one coordinator at a time.
    pub fn with_lease_holder(mut self, holder: String, ttl: std::time::Duration) -> Self {
        self.worker_leases = Arc::new(WorkerLeases::new(
            self.competition_store.clone(),
            holder,
            ttl,
        ));
        self
    }

    /// Leases on the background workers that must run in one coordinator at a time.
    pub fn worker_leases(&self) -> &Arc<WorkerLeases> {
        &self.worker_leases
    }

    /// Run the competition's next step soon. Call it after committing the change it should see.
    pub fn wake_competition(&self, competition_id: Uuid) {
        self.wakes.wake(competition_id);
    }

    /// Save the competition only while this process still holds its lease.
    async fn save_leased(&self, competition: Competition, lease: &Lease) -> Result<(), StepError> {
        let id = competition.id;
        let saved = self
            .competition_store
            .update_competition_fenced(competition, lease)
            .await
            .map_err(|e| anyhow!("Failed to save competition {id}: {e}"))?;
        if saved {
            Ok(())
        } else {
            Err(StepError::LeaseLost)
        }
    }

    /// Advance one competition by one state.
    pub async fn advance_competition(
        &self,
        competition_id: Uuid,
        lease: &Lease,
        pacing: &Pacing,
    ) -> Result<Step, StepError> {
        let mut competition = self
            .competition_store
            .get_competition(competition_id)
            .await
            .map_err(|e| anyhow!("Failed to load competition {competition_id}: {e}"))?;
        let now = OffsetDateTime::now_utc();
        if competition.kind == crate::domain::competitions::CompetitionKind::Queued {
            // Registration, then pools: a queued competition runs no contract of its own.
            return self.advance_queued_competition(competition, lease).await;
        }

        if competition.resume_stranded_settlement() {
            warn!(
                "Resuming settlement of competition {competition_id}: its contract holds the pot \
                 on-chain, but it was stopped as failed or cancelled"
            );
        }
        if competition.is_cancelled() || competition.is_completed() {
            return Ok(Step::Finished);
        }
        if let Some(failed_at) = competition.failed_at {
            let cancel_at = failed_at + FAILED_EXPIRY;
            if now < cancel_at {
                return Ok(Step::Next(Wait::Until(cancel_at)));
            }
            competition.cancelled_at = Some(now);
            self.save_leased(competition, lease).await?;
            info!("Auto-cancelled failed competition {competition_id}");
            return Ok(Step::Finished);
        }
        if competition.unfilled_admission_expired(now) {
            let cancelled = self
                .competition_store
                .cancel_unfilled_at_deadline(&competition, lease)
                .await
                .map_err(|e| {
                    anyhow!("Failed to close unfilled competition {competition_id}: {e}")
                })?;
            if !cancelled {
                // The roster may have filled after our read. Reload before
                // advancing; do not release its invoices from a stale snapshot.
                return Ok(Step::Next(Wait::Now));
            }
            info!("Cancelled unfilled competition {competition_id} at entry close");
            self.release_held_invoices(competition_id).await;
            return Ok(Step::Finished);
        }
        if competition.is_expired_at(now) {
            competition.cancelled_at = Some(now);
            self.save_leased(competition, lease).await?;
            info!("Cancelled expired competition {competition_id}");
            self.release_held_invoices(competition_id).await;
            return Ok(Step::Finished);
        }
        if competition.kind == crate::domain::competitions::CompetitionKind::Pool
            && competition.event_created_at.is_none()
        {
            // A pool registers its players' deposits in a session made for it at kickoff, and
            // creates its oracle event from the queue's frozen lines. One that cannot get both
            // in time fails, and its escrows are refunded.
            if let Err(e) = self.prepare_pool(&mut competition).await {
                if now - competition.created_at < super::queued_kickoff::POOL_SETUP_DEADLINE {
                    return Err(StepError::Failed(e));
                }
                error!(
                    "Pool {competition_id} has no Keymeld session or oracle event and fails: {e:#}"
                );
                let failed_at = now;
                competition.failed_at = Some(failed_at);
                competition
                    .errors
                    .push(CompetitionError::FailedCreateTransaction(e.to_string()));
                self.save_leased(competition, lease).await?;
                return Ok(Step::Next(Wait::Until(failed_at + FAILED_EXPIRY)));
            }
        }
        self.renew_funding_reservation(&competition)
            .await
            .map_err(|e| anyhow!("Cannot reserve funding inputs: {e}"))?;

        let status: CompetitionStatus = competition.into();
        let before = status.state_name();
        let next = self.process_status(status).await;
        let moved_to = next.state_name();
        // Name and schedule the state the next step will load, which is derived from the stored
        // fields, so the log never reports a transition the database does not hold.
        let stored = CompetitionStatus::from(next.into_competition());
        let after = stored.state_name();
        if after != moved_to {
            // Every state a step moves to has stored fields that imply it. One that does not is
            // lost on reload, so the competition repeats the same step and cannot progress.
            if self
                .reported
                .is_new("unstored state", competition_id, moved_to)
            {
                warn!(
                    "Competition {competition_id} moved to {moved_to}, but its stored fields \
                     reload it as {after}, so it cannot progress"
                );
            } else {
                debug!("Competition {competition_id} moved to {moved_to} again, stored as {after}");
            }
        }
        let wait = if after != before && stored.is_immediate_transition() {
            Wait::Now
        } else {
            Wait::Until(stored.next_check(OffsetDateTime::now_utc(), pacing.idle))
        };
        let competition = stored.into_competition();
        let died = competition.is_failed() || competition.is_cancelled();
        let failed_at = competition.failed_at;
        self.save_leased(competition, lease).await?;
        if after != before {
            info!("Competition {competition_id} transitioned {before} -> {after}");
        } else {
            debug!("Competition {competition_id} is still {after}");
        }
        if died {
            self.release_held_invoices(competition_id).await;
            return Ok(match failed_at {
                Some(failed_at) => Step::Next(Wait::Until(failed_at + FAILED_EXPIRY)),
                None => Step::Finished,
            });
        }
        Ok(Step::Next(wait))
    }

    /// Cleanup has its own work queue: cancelled competitions are excluded from active lifecycle
    /// processing, but their invoices and time-locked escrows still need retries after outages
    /// or maturity.
    ///
    /// Then the Keymeld registrations players sent before paying are deleted once nothing will
    /// use them: those of released reservations, and those of competitions that ended with no
    /// refund left to sign.
    pub async fn clean_up_competitions(&self) -> Result<(), anyhow::Error> {
        for competition_id in self
            .competition_store
            .get_competitions_pending_cleanup(self.escrow_enabled)
            .await?
        {
            self.release_held_invoices(competition_id).await;
            self.reclaim_escrows(competition_id).await;
            self.refund_ark_escrows(competition_id).await;
        }
        let purged = self.competition_store.purge_ticket_registrations().await?;
        if purged > 0 {
            debug!("Deleted {purged} Keymeld registrations that nothing will use");
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl CompetitionSteps for Coordinator {
    async fn step(
        &self,
        competition_id: Uuid,
        lease: &Lease,
        pacing: &Pacing,
    ) -> Result<Step, StepError> {
        self.advance_competition(competition_id, lease, pacing)
            .await
    }

    async fn active_competitions(&self) -> Result<Vec<Uuid>, anyhow::Error> {
        Ok(self.competition_store.active_competition_ids().await?)
    }

    async fn clean_up(&self) -> Result<(), anyhow::Error> {
        self.clean_up_competitions().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::KeymeldSettings,
        infra::{
            bitcoin_mock::MockBitcoinClient,
            db::{DBConnection, DatabasePoolConfig, DatabaseType},
            keymeld::KeymeldService,
            lightning_mock::MockLnClient,
            lnurl_mock::MockLnurlPay,
            oracle_mock::MockOracle,
        },
    };
    use bitcoin::Network;

    fn parameters() -> ContractParameters {
        ContractParameters {
            market_maker: dlctix::MarketMaker {
                pubkey: Scalar::from_slice(&[9; 32]).unwrap().base_point_mul(),
            },
            players: [1, 3]
                .into_iter()
                .map(|key| Player {
                    pubkey: Scalar::from_slice(&[key; 32]).unwrap().base_point_mul(),
                    ticket_hash: dlctix::hashlock::sha256(&[key + 10; 32]),
                    payout_hash: dlctix::hashlock::sha256(&[key + 1; 32]),
                })
                .collect(),
            event: dlctix::EventLockingConditions {
                locking_points: vec![Scalar::from_slice(&[10; 32])
                    .unwrap()
                    .base_point_mul()
                    .into()],
                expiry: None,
            },
            outcome_payouts: BTreeMap::from([(
                Outcome::Attestation(0),
                PayoutWeights::from([(0, 100)]),
            )]),
            fee_rate: FeeRate::from_sat_per_vb_u32(1),
            funding_value: Amount::from_sat(100_000),
            relative_locktime_block_delta: 72,
        }
    }

    /// Without Keymeld nothing stores AwaitingSignatures, so a competition whose contract is
    /// built waits for its players' nonces as ContractCreated, the state it reloads as. It used
    /// to move to AwaitingSignatures in memory on every step, lose that on reload, and never
    /// sign.
    #[tokio::test]
    async fn a_legacy_contract_waits_for_nonces_in_the_state_it_reloads_as() {
        let directory = tempfile::tempdir().unwrap();
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database.clone()),
            Arc::new(MockBitcoinClient::new(Network::Regtest)),
            Arc::new(MockLnClient::new()),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            Arc::new(
                KeymeldService::new(KeymeldSettings::default(), Uuid::now_v7(), &[1; 32]).unwrap(),
            ),
            None,
            72,
            1,
            "legacy-signing-test".into(),
            false,
            1,
        )
        .await
        .unwrap();
        assert!(!coordinator.is_keymeld_enabled());

        let now = OffsetDateTime::now_utc();
        let mut competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: now + time::Duration::hours(3),
            start_observation_date: now + time::Duration::hours(1),
            end_observation_date: now + time::Duration::hours(2),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 2,
            entry_fee: 50_000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
            total_competition_pool: 100_000,
            relative_locktime_block_delta: Some(72),
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
        });
        competition.total_entries = 2;
        competition.total_paid_entries = 2;
        // One player has sent nonces; the other has not yet.
        competition.total_entry_nonces = 1;
        competition.contract_parameters = Some(parameters());
        competition.contracted_at = Some(now);
        competition.public_nonces = Some(SigMap {
            by_outcome: BTreeMap::new(),
            by_win_condition: BTreeMap::new(),
        });

        let status = CompetitionStatus::from(competition);
        assert_eq!(status.state_name(), "contract_created");
        let next = coordinator.process_status(status).await;
        assert_eq!(next.state_name(), "contract_created");
        assert_eq!(
            CompetitionStatus::from(next.into_competition()).state_name(),
            "contract_created",
            "the state a waiting legacy contract moves to is the one it reloads as"
        );
        database.close().await.unwrap();
    }

    async fn test_coordinator(directory: &std::path::Path) -> (Coordinator, DBConnection) {
        let database = DBConnection::new(
            directory.to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database.clone()),
            Arc::new(MockBitcoinClient::new(Network::Regtest)),
            Arc::new(MockLnClient::new()),
            Arc::new(MockLnurlPay::new(Network::Regtest)),
            Arc::new(
                KeymeldService::new(KeymeldSettings::default(), Uuid::now_v7(), &[1; 32]).unwrap(),
            ),
            None,
            72,
            1,
            "settlement-test".into(),
            false,
            1,
        )
        .await
        .unwrap();
        (coordinator, database)
    }

    /// A filled competition whose contract funding has confirmed, so the pot is on-chain.
    fn funded_competition() -> Competition {
        let now = OffsetDateTime::now_utc();
        let mut competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: now - time::Duration::hours(1),
            start_observation_date: now - time::Duration::hours(3),
            end_observation_date: now - time::Duration::hours(2),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 2,
            entry_fee: 50_000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
            total_competition_pool: 100_000,
            relative_locktime_block_delta: Some(72),
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
        });
        competition.total_entries = 2;
        competition.total_paid_entries = 2;
        competition.contract_parameters = Some(parameters());
        competition.contracted_at = Some(now - time::Duration::hours(4));
        competition.signed_at = Some(now - time::Duration::hours(4));
        competition.funding_broadcasted_at = Some(now - time::Duration::hours(4));
        competition.funding_confirmed_at = Some(now - time::Duration::hours(4));
        competition.funding_settled_at = Some(now - time::Duration::hours(4));
        competition.awaiting_attestation_at = Some(now - time::Duration::hours(4));
        competition
    }

    /// Settling a funded contract that errs (here: it has no signed contract to build the next
    /// transaction from) stays in its state to try again. It used to fail at the first error and
    /// be cancelled an hour later, leaving the pot in the contract's outputs.
    #[tokio::test]
    async fn settlement_errors_are_retried_instead_of_failing_the_competition() {
        let directory = tempfile::tempdir().unwrap();
        let (coordinator, database) = test_coordinator(directory.path()).await;
        let now = OffsetDateTime::now_utc();

        let mut outcome = funded_competition();
        outcome.outcome_broadcasted_at = Some(now);
        let mut delta = funded_competition();
        delta.outcome_broadcasted_at = Some(now);
        delta.delta_broadcasted_at = Some(now);

        for (competition, state) in [
            (outcome, "outcome_broadcasted"),
            (delta, "delta_broadcasted"),
        ] {
            let mut status = CompetitionStatus::from(competition);
            assert_eq!(status.state_name(), state);
            // More errors than a pre-funding step tolerates before it fails.
            for _ in 0..(KEPT_SETTLEMENT_ERRORS + 3) {
                status = coordinator.process_status(status).await;
                assert_eq!(
                    status.state_name(),
                    state,
                    "an error does not end settlement"
                );
            }
            let competition = status.into_competition();
            assert!(competition.failed_at.is_none());
            assert!(competition.cancelled_at.is_none());
            assert_eq!(
                competition.errors.len(),
                KEPT_SETTLEMENT_ERRORS,
                "the latest errors are kept, and the list stays bounded"
            );
            assert_eq!(
                CompetitionStatus::from(competition).state_name(),
                state,
                "it reloads in the state it retries from"
            );
        }
        database.close().await.unwrap();
    }

    /// An Arkade competition whose contract is built and whose keygen is done, so its next step
    /// runs the kickoff batch. Registration closed `closed_ago` before now.
    fn awaiting_kickoff(closed_ago: time::Duration) -> Competition {
        let now = OffsetDateTime::now_utc();
        let start = now - closed_ago;
        let mut competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + time::Duration::hours(3),
            start_observation_date: start,
            end_observation_date: start + time::Duration::hours(2),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 2,
            entry_fee: 50_000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
            total_competition_pool: 100_000,
            relative_locktime_block_delta: Some(72),
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
        });
        competition.total_entries = 2;
        competition.total_paid_entries = 2;
        competition.contract_parameters = Some(parameters());
        competition.contracted_at = Some(start);
        competition.keymeld_keygen_completed_at = Some(start);
        competition
    }

    /// The Arkade server failing every batch, as it did for thirteen minutes on 2026-09-30.
    fn server_failure() -> anyhow::Error {
        anyhow!("INTERNAL_ERROR (0): failed to create commitment tx: failed to estimate fee")
            .context("fund the pool in a batch")
    }

    #[test]
    fn a_kickoff_waits_as_long_as_a_failed_kickoff_check_would() {
        let hour = time::Duration::hours(1);
        let competition = awaiting_kickoff(time::Duration::minutes(10));
        let start = competition.event_submission.start_observation_date;
        assert_eq!(competition.kickoff_deadline(hour), start + hour);

        // Never past the point it expires waiting for signatures.
        assert_eq!(
            competition.kickoff_deadline(time::Duration::hours(5)),
            competition.contracted_at.unwrap() + time::Duration::hours(2)
        );
        let mut never_contracted = competition.clone();
        never_contracted.contracted_at = None;
        assert_eq!(never_contracted.kickoff_deadline(hour), start + hour);
    }

    /// Kickoff attempts run about a minute apart, so a competition used to fail after a few
    /// failed batches, minutes into an outage of the Arkade server, and refund every entry.
    #[test]
    fn failed_batches_before_the_deadline_leave_the_kickoff_retrying() {
        let now = OffsetDateTime::now_utc();
        let mut competition = awaiting_kickoff(time::Duration::minutes(10));
        // Earlier errors of other kinds still count toward aborting.
        competition.errors =
            vec![CompetitionError::FailedEscrowConfirmation("timed out".into()); 2];
        let deadline = now + time::Duration::minutes(50);
        let counted = crate::metrics::COMPETITION_STEP_FAILURES.get();

        let mut status = CompetitionStatus::from(competition);
        for minute in 0..12 {
            status = kickoff_failed(
                status,
                &server_failure(),
                deadline,
                now + time::Duration::minutes(minute),
            );
            assert_eq!(status.state_name(), "awaiting_signatures");
        }
        assert!(
            crate::metrics::COMPETITION_STEP_FAILURES.get() >= counted + 12,
            "each failed attempt is counted"
        );
        let competition = status.into_competition();
        assert!(competition.failed_at.is_none());
        assert_eq!(
            competition.errors.len(),
            2,
            "failed batches are not counted"
        );
        assert!(!competition.should_abort());
        assert_eq!(
            CompetitionStatus::from(competition).state_name(),
            "awaiting_signatures",
            "it reloads in the state it retries from"
        );
    }

    #[test]
    fn a_failed_batch_at_the_deadline_fails_the_competition_with_the_server_error() {
        let now = OffsetDateTime::now_utc();
        let status = CompetitionStatus::from(awaiting_kickoff(time::Duration::minutes(60)));
        let counted = crate::metrics::COMPETITION_STEP_FAILURES.get();

        let failed = kickoff_failed(status, &server_failure(), now, now);
        assert_eq!(failed.state_name(), "failed");
        assert!(crate::metrics::COMPETITION_STEP_FAILURES.get() > counted);
        let competition = failed.into_competition();
        assert!(competition.failed_at.is_some());
        let reason = competition.errors.last().unwrap().to_string();
        assert!(
            reason.contains("failed to estimate fee") && reason.contains("deadline"),
            "{reason}"
        );
    }

    #[test]
    fn other_errors_still_abort_after_six() {
        let mut competition = awaiting_kickoff(time::Duration::minutes(10));
        competition.errors = vec![CompetitionError::FailedFundingConfirmation("no tip".into()); 5];
        assert!(!competition.should_abort());
        competition
            .errors
            .push(CompetitionError::FailedFundingConfirmation("no tip".into()));
        assert!(competition.should_abort());
    }

    /// Through the step itself: a kickoff that cannot run (here Arkade is not configured) is
    /// retried until the deadline, and fails the competition after it.
    #[tokio::test]
    async fn the_kickoff_step_retries_until_its_deadline_and_then_fails() {
        let directory = tempfile::tempdir().unwrap();
        let (coordinator, database) = test_coordinator(directory.path()).await;
        let store = &coordinator.competition_store;

        let retrying = awaiting_kickoff(time::Duration::minutes(10));
        let late = awaiting_kickoff(time::Duration::minutes(90));
        let fee_wait = time::Duration::seconds(coordinator.kickoff_check.fee_wait_secs as i64);
        assert!(retrying.kickoff_deadline(fee_wait) > OffsetDateTime::now_utc());
        assert!(late.kickoff_deadline(fee_wait) <= OffsetDateTime::now_utc());
        for competition in [&retrying, &late] {
            store
                .add_competition_with_tickets(competition.clone(), vec![])
                .await
                .unwrap();
            store.mark_ark_funded(competition.id).await.unwrap();
        }

        let mut status = CompetitionStatus::from(retrying);
        for _ in 0..(6 + 2) {
            status = coordinator.process_status(status).await;
            assert_eq!(status.state_name(), "awaiting_signatures");
        }
        assert!(status.into_competition().errors.is_empty());

        let failed = coordinator
            .process_status(CompetitionStatus::from(late))
            .await;
        assert_eq!(failed.state_name(), "failed");
        let reason = failed.into_competition().errors.last().unwrap().to_string();
        assert!(reason.contains("Arkade is not configured"), "{reason}");
        database.close().await.unwrap();
    }

    /// Competitions an earlier version failed, then cancelled, while their contract held the pot
    /// on-chain are picked up again and settle; ones that never had a funded contract stay
    /// cancelled.
    #[tokio::test]
    async fn stranded_settlements_resume_and_unfunded_cancellations_stay() {
        let directory = tempfile::tempdir().unwrap();
        let (coordinator, database) = test_coordinator(directory.path()).await;
        let store = &coordinator.competition_store;
        let now = OffsetDateTime::now_utc();

        let mut stranded = funded_competition();
        stranded.outcome_broadcasted_at = Some(now - time::Duration::hours(3));
        stranded.delta_broadcasted_at = Some(now - time::Duration::hours(1));
        stranded.failed_at = Some(now - time::Duration::hours(1));
        stranded.cancelled_at = Some(now);

        let mut unfunded = funded_competition();
        unfunded.funding_broadcasted_at = None;
        unfunded.funding_confirmed_at = None;
        unfunded.funding_settled_at = None;
        unfunded.awaiting_attestation_at = None;
        unfunded.failed_at = Some(now - time::Duration::hours(1));
        unfunded.cancelled_at = Some(now);

        let mut settled = funded_competition();
        settled.outcome_broadcasted_at = Some(now - time::Duration::hours(3));
        settled.completed_at = Some(now);

        for competition in [&stranded, &unfunded, &settled] {
            store
                .add_competition_with_tickets(competition.clone(), vec![])
                .await
                .unwrap();
        }
        store
            .update_competitions(vec![stranded.clone(), unfunded.clone(), settled.clone()])
            .await
            .unwrap();

        let active = store.active_competition_ids().await.unwrap();
        assert!(
            active.contains(&stranded.id),
            "the stranded pot is swept again"
        );
        assert!(!active.contains(&unfunded.id));
        assert!(!active.contains(&settled.id));

        let mut resumed = store.get_competition(stranded.id).await.unwrap();
        assert!(resumed.resume_stranded_settlement());
        assert_eq!(
            CompetitionStatus::from(resumed.clone()).state_name(),
            "delta_broadcasted",
            "it resumes where settlement stopped"
        );
        assert!(!resumed.resume_stranded_settlement(), "only once");

        let mut unfunded = store.get_competition(unfunded.id).await.unwrap();
        assert!(!unfunded.resume_stranded_settlement());
        assert!(unfunded.is_cancelled());
        let mut settled = store.get_competition(settled.id).await.unwrap();
        assert!(!settled.resume_stranded_settlement());
        database.close().await.unwrap();
    }
}
