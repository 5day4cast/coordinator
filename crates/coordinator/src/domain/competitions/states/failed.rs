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
    pub(crate) competition: Option<Competition>,
}

impl Failed {
    /// Create a new Failed state from an error.
    pub fn new(
        competition_id: Uuid,
        error: CompetitionError,
        previous_state: String,
        competition: Option<Competition>,
    ) -> Self {
        Self {
            competition_id,
            failed_at: OffsetDateTime::now_utc(),
            error,
            previous_state,
            competition,
        }
    }

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
            competition: Some(competition),
        }
    }
}

impl HasCompetitionData for Failed {
    fn competition(&self) -> &Competition {
        self.competition
            .as_ref()
            .expect("Failed state should have competition data")
    }

    fn competition_mut(&mut self) -> &mut Competition {
        self.competition
            .as_mut()
            .expect("Failed state should have competition data")
    }

    fn into_competition(self) -> Competition {
        self.competition
            .expect("Failed state should have competition data")
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
    pub(crate) competition: Option<Competition>,
}

impl Cancelled {
    /// Create a new Cancelled state.
    pub fn new(
        competition_id: Uuid,
        reason: String,
        previous_state: String,
        competition: Option<Competition>,
    ) -> Self {
        Self {
            competition_id,
            cancelled_at: OffsetDateTime::now_utc(),
            reason,
            previous_state,
            competition,
        }
    }

    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            cancelled_at: competition
                .cancelled_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            reason: "Loaded from database".to_string(),
            previous_state: "unknown".to_string(),
            competition: Some(competition),
        }
    }
}

impl HasCompetitionData for Cancelled {
    fn competition(&self) -> &Competition {
        self.competition
            .as_ref()
            .expect("Cancelled state should have competition data")
    }

    fn competition_mut(&mut self) -> &mut Competition {
        self.competition
            .as_mut()
            .expect("Cancelled state should have competition data")
    }

    fn into_competition(self) -> Competition {
        self.competition
            .expect("Cancelled state should have competition data")
    }
}
