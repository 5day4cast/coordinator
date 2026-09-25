//! Typestate machine for competition lifecycle management.
//!
//! This module implements a compile-time safe state machine where each state
//! is a separate struct. State transitions consume `self` and return the next
//! state, preventing invalid transitions at compile time.
//!
//! # State Flow
//!
//! ```text
//! Created
//!     ↓
//! CollectingEntries
//!     ↓
//! AwaitingEscrow
//!     ↓
//! EscrowConfirmed
//!     ↓
//! EventCreated
//!     ↓
//! EntriesSubmitted
//!     ↓
//! ContractCreated
//!     ↓
//! AwaitingSignatures (Keymeld handles MuSig internally)
//!     ↓
//! SigningComplete
//!     ↓
//! FundingBroadcasted
//!     ↓
//! FundingConfirmed
//!     ↓
//! FundingSettled
//!     ↓
//! AwaitingAttestation
//!     ↓
//! Attested ──────────────→ ExpiryBroadcasted (if expired)
//!     ↓
//! OutcomeBroadcasted
//!     ↓
//! DeltaBroadcasted
//!     ↓
//! Completed
//!
//! (Any state can transition to Failed or Cancelled)
//! ```

mod awaiting_attestation;
mod awaiting_escrow;
mod awaiting_signatures;
mod collecting_entries;
mod completed;
mod contract_created;
mod created;
mod entries_submitted;
mod escrow_confirmed;
mod event_created;
mod failed;
mod funding;
mod settling;

pub use awaiting_attestation::*;
pub use awaiting_escrow::*;
pub use awaiting_signatures::*;
pub use collecting_entries::*;
pub use completed::*;
pub use contract_created::*;
pub use created::*;
pub use entries_submitted::*;
pub use escrow_confirmed::*;
pub use event_created::*;
pub use failed::*;
pub use funding::*;
pub use settling::*;

use super::{Competition, CompetitionError};

use std::fmt;
use time::OffsetDateTime;
use uuid::Uuid;

/// Wrapper enum for dynamic dispatch when state type isn't known at compile time.
///
/// Used for:
/// - Database persistence (converting to/from Competition)
/// - Handler loops that process competitions of any state
/// - API responses
#[derive(Debug, Clone)]
pub enum CompetitionStatus {
    Created(Created),
    CollectingEntries(CollectingEntries),
    AwaitingEscrow(AwaitingEscrow),
    EscrowConfirmed(EscrowConfirmed),
    EventCreated(EventCreated),
    EntriesSubmitted(EntriesSubmitted),
    ContractCreated(ContractCreated),
    AwaitingSignatures(AwaitingSignatures),
    SigningComplete(SigningComplete),
    FundingBroadcasted(FundingBroadcasted),
    FundingConfirmed(FundingConfirmed),
    FundingSettled(FundingSettled),
    AwaitingAttestation(AwaitingAttestation),
    Attested(Attested),
    ExpiryBroadcasted(ExpiryBroadcasted),
    OutcomeBroadcasted(OutcomeBroadcasted),
    DeltaBroadcasted(DeltaBroadcasted),
    Completed(Completed),
    Failed(Failed),
    Cancelled(Cancelled),
}

impl CompetitionStatus {
    /// Get the competition ID regardless of state.
    pub fn competition_id(&self) -> Uuid {
        match self {
            Self::Created(s) => s.competition_id,
            Self::CollectingEntries(s) => s.competition_id,
            Self::AwaitingEscrow(s) => s.competition_id,
            Self::EscrowConfirmed(s) => s.competition_id,
            Self::EventCreated(s) => s.competition_id,
            Self::EntriesSubmitted(s) => s.competition_id,
            Self::ContractCreated(s) => s.competition_id,
            Self::AwaitingSignatures(s) => s.competition_id,
            Self::SigningComplete(s) => s.competition_id,
            Self::FundingBroadcasted(s) => s.competition_id,
            Self::FundingConfirmed(s) => s.competition_id,
            Self::FundingSettled(s) => s.competition_id,
            Self::AwaitingAttestation(s) => s.competition_id,
            Self::Attested(s) => s.competition_id,
            Self::ExpiryBroadcasted(s) => s.competition_id,
            Self::OutcomeBroadcasted(s) => s.competition_id,
            Self::DeltaBroadcasted(s) => s.competition_id,
            Self::Completed(s) => s.competition_id,
            Self::Failed(s) => s.competition_id,
            Self::Cancelled(s) => s.competition_id,
        }
    }

