//! EscrowConfirmed state - escrow transactions confirmed, ready to create oracle event.

use super::{CompetitionStatus, EventCreated, HasCompetitionData};
use crate::domain::competitions::Competition;
use crate::infra::oracle::Event;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where escrow transactions are confirmed and we can create the oracle event.
///
/// This is a "pass-through" state - the coordinator should immediately
/// proceed to create the oracle event.
#[derive(Debug, Clone)]
pub struct EscrowConfirmed {
    pub competition_id: Uuid,
    pub confirmed_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl EscrowConfirmed {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            confirmed_at: competition
                .escrow_funds_confirmed_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to EventCreated after successfully creating the oracle event.
    ///
    /// # Arguments
    /// * `event` - The event returned from the oracle after creation
    pub fn event_created(mut self, event: Event) -> CompetitionStatus {
        self.competition.event_announcement = Some(event.event_announcement);
        self.competition.event_created_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::EventCreated(EventCreated::from_competition(self.competition))
    }
}

impl HasCompetitionData for EscrowConfirmed {
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
