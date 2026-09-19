//! Failed and Cancelled states - terminal error/cancellation states.

use super::HasCompetitionData;
use crate::domain::competitions::{Competition, CompetitionError};
use time::OffsetDateTime;
use uuid::Uuid;

/// Terminal state indicating the competition failed due to an error.
///
/// In this state:
/// - An unrecoverable error occurred during processing
/// - No further state transitions are possible
/// - Manual intervention may be required
#[derive(Debug, Clone)]
pub struct Failed {
    pub competition_id: Uuid,
    pub failed_at: OffsetDateTime,
    pub error: CompetitionError,
    pub previous_state: String,
    pub(crate) competition: Competition,
}

impl Failed {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        let error =
            competition
                .errors
                .last()
                .cloned()
                .unwrap_or(CompetitionError::InvalidStateTransition(
                    "Unknown error".to_string(),
                ));

        Self {
            competition_id: competition.id,
            failed_at: competition
                .failed_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            error,
            previous_state: "unknown".to_string(),
            competition,
        }
    }
}

impl HasCompetitionData for Failed {
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

/// Terminal state indicating the competition was cancelled.
///
/// In this state:
/// - The competition was cancelled before completion
/// - Could be due to expiry, user request, or system decision
/// - No further state transitions are possible
#[derive(Debug, Clone)]
pub struct Cancelled {
    pub competition_id: Uuid,
    pub cancelled_at: OffsetDateTime,
    pub reason: String,
    pub previous_state: String,
    pub(crate) competition: Competition,
}

impl Cancelled {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            cancelled_at: competition
                .cancelled_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            reason: "Loaded from database".to_string(),
            previous_state: "unknown".to_string(),
            competition,
        }
    }
}

impl HasCompetitionData for Cancelled {
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
