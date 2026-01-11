//! EntriesSubmitted state - entries submitted to oracle, ready to create contract.

use super::{CompetitionStatus, ContractCreated, HasCompetitionData};
use crate::domain::competitions::Competition;
use bdk_wallet::bitcoin::OutPoint;
use dlctix::ContractParameters;
use time::OffsetDateTime;
use uuid::Uuid;

/// State where all entries have been submitted to the oracle.
///
/// This is a "pass-through" state - the coordinator should immediately
/// proceed to create the contract parameters and funding PSBT.
#[derive(Debug, Clone)]
pub struct EntriesSubmitted {
    pub competition_id: Uuid,
    pub entries_submitted_at: OffsetDateTime,
    pub(crate) competition: Competition,
}

impl EntriesSubmitted {
    /// Reconstruct from an existing Competition loaded from DB.
    pub fn from_competition(competition: Competition) -> Self {
        Self {
            competition_id: competition.id,
            entries_submitted_at: competition
                .entries_submitted_at
                .unwrap_or_else(OffsetDateTime::now_utc),
            competition,
        }
    }

    /// Transition to ContractCreated after generating contract parameters and funding PSBT.
    ///
    /// # Arguments
    /// * `contract_params` - The generated contract parameters
    /// * `funding_outpoint` - The funding transaction outpoint
    /// * `funding_psbt_base64` - The unsigned funding PSBT as base64
    pub fn contract_created(
        mut self,
        contract_params: ContractParameters,
        funding_outpoint: OutPoint,
        funding_psbt_base64: String,
    ) -> CompetitionStatus {
        self.competition.contract_parameters = Some(contract_params);
        self.competition.funding_outpoint = Some(funding_outpoint);
        self.competition.funding_psbt_base64 = Some(funding_psbt_base64);
        self.competition.contracted_at = Some(OffsetDateTime::now_utc());
        CompetitionStatus::ContractCreated(ContractCreated::from_competition(self.competition))
    }
}

impl HasCompetitionData for EntriesSubmitted {
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
