//! AwaitingAttestation state - waiting for oracle to attest to the outcome.

use super::{Attested, CompetitionStatus, ExpiryBroadcasted, HasCompetitionData};
use crate::domain::competitions::Competition;
use dlctix::secp::MaybeScalar;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where we're waiting for the oracle to attest to the competition outcome.
///
/// In this state:
/// - Funding transaction is confirmed
/// - All invoices are settled
/// - Waiting for the oracle to publish the attestation
#[derive(Debug, Clone)]
pub struct AwaitingAttestation {
    pub competition_id: Uuid,
    pub(crate) competition: Competition,
}

impl AwaitingAttestation {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            competition,
        }
    }

    /// When to ask the oracle again, unless an event comes first.
    ///
    /// The oracle attests only after the observation window closes, so there is nothing to ask
    /// before then. A contract that expires sooner is checked at its expiry, to broadcast it.
    pub fn next_check(&self, now: OffsetDateTime, idle: std::time::Duration) -> OffsetDateTime {
        let mut ready = self.competition.event_submission.end_observation_date;
        let expiry = self
            .competition
            .signed_contract
            .as_ref()
            .and_then(|contract| contract.dlc().params().event.expiry)
            // Below this, a locktime is a block height, not a time.
            .filter(|expiry| *expiry >= 500_000_000)
            .and_then(|expiry| OffsetDateTime::from_unix_timestamp(i64::from(expiry)).ok());
        if let Some(expiry) = expiry {
            ready = ready.min(expiry);
        }
        (now + idle).max(ready)
    }

    /// Transition to Attested when the oracle provides an attestation.
    ///
    /// # Arguments
    /// * `attestation` - The oracle's attestation (scalar value)
    pub fn attested(mut self, attestation: MaybeScalar) -> CompetitionStatus {
        self.competition.attestation = Some(attestation);
        CompetitionStatus::Attested(Attested::from_competition(self.competition))
    }

    /// Transition to ExpiryBroadcasted if the event has expired.
    ///
    /// This is the refund path - used when the oracle doesn't attest before expiry.
    pub fn expired(mut self) -> CompetitionStatus {
        self.competition.expiry_broadcasted_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::ExpiryBroadcasted(ExpiryBroadcasted::from_competition(self.competition))
    }

    /// Check if the event has expired (past the expiry time without attestation).
    pub fn is_expired(&self, current_blockchain_time: u64) -> bool {
        if let Some(ref event_announcement) = self.competition.event_announcement {
            if let Some(expiry) = event_announcement.expiry {
                return current_blockchain_time > expiry as u64;
            }
        }
        false
    }
}

impl HasCompetitionData for AwaitingAttestation {
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
