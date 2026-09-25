//! How a pick stands while its competition's observation window runs, and how entries rank.
//!
//! Final scoring belongs to the oracle (noaa-oracle `scoring.rs` and `sources/noaa.rs`). This
//! module repeats its definitions so the leaderboard can say what a pick would score if the
//! window ended now, and how far it can still move:
//!
//! - A reading is the forecast ("baseline") and the value observed over the window. The window's
//!   observations only move one way as it runs: the high and the wind are running maxima, the low
//!   a running minimum.
//! - Temperatures compare whole degrees: both values are rounded before any comparison. Wind
//!   compares exact knots.
//! - Par earns 20 when the values are equal; Over earns 10 when the observation is above the
//!   forecast, Under 10 when it is below. A missing value earns nothing.

use std::cmp::Ordering;

use crate::infra::oracle::ValueOptions;

pub const OVER_OR_UNDER_POINTS: u64 = 10;
pub const PAR_POINTS: u64 = 20;

/// A metric an entry can pick, as the oracle names and scores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Metric {
    TempHigh,
    TempLow,
    WindSpeed,
}

impl Metric {
    pub const ALL: [Metric; 3] = [Metric::TempHigh, Metric::TempLow, Metric::WindSpeed];

    /// The oracle's metric id.
    pub fn id(self) -> &'static str {
        match self {
            Metric::TempHigh => "temp_high",
            Metric::TempLow => "temp_low",
            Metric::WindSpeed => "wind_speed",
        }
    }

    /// Which way the observed value can still move as the window runs.
    pub fn trend(self) -> Trend {
        match self {
            Metric::TempHigh | Metric::WindSpeed => Trend::Rises,
            Metric::TempLow => Trend::Falls,
        }
    }

    pub fn par_rule(self) -> ParRule {
        match self {
            Metric::TempHigh | Metric::TempLow => ParRule::Rounded,
            Metric::WindSpeed => ParRule::Exact,
        }
    }
}

/// How the observation over a window can change as more of the window is observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    /// A running maximum or total: it never falls.
    Rises,
    /// A running minimum: it never rises.
    Falls,
}

/// How values compare, as the oracle's `ParRule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParRule {
    /// Par when the values are equal.
    Exact,
    /// Both values are rounded to whole units before every comparison.
    Rounded,
}

/// Forecast and observation in the form they are compared, or `None` when either is missing.
fn comparable(rule: ParRule, forecast: Option<f64>, observed: Option<f64>) -> Option<(f64, f64)> {
    let (forecast, observed) = (forecast?, observed?);
    if !forecast.is_finite() || !observed.is_finite() {
        return None;
    }
    Some(match rule {
        ParRule::Exact => (forecast, observed),
        ParRule::Rounded => (forecast.round(), observed.round()),
    })
}

/// Points for a pick, exactly as the oracle scores it.
pub fn points(
    pick: &ValueOptions,
    metric: Metric,
    forecast: Option<f64>,
    observed: Option<f64>,
) -> u64 {
    let Some((forecast, observed)) = comparable(metric.par_rule(), forecast, observed) else {
        return 0;
    };
    match (pick, observed.partial_cmp(&forecast)) {
        (ValueOptions::Par, Some(Ordering::Equal)) => PAR_POINTS,
        (ValueOptions::Over, Some(Ordering::Greater))
        | (ValueOptions::Under, Some(Ordering::Less)) => OVER_OR_UNDER_POINTS,
        _ => 0,
    }
}

/// Where a pick stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PickState {
    /// Nothing to compare yet: no forecast, or nothing observed in the window so far.
    Pending,
    /// Right, and the rest of the window cannot change that.
    LockedIn,
    /// Right if the window ended now, but it can still change.
    OnTrack,
    /// Wrong if the window ended now, but it can still come right.
    OffTrack,
    /// Wrong, and the rest of the window cannot change that.
    Out,
    /// The window has closed, and the oracle has not attested the result yet. The points say
    /// what the observations so far score; the oracle's own reading can still differ.
    AwaitingResult,
    /// The oracle has attested the result: the points are the oracle's.
    Final,
}

