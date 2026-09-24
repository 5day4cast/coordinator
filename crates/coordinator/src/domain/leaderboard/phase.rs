//! Where a competition stands, as players see it. Every page and fragment reads it from here.

use time::OffsetDateTime;

use crate::domain::{Competition, CompetitionState};

/// Where a competition stands, as players see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Taking entries; the observation window has not opened.
    Upcoming,
    /// The window opened before every entry was in and paid for. The competition will be
    /// cancelled and the held ticket payments refunded; nothing is scored.
    Unfilled,
    /// The observation window is open.
    Live,
    /// The window has closed; the oracle has not attested the result yet.
    AwaitingResult,
    /// The oracle has attested the result. Nothing changes any more.
    Scored,
    /// The oracle never attested, and the contract refunded every entry at its expiry.
    Expired,
    Cancelled,
    Failed,
}

impl Phase {
    /// The phase of `competition` from what the coordinator has stored. A leaderboard also
    /// counts a competition as scored once the oracle's own result is final; see
    /// [`Leaderboard::phase`](super::Leaderboard::phase).
    pub fn of(competition: &Competition, now: OffsetDateTime) -> Self {
        let event = &competition.event_submission;
        match competition.get_state() {
            CompetitionState::Cancelled => Phase::Cancelled,
            CompetitionState::Failed => Phase::Failed,
            _ if competition.attestation.is_some() => Phase::Scored,
            CompetitionState::ExpiryBroadcasted | CompetitionState::Completed => Phase::Expired,
            _ if now < event.start_observation_date => Phase::Upcoming,
            CompetitionState::Created => Phase::Unfilled,
            _ if now < event.end_observation_date => Phase::Live,
            _ => Phase::AwaitingResult,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::competitions::CreateEvent;
    use uuid::Uuid;

    fn start() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap()
    }

    fn competition(allowed: usize) -> Competition {
        Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start() + time::Duration::days(2),
            start_observation_date: start(),
            end_observation_date: start() + time::Duration::hours(18),
            locations: vec!["KPWM".into()],
            number_of_values_per_entry: 1,
            number_of_places_win: 1,
            total_allowed_entries: allowed,
            entry_fee: 1_000,
            coordinator_fee_percentage: 5,
            total_competition_pool: 2_000,
            relative_locktime_block_delta: None,
        })
    }

    fn filled() -> Competition {
        let mut competition = competition(2);
        competition.total_entries = 2;
        competition.total_paid_entries = 2;
        competition
    }

    #[test]
    fn a_filled_competition_follows_its_window() {
        let competition = filled();
        let at = |hours: i64| Phase::of(&competition, start() + time::Duration::hours(hours));
        assert_eq!(at(-1), Phase::Upcoming);
        assert_eq!(at(1), Phase::Live);
        assert_eq!(at(19), Phase::AwaitingResult);
    }

    #[test]
    fn a_competition_the_window_opens_on_before_it_fills_is_unfilled() {
        let mut competition = competition(3);
        competition.total_entries = 1;
        competition.total_paid_entries = 1;
        assert_eq!(
            Phase::of(&competition, start() - time::Duration::minutes(1)),
            Phase::Upcoming
        );
        assert_eq!(
            Phase::of(&competition, start() + time::Duration::minutes(1)),
            Phase::Unfilled
        );
        // Full but not all paid for is not filled either.
        competition.total_entries = 3;
        assert_eq!(
            Phase::of(&competition, start() + time::Duration::hours(20)),
            Phase::Unfilled
        );
        // Once cancelled, it says so.
        competition.cancelled_at = Some(start() + time::Duration::days(1));
        assert_eq!(
            Phase::of(&competition, start() + time::Duration::days(2)),
            Phase::Cancelled
        );
    }

    /// A competition cancelled or failed mid-window stops being live.
    #[test]
    fn stored_endings_win_over_the_clock() {
        let during = start() + time::Duration::hours(1);
        let mut cancelled = filled();
        cancelled.cancelled_at = Some(during);
        assert_eq!(Phase::of(&cancelled, during), Phase::Cancelled);
        let mut failed = filled();
        failed.failed_at = Some(during);
        assert_eq!(Phase::of(&failed, during), Phase::Failed);
        let mut expired = filled();
        expired.expiry_broadcasted_at = Some(start() + time::Duration::days(3));
        assert_eq!(
            Phase::of(&expired, start() + time::Duration::days(3)),
            Phase::Expired
        );
    }
}
