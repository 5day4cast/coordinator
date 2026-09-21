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

        if competition.is_cancelled()
            || competition.is_completed()
            || competition.is_expiry_broadcasted()
        {
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
        if competition.is_expired() {
            competition.cancelled_at = Some(now);
            self.save_leased(competition, lease).await?;
            info!("Cancelled expired competition {competition_id}");
            self.release_held_invoices(competition_id).await;
            return Ok(Step::Finished);
        }
        self.renew_funding_reservation(&competition)
            .await
            .map_err(|e| anyhow!("Cannot reserve funding inputs: {e}"))?;

        let status: CompetitionStatus = competition.into();
        let before = status.state_name();
        let next = self.process_status(status).await;
        let after = next.state_name();
        let wait = if after != before && next.is_immediate_transition() {
            Wait::Now
        } else {
            Wait::Until(next.next_check(OffsetDateTime::now_utc(), pacing.idle))
        };
        let competition = next.into_competition();
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
    pub async fn clean_up_competitions(&self) -> Result<(), anyhow::Error> {
        for competition_id in self
            .competition_store
            .get_competitions_pending_cleanup(self.escrow_enabled)
            .await?
        {
            self.release_held_invoices(competition_id).await;
            self.reclaim_escrows(competition_id).await;
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
