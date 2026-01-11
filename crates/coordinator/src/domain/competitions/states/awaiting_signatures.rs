//! AwaitingSignatures state - waiting for all participants to sign.
//!
//! This state consolidates the old MuSig-specific states:
//! - NoncesCollected
//! - AggregateNoncesGenerated
//! - PartialSignaturesCollected
//!
//! With Keymeld integration, all the MuSig2 complexity is handled by the
//! Keymeld service, so we just need to wait for signing to complete.

use super::{CompetitionStatus, HasCompetitionData, SigningComplete};
use crate::domain::competitions::Competition;
use dlctix::SignedContract;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where we're waiting for all signatures to be collected.
///
/// With Keymeld integration:
/// - Keymeld handles nonce aggregation internally
/// - Keymeld handles partial signature collection
/// - Keymeld handles signature aggregation
/// - We just poll for completion
///
/// Without Keymeld (legacy):
/// - Collect nonces from all participants
/// - Aggregate nonces and generate coordinator's partial signatures
/// - Collect partial signatures from all participants
/// - Aggregate all signatures into final contract
#[derive(Debug, Clone)]
pub struct AwaitingSignatures {
    pub competition_id: Uuid,
    pub(crate) competition: Competition,
}

impl AwaitingSignatures {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            competition,
        }
    }

    /// Check if all participant nonces have been collected.
    pub fn has_all_nonces(&self) -> bool {
        self.competition.has_all_entry_nonces()
    }

    /// Check if all participant signatures have been collected.
    pub fn has_all_signatures(&self) -> bool {
        self.competition.has_all_entry_partial_signatures()
    }

    /// Check if we already have a signed contract.
    pub fn is_signed(&self) -> bool {
        self.competition.signed_contract.is_some()
    }

    /// Transition to SigningComplete after all signatures are aggregated.
    ///
    /// # Arguments
    /// * `signed_contract` - The fully signed DLC contract
    pub fn signing_complete(mut self, signed_contract: SignedContract) -> CompetitionStatus {
        self.competition.signed_contract = Some(signed_contract);
        self.competition.signed_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::SigningComplete(SigningComplete::from_competition(self.competition))
    }

    /// Check if the signing process has timed out.
    ///
    /// Users have a limited time to submit their nonces and signatures.
    pub fn is_expired(&self) -> bool {
        self.competition.is_expired()
    }
}

impl HasCompetitionData for AwaitingSignatures {
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