/// How `pick` stands against the observation so far, while the window is open.
pub fn pick_state(
    pick: &ValueOptions,
    metric: Metric,
    forecast: Option<f64>,
    observed: Option<f64>,
) -> PickState {
    let Some((forecast, observed)) = comparable(metric.par_rule(), forecast, observed) else {
        return PickState::Pending;
    };
    let Some(now) = observed.partial_cmp(&forecast) else {
        return PickState::Pending;
    };
    use Ordering::{Equal, Greater, Less};
    use PickState::{LockedIn, OffTrack, OnTrack, Out};
    match (metric.trend(), pick) {
        // The final value is at least the value so far.
        (Trend::Rises, ValueOptions::Over) => match now {
            Greater => LockedIn,
            Equal | Less => OffTrack,
        },
        (Trend::Rises, ValueOptions::Under) => match now {
            Less => OnTrack,
            Equal | Greater => Out,
        },
        (Trend::Rises, ValueOptions::Par) => match now {
            Equal => OnTrack,
            Less => OffTrack,
            Greater => Out,
        },
        // The final value is at most the value so far.
        (Trend::Falls, ValueOptions::Under) => match now {
            Less => LockedIn,
            Equal | Greater => OffTrack,
        },
        (Trend::Falls, ValueOptions::Over) => match now {
            Greater => OnTrack,
            Equal | Less => Out,
        },
        (Trend::Falls, ValueOptions::Par) => match now {
            Equal => OnTrack,
            Greater => OffTrack,
            Less => Out,
        },
    }
}

/// A place in a ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    /// 1 for the best score. Equal scores share a rank, and the next rank skips: 1, 2, 2, 4.
    pub rank: usize,
    /// Points.
    pub score: u64,
}

/// Standings for `scores`, in the same order. Equal scores share a rank.
pub fn standings(scores: &[u64]) -> Vec<Standing> {
    scores
        .iter()
        .map(|&score| Standing {
            rank: 1 + scores.iter().filter(|&&other| other > score).count(),
            score,
        })
        .collect()
}

