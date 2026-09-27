//! The coordinator's fee on each entry, as an exact integer.
//!
//! Operators set the fee as a percentage with up to two decimals (2.5%). It is
//! stored as basis points, hundredths of a percent (250), so the ticket price
//! is integer arithmetic with no floating-point rounding.
//!
//! JSON carries `coordinator_fee_basis_points` plus `coordinator_fee_percentage`
//! for display: a whole number when the fee is a whole percent (10), a decimal
//! otherwise (2.5). Events stored before basis points only have a whole
//! `coordinator_fee_percentage`; reading one converts it (10 -> 1000).

use serde::{de, ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Basis points in 100%.
const FULL: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoordinatorFee {
    basis_points: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoordinatorFeeError {
    #[error("the coordinator fee must be between 0% and 100%")]
    OutOfRange,
    #[error("the coordinator fee must be a percentage with at most two decimals, such as 2.5")]
    Invalid,
    #[error("coordinator_fee_basis_points and coordinator_fee_percentage disagree")]
    Mismatch,
}

impl CoordinatorFee {
    pub const ZERO: Self = Self { basis_points: 0 };

    pub fn from_basis_points(basis_points: u32) -> Result<Self, CoordinatorFeeError> {
        if u64::from(basis_points) > FULL {
            return Err(CoordinatorFeeError::OutOfRange);
        }
        Ok(Self { basis_points })
    }

    /// A whole percent written in code; panics above 100%.
    pub const fn whole_percent(percent: u32) -> Self {
        assert!(percent <= 100, "coordinator fee above 100%");
        Self {
            basis_points: percent * 100,
        }
    }

    /// A whole percent, as stored before basis points.
    pub fn from_whole_percent(percent: u64) -> Result<Self, CoordinatorFeeError> {
        let basis_points = percent
            .checked_mul(100)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(CoordinatorFeeError::OutOfRange)?;
        Self::from_basis_points(basis_points)
    }

    /// Parse a percentage such as "5", "2.5", or "2.75" exactly, without
    /// going through a float. More than two decimals is an error, not rounded.
    pub fn parse_percent(text: &str) -> Result<Self, CoordinatorFeeError> {
        let text = text.trim();
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        if (whole.is_empty() && fraction.is_empty())
            || !digits(whole)
            || !digits(fraction)
            || fraction.len() > 2
            || whole.len() > 3
        {
            return Err(CoordinatorFeeError::Invalid);
        }
        let whole: u32 = if whole.is_empty() {
            0
        } else {
            whole.parse().map_err(|_| CoordinatorFeeError::Invalid)?
        };
        let fraction: u32 = format!("{fraction:0<2}")
            .parse()
            .map_err(|_| CoordinatorFeeError::Invalid)?;
        Self::from_basis_points(whole * 100 + fraction)
    }

    pub fn basis_points(self) -> u32 {
        self.basis_points
    }

    /// The fee on an entry, in sats: entry x basis points / 10,000, with
    /// exact halves rounded up. The earlier float formula rounded some exact
    /// halves down (50 sats at 3%: 0.03 is not exact in binary).
    pub fn fee_for(self, entry_sats: u64) -> u64 {
        let scaled = u128::from(entry_sats) * u128::from(self.basis_points) + u128::from(FULL / 2);
        (scaled / u128::from(FULL)) as u64
    }

    /// The percentage for people: "10", "2.5", "2.75".
    pub fn percent_text(self) -> String {
        let whole = self.basis_points / 100;
        match self.basis_points % 100 {
            0 => whole.to_string(),
            hundredths if hundredths.is_multiple_of(10) => format!("{whole}.{}", hundredths / 10),
            hundredths => format!("{whole}.{hundredths:02}"),
        }
    }
}

impl fmt::Display for CoordinatorFee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.percent_text())
    }
}

/// Written into the enclosing event with `#[serde(flatten)]`.
impl Serialize for CoordinatorFee {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("coordinator_fee_basis_points", &self.basis_points)?;
        if self.basis_points.is_multiple_of(100) {
            // A whole number, so clients that read an integer percent still work.
            map.serialize_entry("coordinator_fee_percentage", &(self.basis_points / 100))?;
        } else {
            map.serialize_entry(
                "coordinator_fee_percentage",
                &(f64::from(self.basis_points) / 100.0),
            )?;
        }
        map.end()
    }
}

#[derive(Deserialize)]
struct FeeFields {
    #[serde(default)]
    coordinator_fee_basis_points: Option<u32>,
    #[serde(default)]
    coordinator_fee_percentage: Option<serde_json::Number>,
}

fn percent_to_basis_points(percent: &serde_json::Number) -> Result<u32, CoordinatorFeeError> {
    if let Some(whole) = percent.as_u64() {
        return CoordinatorFee::from_whole_percent(whole).map(CoordinatorFee::basis_points);
    }
    let value = percent.as_f64().ok_or(CoordinatorFeeError::Invalid)?;
    let scaled = value * 100.0;
    let rounded = scaled.round();
    if !value.is_finite() || value < 0.0 || (scaled - rounded).abs() > 1e-6 {
        return Err(CoordinatorFeeError::Invalid);
    }
    CoordinatorFee::from_basis_points(rounded as u32).map(CoordinatorFee::basis_points)
}

