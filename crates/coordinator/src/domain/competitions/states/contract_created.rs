//! ContractCreated state - contract created, awaiting signatures.

use super::{AwaitingSignatures, CompetitionStatus, HasCompetitionData};
use crate::domain::competitions::Competition;
use dlctix::musig2::PubNonce;
use dlctix::SigMap;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where contract parameters have been created.
///
/// In this state:
/// - Contract parameters are generated
/// - Funding PSBT is created
/// - Coordinator's public nonces are generated
/// - Waiting for participant signatures (via Keymeld)
#[derive(Debug, Clone)]
pub struct ContractCreated {
    pub competition_id: Uuid,
    pub contracted_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl ContractCreated {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            contracted_at: competition
                .contracted_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Set the coordinator's public nonces and transition to AwaitingSignatures.
    ///
    /// This is called after the coordinator generates its nonces for the MuSig2
    /// signing session. With Keymeld integration, this will be handled by the
    /// Keymeld service.
    pub fn nonces_generated(mut self, public_nonces: SigMap<PubNonce>) -> CompetitionStatus {
        self.competition.public_nonces = Some(public_nonces);
        CompetitionStatus::AwaitingSignatures(AwaitingSignatures::from_competition(
            self.competition,
        ))
    }

    /// Check if we already have nonces (can skip to AwaitingSignatures).
    pub fn has_nonces(&self) -> bool {
        self.competition.public_nonces.is_some()
    }
}

impl HasCompetitionData for ContractCreated {
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