/// The order the oracle pays in: highest score first, equal scores by entry id (the entry the
/// oracle holds, a UUIDv7, so the earlier entry wins a tie). Returns indices into `entries`.
pub fn payout_order(entries: &[(u64, uuid::Uuid)]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&index| (std::cmp::Reverse(entries[index].0), entries[index].1));
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use ValueOptions::{Over, Par, Under};

    /// The oracle's own cases (noaa-oracle `scoring.rs`), for the rules the coordinator's metrics use.
    #[test]
    fn points_match_the_oracle() {
        assert_eq!(
            points(&Par, Metric::WindSpeed, Some(10.0), Some(10.0)),
            PAR_POINTS
        );
        assert_eq!(points(&Par, Metric::WindSpeed, Some(10.0), Some(11.0)), 0);
        assert_eq!(
            points(&Over, Metric::WindSpeed, Some(10.0), Some(11.0)),
            OVER_OR_UNDER_POINTS
        );
        assert_eq!(points(&Over, Metric::WindSpeed, Some(10.0), Some(9.0)), 0);
        assert_eq!(
            points(&Under, Metric::WindSpeed, Some(10.0), Some(9.0)),
            OVER_OR_UNDER_POINTS
        );
        // Temperatures compare whole degrees, for Over and Under as well as Par.
        assert_eq!(
            points(&Par, Metric::TempHigh, Some(70.0), Some(70.4)),
            PAR_POINTS
        );
        assert_eq!(points(&Over, Metric::TempHigh, Some(70.0), Some(70.4)), 0);
        assert_eq!(
            points(&Over, Metric::TempHigh, Some(70.0), Some(70.5)),
            OVER_OR_UNDER_POINTS
        );
        assert_eq!(
            points(&Under, Metric::TempLow, Some(41.0), Some(40.4)),
            OVER_OR_UNDER_POINTS
        );
        for missing in [
            (None, Some(1.0)),
            (Some(1.0), None),
            (Some(f64::NAN), Some(1.0)),
            (Some(1.0), Some(f64::INFINITY)),
        ] {
            for pick in [Over, Par, Under] {
                assert_eq!(points(&pick, Metric::TempHigh, missing.0, missing.1), 0);
                assert_eq!(
                    pick_state(&pick, Metric::TempHigh, missing.0, missing.1),
                    PickState::Pending
                );
            }
        }
    }

    /// Every metric × pick against an observation below, at, and above the forecast of 70.
    #[test]
    fn a_pick_state_follows_what_the_rest_of_the_window_can_still_do() {
        use PickState::{LockedIn, OffTrack, OnTrack, Out};
        let forecast = Some(70.0);
        // (observed so far, expected state for Over, Par, Under)
        let rising: [(f64, [PickState; 3]); 3] = [
            (69.0, [OffTrack, OffTrack, OnTrack]),
            (70.0, [OffTrack, OnTrack, Out]),
            (71.0, [LockedIn, Out, Out]),
        ];
        let falling: [(f64, [PickState; 3]); 3] = [
            (69.0, [Out, Out, LockedIn]),
            (70.0, [Out, OnTrack, OffTrack]),
            (71.0, [OnTrack, OffTrack, OffTrack]),
        ];
        for (metric, cases) in [
            (Metric::TempHigh, rising),
            (Metric::WindSpeed, rising),
            (Metric::TempLow, falling),
        ] {
            for (observed, expected) in cases {
                for (pick, expected) in [Over, Par, Under].iter().zip(expected) {
                    assert_eq!(
                        pick_state(pick, metric, forecast, Some(observed)),
                        expected,
                        "{metric:?} {pick:?} with {observed} observed against 70"
                    );
                }
            }
        }
    }

    /// The boundary between Par and its neighbours is whole-degree rounding for temperatures.
    #[test]
    fn temperature_states_change_at_the_rounding_boundary() {
        use PickState::{LockedIn, OffTrack, OnTrack, Out};
        let state = |pick, metric, observed| pick_state(pick, metric, Some(70.0), Some(observed));
        // A high of 70.49 rounds to the forecast: Par holds, Over has not happened yet.
        assert_eq!(state(&Par, Metric::TempHigh, 70.49), OnTrack);
        assert_eq!(state(&Over, Metric::TempHigh, 70.49), OffTrack);
        assert_eq!(state(&Under, Metric::TempHigh, 70.49), Out);
        // 70.5 rounds up: Over is certain and Par can no longer happen.
        assert_eq!(state(&Over, Metric::TempHigh, 70.5), LockedIn);
        assert_eq!(state(&Par, Metric::TempHigh, 70.5), Out);
        // A high of 69.5 already rounds to 70.
        assert_eq!(state(&Under, Metric::TempHigh, 69.5), Out);
        assert_eq!(state(&Under, Metric::TempHigh, 69.49), OnTrack);
        // A low of 69.5 rounds to 70: Under is not certain until it rounds below.
        assert_eq!(state(&Under, Metric::TempLow, 69.5), OffTrack);
        assert_eq!(state(&Under, Metric::TempLow, 69.49), LockedIn);
        assert_eq!(state(&Over, Metric::TempLow, 70.5), OnTrack);
        assert_eq!(state(&Over, Metric::TempLow, 70.49), Out);
        // Wind compares exact knots, so no rounding moves it.
        assert_eq!(
            pick_state(&Par, Metric::WindSpeed, Some(10.0), Some(10.0)),
            OnTrack
        );
        assert_eq!(
            pick_state(&Over, Metric::WindSpeed, Some(10.0), Some(10.4)),
            LockedIn
        );
    }

    /// A state never contradicts the points the pick would score if the window ended now.
    #[test]
    fn states_agree_with_points() {
        for metric in Metric::ALL {
            for observed in [68.0, 69.6, 70.0, 70.3, 70.5, 72.0] {
                for pick in [Over, Par, Under] {
                    let scored = points(&pick, metric, Some(70.0), Some(observed)) > 0;
                    let state = pick_state(&pick, metric, Some(70.0), Some(observed));
                    let right_now = matches!(state, PickState::LockedIn | PickState::OnTrack);
                    assert_eq!(scored, right_now, "{metric:?} {pick:?} at {observed}");
                }
            }
        }
    }

    #[test]
    fn equal_scores_share_a_rank() {
        let ranks: Vec<usize> = standings(&[30, 50, 30, 10, 50])
            .iter()
            .map(|standing| standing.rank)
            .collect();
        assert_eq!(ranks, vec![3, 1, 3, 5, 1]);
        assert!(standings(&[]).is_empty());
    }

    #[test]
    fn the_payout_order_breaks_ties_by_the_earlier_entry_id() {
        let early = Uuid::from_u128(1);
        let late = Uuid::from_u128(2);
        let third = Uuid::from_u128(3);
        assert_eq!(
            payout_order(&[(20, late), (20, early), (40, third)]),
            vec![2, 1, 0]
        );
    }
}
