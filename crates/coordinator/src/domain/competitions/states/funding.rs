//! Funding-related states: SigningComplete, FundingBroadcasted, FundingConfirmed, FundingSettled

use super::{AwaitingAttestation, CompetitionStatus, HasCompetitionData};
use crate::domain::competitions::Competition;
use bdk_wallet::bitcoin::Transaction;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where all signatures have been collected and the contract is fully signed.
///
/// This is a "pass-through" state - the coordinator should immediately
/// proceed to sign and broadcast the funding transaction.
#[derive(Debug, Clone)]
pub struct SigningComplete {
    pub competition_id: Uuid,
    pub signed_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl SigningComplete {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            signed_at: competition
                .signed_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to FundingBroadcasted after broadcasting the funding transaction.
    ///
    /// # Arguments
    /// * `funding_tx` - The signed funding transaction that was broadcast
    pub fn funding_broadcasted(mut self, funding_tx: Transaction) -> CompetitionStatus {
        self.competition.funding_transaction = Some(funding_tx);
        self.competition.funding_broadcasted_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::FundingBroadcasted(FundingBroadcasted::from_competition(
            self.competition,
        ))
    }
}

impl HasCompetitionData for SigningComplete {
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

/// State where the funding transaction has been broadcast to the network.
///
/// In this state, we're waiting for the transaction to be confirmed.
#[derive(Debug, Clone)]
pub struct FundingBroadcasted {
    pub competition_id: Uuid,
    pub broadcasted_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl FundingBroadcasted {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            broadcasted_at: competition
                .funding_broadcasted_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to FundingConfirmed when the transaction has enough confirmations.
    pub fn funding_confirmed(mut self) -> CompetitionStatus {
        self.competition.funding_confirmed_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::FundingConfirmed(FundingConfirmed::from_competition(self.competition))
    }

    /// Get the funding transaction for confirmation checking.
    pub fn funding_transaction(&self) -> Option<&Transaction> {
        self.competition.funding_transaction.as_ref()
    }
}

impl HasCompetitionData for FundingBroadcasted {
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

/// State where the funding transaction has been confirmed.
///
/// This is a "pass-through" state - we immediately check if invoices are settled.
#[derive(Debug, Clone)]
pub struct FundingConfirmed {
    pub competition_id: Uuid,
    pub confirmed_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl FundingConfirmed {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            confirmed_at: competition
                .funding_confirmed_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to FundingSettled when all HODL invoices are settled.
    pub fn funding_settled(mut self) -> CompetitionStatus {
        self.competition.funding_settled_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::FundingSettled(FundingSettled::from_competition(self.competition))
    }
}

impl HasCompetitionData for FundingConfirmed {
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

/// State where funding is complete and all invoices are settled.
///
/// This is a "pass-through" state - we immediately start waiting for attestation.
#[derive(Debug, Clone)]
pub struct FundingSettled {
    pub competition_id: Uuid,
    pub settled_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl FundingSettled {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            settled_at: competition
                .funding_settled_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to AwaitingAttestation.
    pub fn await_attestation(self) -> CompetitionStatus {
        CompetitionStatus::AwaitingAttestation(AwaitingAttestation::from_competition(
            self.competition,
        ))
    }
}

impl HasCompetitionData for FundingSettled {
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