impl<'de> Deserialize<'de> for CoordinatorFee {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = FeeFields::deserialize(deserializer)?;
        let from_percent = fields
            .coordinator_fee_percentage
            .as_ref()
            .map(percent_to_basis_points)
            .transpose()
            .map_err(de::Error::custom)?;
        let basis_points = match (fields.coordinator_fee_basis_points, from_percent) {
            (Some(basis_points), Some(percent)) if basis_points != percent => {
                return Err(de::Error::custom(CoordinatorFeeError::Mismatch));
            }
            (Some(basis_points), _) => basis_points,
            (None, Some(percent)) => percent,
            (None, None) => return Err(de::Error::missing_field("coordinator_fee_basis_points")),
        };
        CoordinatorFee::from_basis_points(basis_points).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Event {
        entry_fee: u64,
        #[serde(flatten)]
        coordinator_fee: CoordinatorFee,
    }

    fn fee(basis_points: u32) -> CoordinatorFee {
        CoordinatorFee::from_basis_points(basis_points).unwrap()
    }

    #[test]
    fn parses_percentages_exactly() {
        for (text, basis_points) in [
            ("0", 0),
            ("5", 500),
            ("2.5", 250),
            ("2.50", 250),
            ("2.75", 275),
            ("0.01", 1),
            (".5", 50),
            ("10.", 1000),
            ("100", 10_000),
            (" 3 ", 300),
        ] {
            assert_eq!(
                CoordinatorFee::parse_percent(text).unwrap().basis_points(),
                basis_points,
                "{text}"
            );
        }
        for text in [
            "", ".", "2.555", "-1", "1e2", "2,5", "abc", "100.01", "1000",
        ] {
            assert!(CoordinatorFee::parse_percent(text).is_err(), "{text}");
        }
    }

    #[test]
    fn fee_matches_the_float_formula_except_exact_halves() {
        // Whole percents give the old f64 formula's amounts, except where the
        // exact fee ends in .5: the float formula sometimes rounded those down.
        for entry in [0u64, 1, 49, 50, 333, 1000, 5000, 12_345, 1_000_000] {
            for percent in 0..=100u64 {
                let new = CoordinatorFee::from_whole_percent(percent)
                    .unwrap()
                    .fee_for(entry);
                let exact_half = entry * percent % 100 == 50;
                let old = (entry as f64 * (percent as f64 / 100.0)).round() as u64;
                if exact_half {
                    assert_eq!(
                        new,
                        (entry * percent).div_ceil(100),
                        "entry {entry} at {percent}%"
                    );
                } else {
                    assert_eq!(new, old, "entry {entry} at {percent}%");
                }
            }
        }
        assert_eq!(
            CoordinatorFee::from_whole_percent(3).unwrap().fee_for(50),
            2
        );
        assert_eq!(fee(250).fee_for(5000), 125);
        assert_eq!(fee(250).fee_for(333), 8); // 8.325
        assert_eq!(fee(250).fee_for(20), 1); // 0.5 rounds up
        assert_eq!(fee(10_000).fee_for(u64::MAX), u64::MAX);
    }

    #[test]
    fn percent_text_trims_zeros() {
        assert_eq!(fee(1000).percent_text(), "10");
        assert_eq!(fee(250).percent_text(), "2.5");
        assert_eq!(fee(275).percent_text(), "2.75");
        assert_eq!(fee(5).percent_text(), "0.05");
        assert_eq!(fee(250).to_string(), "2.5%");
    }

    #[test]
    fn serializes_basis_points_and_a_readable_percentage() {
        let whole = serde_json::to_value(Event {
            entry_fee: 1000,
            coordinator_fee: fee(1000),
        })
        .unwrap();
        assert_eq!(
            whole,
            json!({"entry_fee": 1000, "coordinator_fee_basis_points": 1000, "coordinator_fee_percentage": 10})
        );
        // An integer, so a client that reads a whole percent still parses it.
        assert!(whole["coordinator_fee_percentage"].is_u64());
        let fractional = serde_json::to_value(Event {
            entry_fee: 1000,
            coordinator_fee: fee(250),
        })
        .unwrap();
        assert_eq!(fractional["coordinator_fee_basis_points"], 250);
        assert_eq!(fractional["coordinator_fee_percentage"], 2.5);
    }

    #[test]
    fn reads_stored_whole_percent_events() {
        let event: Event =
            serde_json::from_value(json!({"entry_fee": 1000, "coordinator_fee_percentage": 10}))
                .unwrap();
        assert_eq!(event.coordinator_fee.basis_points(), 1000);
    }

    #[test]
    fn reads_basis_points_and_decimal_percentages() {
        let event: Event =
            serde_json::from_value(json!({"entry_fee": 1, "coordinator_fee_basis_points": 250}))
                .unwrap();
        assert_eq!(event.coordinator_fee, fee(250));
        let event: Event =
            serde_json::from_value(json!({"entry_fee": 1, "coordinator_fee_percentage": 2.5}))
                .unwrap();
        assert_eq!(event.coordinator_fee, fee(250));
        let both: Event = serde_json::from_value(
            json!({"entry_fee": 1, "coordinator_fee_basis_points": 250, "coordinator_fee_percentage": 2.5})).unwrap();
        assert_eq!(both.coordinator_fee, fee(250));
        let round_trip: Event =
            serde_json::from_str(&serde_json::to_string(&both).unwrap()).unwrap();
        assert_eq!(round_trip, both);
    }

    #[test]
    fn rejects_bad_fees() {
        for value in [
            json!({"entry_fee": 1}),
            json!({"entry_fee": 1, "coordinator_fee_basis_points": 10_001}),
            json!({"entry_fee": 1, "coordinator_fee_percentage": 101}),
            json!({"entry_fee": 1, "coordinator_fee_percentage": 2.555}),
            json!({"entry_fee": 1, "coordinator_fee_percentage": -1}),
            json!({"entry_fee": 1, "coordinator_fee_basis_points": 250, "coordinator_fee_percentage": 3}),
        ] {
            assert!(
                serde_json::from_value::<Event>(value.clone()).is_err(),
                "{value}"
            );
        }
    }
}
