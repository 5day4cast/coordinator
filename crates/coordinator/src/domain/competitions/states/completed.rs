//! Completed state - competition has finished successfully.

use super::HasCompetitionData;
use crate::domain::competitions::Competition;
use time::OffsetDateTime;
use uuid::Uuid;

/// Terminal state indicating the competition completed successfully.
///
/// In this state:
/// - All transactions have been broadcast and confirmed
/// - Winners have been paid out (or funds returned via expiry)
/// - No further state transitions are possible
#[derive(Debug, Clone)]
pub struct Completed {
    pub competition_id: Uuid,
    pub completed_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl Completed {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            completed_at: competition
                .completed_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }
}

impl HasCompetitionData for Completed {
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
