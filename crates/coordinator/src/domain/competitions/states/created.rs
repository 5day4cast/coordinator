//! Created state - initial state when competition is first created.

use super::{CollectingEntries, CompetitionStatus, HasCompetitionData};
use crate::domain::competitions::{Competition, CreateEvent};
use time::OffsetDateTime;
use uuid::Uuid;

/// Initial state when a competition is first created.
///
/// In this state, the competition is waiting for entries to be added.
/// No entries have been received yet.
#[derive(Debug, Clone)]
pub struct Created {
    pub competition_id: Uuid,
    pub created_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl Created {
    /// Create a new competition in the Created state.
    pub fn new(create_event: &CreateEvent) -> Self {
        let competition = Competition::new(create_event);
        Self {
            competition_id: competition.id,
            created_at: competition.created_at,
            competition,
        }
    }

    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            created_at: competition.created_at,
            competition,
        }
    }

    /// Transition to CollectingEntries when the first entry is added.
    ///
    /// This transition occurs when a user pays for a ticket and submits an entry.
    pub fn first_entry_added(self) -> CompetitionStatus {
        // The entry has already been added to the competition via the store
        // We just need to transition to the next state
        CompetitionStatus::CollectingEntries(CollectingEntries::from_competition(self.competition))
    }

    /// Check if competition is expired before receiving any entries.
    pub fn is_expired(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        now >= self.competition.event_submission.start_observation_date
    }
}

impl HasCompetitionData for Created {
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
