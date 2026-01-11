//! CollectingEntries state - collecting entries until all slots are filled.

use super::{AwaitingEscrow, CompetitionStatus, HasCompetitionData};
use crate::domain::competitions::Competition;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where the competition is collecting entries from participants.
///
/// In this state:
/// - Users can request tickets and pay for them
/// - Users can submit entries after paying
/// - The competition waits until all entry slots are filled
#[derive(Debug, Clone)]
pub struct CollectingEntries {
    pub competition_id: Uuid,
    pub(crate) competition: Competition,
}

impl CollectingEntries {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            competition,
        }
    }

    /// Check if all entry slots have been filled and paid.
    pub fn has_all_entries(&self) -> bool {
        self.competition.has_full_entries() && self.competition.has_all_entries_paid()
    }

    /// Get current entry count.
    pub fn entry_count(&self) -> u64 {
        self.competition.total_entries
    }

    /// Get total allowed entries.
    pub fn total_allowed(&self) -> usize {
        self.competition.event_submission.total_allowed_entries
    }

    /// Transition to AwaitingEscrow when all entries are collected and paid.
    ///
    /// This transition occurs when:
    /// - All entry slots are filled
    /// - All entries have been paid via Lightning HODL invoices
    pub fn all_entries_collected(self) -> Result<CompetitionStatus, CompetitionStatus> {
        if !self.has_all_entries() {
            return Err(CompetitionStatus::CollectingEntries(self));
        }

        Ok(CompetitionStatus::AwaitingEscrow(
            AwaitingEscrow::from_competition(self.competition),
        ))
    }

    /// Check if competition is expired before all entries collected.
    pub fn is_expired(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        now >= self.competition.event_submission.start_observation_date
    }
}

impl HasCompetitionData for CollectingEntries {
    fn competition(&self) -> &Competition {
        &self.competition
    }

    fn competition_mut(&mut self) -> &mut Competition {
        &mut self.competition
    }

    fn into_competition(self) -> Competition {
        self.competition
    }
}
