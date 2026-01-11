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

    /// Transition to Failed state from any state.
    pub fn fail(self, error: CompetitionError) -> CompetitionStatus {
        let competition_id = self.competition_id();
        let previous_state = self.state_name().to_string();
        let competition = self.into_competition();
        CompetitionStatus::Failed(Failed {
            competition_id,
            failed_at: OffsetDateTime::now_utc(),
            error,
            previous_state,
            competition: Some(competition),
        })
    }

    /// Transition to Cancelled state from any state.
    pub fn cancel(self, reason: String) -> CompetitionStatus {
        let competition_id = self.competition_id();
        let previous_state = self.state_name().to_string();
        let competition = self.into_competition();
        CompetitionStatus::Cancelled(Cancelled {
            competition_id,
            cancelled_at: OffsetDateTime::now_utc(),
            reason,
            previous_state,
            competition: Some(competition),
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
            super::CompetitionState::NoncesCollected
            | super::CompetitionState::AggregateNoncesGenerated
            | super::CompetitionState::PartialSignaturesCollected => {
                // All MuSig-related states map to AwaitingSignatures
                // (Keymeld handles this internally)
                CompetitionStatus::AwaitingSignatures(AwaitingSignatures::from_competition(
                    competition,
                ))
            }
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
