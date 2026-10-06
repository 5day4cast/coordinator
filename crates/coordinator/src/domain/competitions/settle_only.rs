//! Settle-only mode: finish every money obligation and take no new money, as after a restore
//! from backups (`docs/ops/disaster-recovery.md`).
//!
//! While it is on, no competition is created, no ticket is issued and no entry is accepted, so
//! no new swap or escrow is made either. Everything that settles money already taken carries on:
//! the runners' kickoffs, attestation polling, outcome, expiry and split broadcasts, Lightning
//! payouts and their window cutoff, refunds, escrow recovery and market-maker reclaims.
//!
//! A competition or pool that has not kicked off, meaning no contract has been built for it and
//! no Arkade batch has funded it, is cancelled and refunded through the usual cleanup by default
//! (`settle_only_unstarted = "refund"`). With `"kickoff"` it starts as usual once its paid
//! entries are in.

use super::*;
use crate::config::SettleOnlyUnstarted;
use crate::domain::competitions::{CompetitionKind, Lease};

/// What a player sees while the coordinator is in settle-only mode: no operator detail.
pub const SETTLE_ONLY_PAUSED: &str = "Entries are paused";

/// The mode as configured at start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleOnly {
    pub enabled: bool,
    pub unstarted: SettleOnlyUnstarted,
}

impl SettleOnly {
    /// Whether a competition that has not kicked off is cancelled and refunded.
    pub fn refunds_unstarted(&self) -> bool {
        self.enabled && self.unstarted == SettleOnlyUnstarted::Refund
    }
}

impl Coordinator {
    pub fn with_settle_only(mut self, enabled: bool, unstarted: SettleOnlyUnstarted) -> Self {
        self.settle_only = SettleOnly { enabled, unstarted };
        if enabled {
            warn!(
                "Settle-only mode: no new competitions, tickets or entries; unstarted \
                 competitions are {}",
                match unstarted {
                    SettleOnlyUnstarted::Refund => "cancelled and refunded",
                    SettleOnlyUnstarted::Kickoff => "kicked off once their entries are paid",
                }
            );
        }
        crate::metrics::SETTLE_ONLY.set(i64::from(enabled));
        self
    }

    /// Whether the coordinator is in settle-only mode.
    pub fn settle_only(&self) -> bool {
        self.settle_only.enabled
    }

    /// Refuse anything that would take new money while in settle-only mode: a competition, a
    /// ticket, an entry.
    pub(super) fn require_new_money_allowed(&self) -> Result<(), Error> {
        if self.settle_only.enabled {
            return Err(Error::SettleOnly);
        }
        Ok(())
    }

    /// In settle-only mode with unstarted competitions refunded, cancel `competition` if it has
    /// not kicked off: cleanup then releases its held invoices and refunds its escrows. Returns
    /// whether it was cancelled. One that kicked off meanwhile, or whose lease was lost, is left
    /// for its next step.
    pub(super) async fn cancel_unstarted_for_settle_only(
        &self,
        competition: &Competition,
        lease: &Lease,
    ) -> Result<bool, anyhow::Error> {
        if !self.settle_only.refunds_unstarted() || has_kicked_off(competition) {
            return Ok(false);
        }
        let cancelled = if competition.kind == CompetitionKind::Queued {
            if competition.pools_formed_at.is_some() {
                return Ok(false);
            }
            self.competition_store
                .cancel_queued_competition(competition.id, lease)
                .await?
        } else {
            self.competition_store
                .cancel_unstarted(competition.id, lease)
                .await?
        };
        if cancelled {
            info!(
                "Settle-only mode: cancelled competition {}, which had not kicked off; its \
                 entries are refunded",
                competition.id
            );
            self.release_held_invoices(competition.id).await;
        }
        Ok(cancelled)
    }
}

/// Whether a competition has gone past the point where cancelling it refunds every entry: its
/// contract is built, signed or funded, or its settlement has begun.
pub(super) fn has_kicked_off(competition: &Competition) -> bool {
    competition.contract_parameters.is_some()
        || competition.contracted_at.is_some()
        || competition.signed_at.is_some()
        || competition.funding_outpoint.is_some()
        || competition.funding_broadcasted_at.is_some()
        || competition.funding_confirmed_at.is_some()
        || competition.completed_at.is_some()
}

impl CompetitionStore {
    /// Cancel a single competition or pool that has not kicked off, under its lease. The
    /// conditions are checked in the write, since a step elsewhere may have built its contract
    /// after this one read it. Returns whether it was cancelled.
    pub(super) async fn cancel_unstarted(
        &self,
        competition_id: Uuid,
        lease: &Lease,
    ) -> Result<bool, DatabaseWriteError> {
        let id = competition_id.to_string();
        let lease = lease.clone();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        self.db_connection
            .execute_write(move |pool| async move {
                let changed = sqlx::query(
                    "UPDATE competitions SET cancelled_at = ?
                     WHERE id = ? AND kind != 'queued'
                       AND cancelled_at IS NULL AND failed_at IS NULL AND completed_at IS NULL
                       AND contract_parameters IS NULL AND contracted_at IS NULL
                       AND signed_at IS NULL AND funding_outpoint IS NULL
                       AND funding_broadcasted_at IS NULL AND funding_confirmed_at IS NULL
                       AND NOT EXISTS (SELECT 1 FROM ark_funded_competitions a
                                       WHERE a.event_id = competitions.id
                                         AND a.commitment_tx IS NOT NULL)
                       AND EXISTS (SELECT 1 FROM leases WHERE resource = ? AND holder = ? AND token = ?)",
                )
                .bind(now)
                .bind(id)
                .bind(lease.resource)
                .bind(lease.holder)
                .bind(lease.token)
                .execute(&pool)
                .await?
                .rows_affected();
                Ok(changed == 1)
            })
            .await
    }
}
