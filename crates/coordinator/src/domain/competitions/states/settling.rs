//! Settling states: Attested, ExpiryBroadcasted, OutcomeBroadcasted, DeltaBroadcasted

use super::{CompetitionStatus, Completed, HasCompetitionData};
use crate::domain::competitions::Competition;
use bdk_wallet::bitcoin::Transaction;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where the oracle has attested to the outcome.
///
/// In this state, we can broadcast the outcome transaction based on the attestation.
#[derive(Debug, Clone)]
pub struct Attested {
    pub competition_id: Uuid,
    pub(crate) competition: Competition,
}

impl Attested {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            competition,
        }
    }

    /// Transition to OutcomeBroadcasted after broadcasting the outcome transaction.
    ///
    /// # Arguments
    /// * `outcome_tx` - The outcome transaction that was broadcast
    pub fn outcome_broadcasted(mut self, outcome_tx: Transaction) -> CompetitionStatus {
        self.competition.outcome_transaction = Some(outcome_tx);
        self.competition.outcome_broadcasted_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::OutcomeBroadcasted(OutcomeBroadcasted::from_competition(
            self.competition,
        ))
    }

    /// Get the attestation value.
    pub fn attestation(&self) -> Option<&dlctix::secp::MaybeScalar> {
        self.competition.attestation.as_ref()
    }
}

impl HasCompetitionData for Attested {
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

/// State where the event expired and the expiry transaction was broadcast.
///
/// This is the refund path - all participants get their funds back.
#[derive(Debug, Clone)]
pub struct ExpiryBroadcasted {
    pub competition_id: Uuid,
    pub expiry_broadcasted_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl ExpiryBroadcasted {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            expiry_broadcasted_at: competition
                .expiry_broadcasted_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to Completed after expiry refunds are processed.
    pub fn completed(mut self) -> CompetitionStatus {
        self.competition.completed_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::Completed(Completed::from_competition(self.competition))
    }
}

impl HasCompetitionData for ExpiryBroadcasted {
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

/// State where the outcome transaction has been broadcast.
///
/// In this state, we wait for relative timelock then broadcast delta transactions.
#[derive(Debug, Clone)]
pub struct OutcomeBroadcasted {
    pub competition_id: Uuid,
    pub outcome_broadcasted_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl OutcomeBroadcasted {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            outcome_broadcasted_at: competition
                .outcome_broadcasted_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to DeltaBroadcasted after broadcasting delta (split/close) transactions.
    pub fn delta_broadcasted(mut self) -> CompetitionStatus {
        self.competition.delta_broadcasted_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::DeltaBroadcasted(DeltaBroadcasted::from_competition(self.competition))
    }

    /// Get the outcome transaction for confirmation checking.
    pub fn outcome_transaction(&self) -> Option<&Transaction> {
        self.competition.outcome_transaction.as_ref()
    }
}

impl HasCompetitionData for OutcomeBroadcasted {
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

/// State where the first delta (split/close) transactions have been broadcast.
///
/// In this state, we process any remaining reclaim transactions.
#[derive(Debug, Clone)]
pub struct DeltaBroadcasted {
    pub competition_id: Uuid,
    pub delta_broadcasted_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl DeltaBroadcasted {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            delta_broadcasted_at: competition
                .delta_broadcasted_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to Completed after all reclaim transactions are broadcast.
    pub fn completed(mut self) -> CompetitionStatus {
        self.competition.completed_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::Completed(Completed::from_competition(self.competition))
    }
}

impl HasCompetitionData for DeltaBroadcasted {
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