    /// Get the state name as a string for logging/display.
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Created(_) => "created",
            Self::CollectingEntries(_) => "collecting_entries",
            Self::AwaitingEscrow(_) => "awaiting_escrow",
            Self::EscrowConfirmed(_) => "escrow_confirmed",
            Self::EventCreated(_) => "event_created",
            Self::EntriesSubmitted(_) => "entries_submitted",
            Self::ContractCreated(_) => "contract_created",
            Self::AwaitingSignatures(_) => "awaiting_signatures",
            Self::SigningComplete(_) => "signing_complete",
            Self::FundingBroadcasted(_) => "funding_broadcasted",
            Self::FundingConfirmed(_) => "funding_confirmed",
            Self::FundingSettled(_) => "funding_settled",
            Self::AwaitingAttestation(_) => "awaiting_attestation",
            Self::Attested(_) => "attested",
            Self::ExpiryBroadcasted(_) => "expiry_broadcasted",
            Self::OutcomeBroadcasted(_) => "outcome_broadcasted",
            Self::DeltaBroadcasted(_) => "delta_broadcasted",
            Self::Completed(_) => "completed",
            Self::Failed(_) => "failed",
            Self::Cancelled(_) => "cancelled",
        }
    }

    /// Check if this is a terminal state (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed(_) | Self::Failed(_) | Self::Cancelled(_)
        )
    }

    /// Check if this state can be immediately transitioned without waiting.
    ///
    /// Some states are "pass-through" states where the coordinator can
    /// immediately proceed to the next action without waiting for external input.
    pub fn is_immediate_transition(&self) -> bool {
        matches!(
            self,
            Self::EscrowConfirmed(_)
                | Self::EventCreated(_)
                | Self::EntriesSubmitted(_)
                | Self::SigningComplete(_)
                | Self::FundingConfirmed(_)
                | Self::FundingSettled(_)
        )
    }

    /// When a runner should step this competition again, unless an event wakes it first.
    ///
    /// States with a specific policy answer themselves; the rest wait at most `idle`.
    pub fn next_check(&self, now: OffsetDateTime, idle: std::time::Duration) -> OffsetDateTime {
        match self {
            Self::AwaitingAttestation(state) => state.next_check(now, idle),
            _ => now + idle,
        }
    }

    /// Transition to Failed state from any state.
    pub fn fail(self, error: CompetitionError) -> CompetitionStatus {
        let competition_id = self.competition_id();
        let previous_state = self.state_name().to_string();
        let mut competition = self.into_competition();
        let failed_at = OffsetDateTime::now_utc();
        competition.failed_at = Some(failed_at);
        CompetitionStatus::Failed(Failed {
            competition_id,
            failed_at,
            error,
            previous_state,
            competition,
        })
    }

    /// Transition to Cancelled state from any state.
    pub fn cancel(self, reason: String) -> CompetitionStatus {
        let competition_id = self.competition_id();
        let previous_state = self.state_name().to_string();
        let mut competition = self.into_competition();
        let cancelled_at = OffsetDateTime::now_utc();
        competition.cancelled_at = Some(cancelled_at);
        CompetitionStatus::Cancelled(Cancelled {
            competition_id,
            cancelled_at,
            reason,
            previous_state,
            competition,
        })
    }

    /// Extract the competition data from any state.
    pub fn into_competition(self) -> Competition {
        match self {
            Self::Created(s) => s.into_competition(),
            Self::CollectingEntries(s) => s.into_competition(),
            Self::AwaitingEscrow(s) => s.into_competition(),
            Self::EscrowConfirmed(s) => s.into_competition(),
            Self::EventCreated(s) => s.into_competition(),
            Self::EntriesSubmitted(s) => s.into_competition(),
            Self::ContractCreated(s) => s.into_competition(),
            Self::AwaitingSignatures(s) => s.into_competition(),
            Self::SigningComplete(s) => s.into_competition(),
            Self::FundingBroadcasted(s) => s.into_competition(),
            Self::FundingConfirmed(s) => s.into_competition(),
            Self::FundingSettled(s) => s.into_competition(),
            Self::AwaitingAttestation(s) => s.into_competition(),
            Self::Attested(s) => s.into_competition(),
            Self::ExpiryBroadcasted(s) => s.into_competition(),
            Self::OutcomeBroadcasted(s) => s.into_competition(),
            Self::DeltaBroadcasted(s) => s.into_competition(),
            Self::Completed(s) => s.into_competition(),
            Self::Failed(s) => s.into_competition(),
            Self::Cancelled(s) => s.into_competition(),
        }
    }
}

impl fmt::Display for CompetitionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.state_name())
    }
}

/// Trait for states that hold competition data and can be converted back.
pub trait HasCompetitionData {
    /// Get a reference to the underlying competition data.
    fn competition(&self) -> &Competition;

    /// Get a mutable reference to the underlying competition data.
    fn competition_mut(&mut self) -> &mut Competition;

    /// Consume self and return the competition data.
    fn into_competition(self) -> Competition;
}

