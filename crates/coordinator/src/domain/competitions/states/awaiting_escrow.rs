//! AwaitingEscrow state - waiting for escrow transactions to confirm.

use super::{CompetitionStatus, EscrowConfirmed, HasCompetitionData};
use crate::domain::competitions::Competition;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where all entries are collected and we're waiting for escrow confirmations.
///
/// In this state:
/// - All entries have been paid via Lightning HODL invoices
/// - Escrow transactions have been broadcast to Bitcoin network
/// - Waiting for required confirmations on escrow transactions
#[derive(Debug, Clone)]
pub struct AwaitingEscrow {
    pub competition_id: Uuid,
    pub(crate) competition: Competition,
}

impl AwaitingEscrow {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            competition,
        }
    }

    /// Transition to EscrowConfirmed when all escrow transactions are confirmed.
    ///
    /// This should be called after verifying all escrow transactions have
    /// the required number of confirmations.
    pub fn escrow_confirmed(mut self) -> CompetitionStatus {
        self.competition.escrow_funds_confirmed_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::EscrowConfirmed(EscrowConfirmed::from_competition(self.competition))
    }

    /// Check if we're still waiting (not yet confirmed).
    pub fn is_waiting(&self) -> bool {
        self.competition.escrow_funds_confirmed_at.is_none()
    }
}

impl HasCompetitionData for AwaitingEscrow {
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
