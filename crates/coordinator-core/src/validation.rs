//! Validation utilities shared between server and client

use crate::{CoreError, ObservationChoice};

/// Validate observation choices
pub fn validate_observations(observations: &[ObservationChoice]) -> Result<(), CoreError> {
    if observations.is_empty() {
        return Err(CoreError::Validation(
            "at least one observation required".into(),
        ));
    }

    for obs in observations {
        if obs.source_id.is_empty() {
            return Err(CoreError::InvalidObservation(
                "source_id cannot be empty".into(),
            ));
        }
        if obs.metric.is_empty() {
            return Err(CoreError::InvalidObservation(
                "metric cannot be empty".into(),
            ));
        }
    }

    Ok(())
}
