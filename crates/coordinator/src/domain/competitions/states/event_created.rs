//! EventCreated state - oracle event created, ready to submit entries.

use super::{CompetitionStatus, EntriesSubmitted, HasCompetitionData};
use crate::domain::competitions::Competition;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where the oracle event has been created and entries can be submitted.
///
/// This is a "pass-through" state - the coordinator should immediately
/// proceed to submit all entries to the oracle.
#[derive(Debug, Clone)]
pub struct EventCreated {
    pub competition_id: Uuid,
    pub event_created_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl EventCreated {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            event_created_at: competition
                .event_created_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to EntriesSubmitted after successfully submitting all entries to oracle.
    pub fn entries_submitted(mut self) -> CompetitionStatus {
        self.competition.entries_submitted_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::EntriesSubmitted(EntriesSubmitted::from_competition(self.competition))
    }
}

impl HasCompetitionData for EventCreated {
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