/// Convert from Competition (loaded from DB) to the appropriate state.
impl From<Competition> for CompetitionStatus {
    fn from(competition: Competition) -> Self {
        // Use the existing get_state() logic to determine current state,
        // then wrap the competition in the appropriate state struct.
        let state = competition.get_state();

        match state {
            // `get_state` has no state for a competition that has entries but is not full yet.
            // Its entry count is stored, so it reloads as collecting entries, the state its first
            // entry moved it to; otherwise every step would log that move again.
            super::CompetitionState::Created if competition.total_entries > 0 => {
                CompetitionStatus::CollectingEntries(CollectingEntries::from_competition(
                    competition,
                ))
            }
            super::CompetitionState::Created => {
                CompetitionStatus::Created(Created::from_competition(competition))
            }
            super::CompetitionState::EntriesCollected => {
                CompetitionStatus::AwaitingEscrow(AwaitingEscrow::from_competition(competition))
            }
            super::CompetitionState::EscrowFundsConfirmed => {
                CompetitionStatus::EscrowConfirmed(EscrowConfirmed::from_competition(competition))
            }
            super::CompetitionState::EventCreated => {
                CompetitionStatus::EventCreated(EventCreated::from_competition(competition))
            }
            super::CompetitionState::EntriesSubmitted => {
                CompetitionStatus::EntriesSubmitted(EntriesSubmitted::from_competition(competition))
            }
            super::CompetitionState::ContractCreated => {
                CompetitionStatus::ContractCreated(ContractCreated::from_competition(competition))
            }
            super::CompetitionState::AwaitingSignatures => CompetitionStatus::AwaitingSignatures(
                AwaitingSignatures::from_competition(competition),
            ),
            super::CompetitionState::SigningComplete => {
                CompetitionStatus::SigningComplete(SigningComplete::from_competition(competition))
            }
            super::CompetitionState::FundingBroadcasted => CompetitionStatus::FundingBroadcasted(
                FundingBroadcasted::from_competition(competition),
            ),
            super::CompetitionState::FundingConfirmed => {
                CompetitionStatus::FundingConfirmed(FundingConfirmed::from_competition(competition))
            }
            super::CompetitionState::FundingSettled => {
                CompetitionStatus::FundingSettled(FundingSettled::from_competition(competition))
            }
            super::CompetitionState::AwaitingAttestation => CompetitionStatus::AwaitingAttestation(
                AwaitingAttestation::from_competition(competition),
            ),
            super::CompetitionState::Attested => {
                CompetitionStatus::Attested(Attested::from_competition(competition))
            }
            super::CompetitionState::ExpiryBroadcasted => CompetitionStatus::ExpiryBroadcasted(
                ExpiryBroadcasted::from_competition(competition),
            ),
            super::CompetitionState::OutcomeBroadcasted => CompetitionStatus::OutcomeBroadcasted(
                OutcomeBroadcasted::from_competition(competition),
            ),
            super::CompetitionState::DeltaBroadcasted => {
                CompetitionStatus::DeltaBroadcasted(DeltaBroadcasted::from_competition(competition))
            }
            super::CompetitionState::Completed => {
                CompetitionStatus::Completed(Completed::from_competition(competition))
            }
            super::CompetitionState::Failed => {
                CompetitionStatus::Failed(Failed::from_competition(competition))
            }
            super::CompetitionState::Cancelled => {
                CompetitionStatus::Cancelled(Cancelled::from_competition(competition))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::competitions::CreateEvent;

    fn competition() -> Competition {
        let start = OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap();
        Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + time::Duration::days(2),
            start_observation_date: start,
            end_observation_date: start + time::Duration::DAY,
            locations: vec!["KORD".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 3,
            entry_fee: 1_000,
            coordinator_fee_percentage: 10,
            total_competition_pool: 2_700,
            relative_locktime_block_delta: None,
            unlisted: false,
        })
    }

    /// What a runner logs as the next state must be what the next step loads.
    fn stored(status: CompetitionStatus) -> &'static str {
        CompetitionStatus::from(status.into_competition()).state_name()
    }

    #[test]
    fn a_competition_without_entries_loads_as_created() {
        assert_eq!(
            CompetitionStatus::from(competition()).state_name(),
            "created"
        );
    }

    #[test]
    fn the_first_entry_loads_a_competition_as_collecting_entries() {
        let mut competition = competition();
        competition.total_entries = 1;
        let status = CompetitionStatus::from(competition);
        assert_eq!(
            status.state_name(),
            "collecting_entries",
            "a competition with some entries reloads as collecting them, not as created"
        );
        assert_eq!(stored(status), "collecting_entries");
    }

    #[test]
    fn a_partly_filled_competition_stays_collecting_entries() {
        let mut competition = competition();
        competition.total_entries = 2;
        competition.total_paid_entries = 2;
        let status = CompetitionStatus::from(competition);
        assert_eq!(status.state_name(), "collecting_entries");
        assert_eq!(stored(status), "collecting_entries");
    }

    #[test]
    fn a_full_paid_competition_still_loads_as_awaiting_escrow() {
        let mut competition = competition();
        competition.total_entries = 3;
        competition.total_paid_entries = 3;
        assert_eq!(
            CompetitionStatus::from(competition).state_name(),
            "awaiting_escrow"
        );
    }
}
