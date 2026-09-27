//! The oracle's signed statement of one event, checked without trusting the coordinator.
//!
//! A DLC's locking points depend only on the oracle key, the event's nonce point, and each
//! outcome's message, and a message names no event. A queued competition creates each pool's event
//! at kickoff, so no player sees it; the verifier in Keymeld's enclave checks it instead. The oracle
//! signs a statement binding the nonce point to the event's id, outcomes, and terms, and the
//! verifier derives the locking points from it.
//!
//! This is the noaa-oracle encoding, specified in its `docs/attestation.md` ("Signed event
//! statement") and implemented there as `oracle::statement`. It is repeated here because the
//! enclave cannot build the oracle's server crate. The test vector is the oracle's.

use dlctix::{
    attestation_locking_point,
    musig2::secp256k1::{schnorr::Signature, Secp256k1, XOnlyPublicKey},
    secp::{MaybePoint, Point},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;
use uuid::Uuid;

/// Domain tag of the statement's BIP340 tagged hash.
pub const STATEMENT_TAG: &[u8] = b"noaa-oracle/statement/v1";

/// What the oracle attests about one event, as it signs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    pub event_id: Uuid,
    /// When the oracle attests the outcome, UNIX seconds, rounded down
    pub signing_date: i64,
    /// The DLC expiry the announcement carries, UNIX seconds
    pub expiry: u32,
    /// Public nonce point `R` the attestation will be made with
    pub nonce_point: Point,
    pub outcomes: Outcomes,
    pub terms: Terms,
}

/// How an event's outcomes are listed. An unknown kind is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcomes {
    /// Every ordered choice of winners among the entries, then the refund-all outcome
    Ranking(RankingOutcomes),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankingOutcomes {
    pub number_of_places_win: u32,
    /// Every entry, in id order: entry `i` is outcome index `i`
    pub entry_ids: Vec<Uuid>,
}

/// What an event measures, and how it is judged. An unknown kind is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Terms {
    /// Predictions scored against values observed over a window, such as NOAA weather
    Observation(ObservationTerms),
}

/// The terms of an event scored against observed values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationTerms {
    pub source: String,
    /// UNIX seconds, rounded down
    pub start_observation_date: i64,
    /// UNIX seconds, rounded down
    pub end_observation_date: i64,
    /// What the source observes: NOAA station ids for `noaa_weather`
    pub targets: Vec<String>,
    pub scoring_fields: Vec<String>,
    pub number_of_values_per_entry: u32,
    pub scoring_rules: ScoringRules,
    /// For `lines` rules, the line each target and metric scores against, sorted by target then
    /// metric; empty for `fixed` rules
    pub lines: Vec<LineTerms>,
}

/// How an event scores its picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoringRules {
    Fixed,
    Lines,
}

impl ScoringRules {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Lines => "lines",
        }
    }
}

/// A line an event scores one target and metric against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineTerms {
    pub target: String,
    pub metric: String,
    pub lower: f64,
    pub upper: f64,
    pub window_hours: u32,
}

/// A [`Statement`] and the oracle's signature over its digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedStatement {
    pub statement: Statement,
    /// BIP340 signature by the oracle key over [`Statement::digest`], hex
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StatementError {
    #[error("the statement signature is not a BIP340 signature")]
    Malformed,
    #[error("the statement is not signed by this oracle key")]
    BadSignature,
    #[error("a statement field is too long to encode")]
    TooLong,
}

impl Statement {
    /// The message each outcome attests, in announcement order: every ordered choice of winners,
    /// then refund-all. Each winner index is an 8-byte big-endian integer.
    pub fn outcome_messages(&self) -> Vec<Vec<u8>> {
        match &self.outcomes {
            Outcomes::Ranking(ranking) => ranking_outcomes(
                ranking.entry_ids.len(),
                ranking.number_of_places_win as usize,
            )
            .iter()
            .map(|winners| {
                winners
                    .iter()
                    .flat_map(|&index| (index as u64).to_be_bytes())
                    .collect()
            })
            .collect(),
        }
    }

    /// The locking points of every outcome under `oracle`, in announcement order.
    pub fn locking_points(&self, oracle: impl Into<Point>) -> Vec<MaybePoint> {
        let oracle = oracle.into();
        self.outcome_messages()
            .iter()
            .map(|message| attestation_locking_point(oracle, self.nonce_point, message))
            .collect()
    }

    /// The bytes the digest commits to: the core, then each part's kind and fields.
    pub fn message(&self) -> Result<Vec<u8>, StatementError> {
        let mut message = Vec::with_capacity(256);
        message.extend_from_slice(self.event_id.as_bytes());
        message.extend_from_slice(&self.signing_date.to_be_bytes());
        message.extend_from_slice(&self.expiry.to_be_bytes());
        message.extend_from_slice(&self.nonce_point.serialize());
        match &self.outcomes {
            Outcomes::Ranking(ranking) => {
                put_string(&mut message, "ranking")?;
                message.extend_from_slice(&ranking.number_of_places_win.to_be_bytes());
                put_count(&mut message, ranking.entry_ids.len())?;
                for entry in &ranking.entry_ids {
                    message.extend_from_slice(entry.as_bytes());
                }
            }
        }
        match &self.terms {
            Terms::Observation(terms) => {
                put_string(&mut message, "observation")?;
                terms.encode(&mut message)?;
            }
        }
        Ok(message)
    }

    /// The BIP340 tagged hash of [`Self::message`] under [`STATEMENT_TAG`].
    pub fn digest(&self) -> Result<[u8; 32], StatementError> {
        let tag = Sha256::digest(STATEMENT_TAG);
        Ok(Sha256::new()
            .chain_update(tag)
            .chain_update(tag)
            .chain_update(self.message()?)
            .finalize()
            .into())
    }
}

impl ObservationTerms {
    fn encode(&self, message: &mut Vec<u8>) -> Result<(), StatementError> {
        put_string(message, &self.source)?;
        message.extend_from_slice(&self.start_observation_date.to_be_bytes());
        message.extend_from_slice(&self.end_observation_date.to_be_bytes());
        put_strings(message, &self.targets)?;
        put_strings(message, &self.scoring_fields)?;
        message.extend_from_slice(&self.number_of_values_per_entry.to_be_bytes());
        put_string(message, self.scoring_rules.as_str())?;
        put_count(message, self.lines.len())?;
        for line in &self.lines {
            put_string(message, &line.target)?;
            put_string(message, &line.metric)?;
            message.extend_from_slice(&line.lower.to_be_bytes());
            message.extend_from_slice(&line.upper.to_be_bytes());
            message.extend_from_slice(&line.window_hours.to_be_bytes());
        }
        Ok(())
    }

    /// Whether two sets of terms are the same, bit for bit. Lines compare as the exact doubles an
    /// event scores against, so `-0.0` and `0.0` differ, as they do in the signed bytes.
    pub fn same_as(&self, other: &Self) -> bool {
        let (mut mine, mut theirs) = (Vec::new(), Vec::new());
        self.encode(&mut mine).is_ok() && other.encode(&mut theirs).is_ok() && mine == theirs
    }
}

impl SignedStatement {
    /// Checks the signature against the oracle's x-only public key.
    pub fn verify(&self, oracle: &XOnlyPublicKey) -> Result<(), StatementError> {
        let signature =
            Signature::from_str(&self.signature).map_err(|_| StatementError::Malformed)?;
        Secp256k1::verification_only()
            .verify_schnorr(&signature, &self.statement.digest()?, oracle)
            .map_err(|_| StatementError::BadSignature)
    }
}

/// Every announced ranking outcome, in the oracle's order: the ordered choices of `places`
/// winners among `entries`, then the refund-all outcome listing every entry.
pub fn ranking_outcomes(entries: usize, places: usize) -> Vec<Vec<usize>> {
    let mut outcomes = Vec::new();
    permutations(entries, places, &mut Vec::new(), &mut outcomes);
    outcomes.push((0..entries).collect());
    outcomes
}

/// Lexicographic ordered choices, the order of itertools' `permutations`, which the oracle uses.
fn permutations(n: usize, k: usize, prefix: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    if prefix.len() == k {
        out.push(prefix.clone());
        return;
    }
    for index in 0..n {
        if !prefix.contains(&index) {
            prefix.push(index);
            permutations(n, k, prefix, out);
            prefix.pop();
        }
    }
}

fn put_count(message: &mut Vec<u8>, count: usize) -> Result<(), StatementError> {
    let count = u16::try_from(count).map_err(|_| StatementError::TooLong)?;
    message.extend_from_slice(&count.to_be_bytes());
    Ok(())
}

fn put_string(message: &mut Vec<u8>, value: &str) -> Result<(), StatementError> {
    put_count(message, value.len())?;
    message.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_strings(message: &mut Vec<u8>, values: &[String]) -> Result<(), StatementError> {
    put_count(message, values.len())?;
    for value in values {
        put_string(message, value)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dlctix::musig2::secp256k1::{Keypair, SecretKey};

    pub(crate) fn statement() -> Statement {
        let start = 1_790_000_000;
        Statement {
            event_id: Uuid::from_str("01926f3a-0000-7000-8000-0000000000aa").unwrap(),
            signing_date: start + 2 * 86_400,
            expiry: (start + 3 * 86_400) as u32,
            // 2·G
            nonce_point: Point::from_hex(
                "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            )
            .unwrap(),
            outcomes: Outcomes::Ranking(RankingOutcomes {
                number_of_places_win: 1,
                entry_ids: (1..=3)
                    .map(|entry| Uuid::from_u128(0x01926f3a_0000_7000_8000_000000000000 | entry))
                    .collect(),
            }),
            terms: Terms::Observation(ObservationTerms {
                source: "noaa_weather".into(),
                start_observation_date: start,
                end_observation_date: start + 86_400,
                targets: vec!["KORD".into(), "KSAW".into()],
                scoring_fields: vec!["temp_high".into(), "temp_low".into(), "wind_speed".into()],
                number_of_values_per_entry: 3,
                scoring_rules: ScoringRules::Lines,
                lines: vec![
                    LineTerms {
                        target: "KORD".into(),
                        metric: "temp_high".into(),
                        lower: -2.5,
                        upper: -0.5,
                        window_hours: 24,
                    },
                    LineTerms {
                        target: "KSAW".into(),
                        metric: "wind_speed".into(),
                        lower: 1.5,
                        upper: 4.5,
                        window_hours: 24,
                    },
                ],
            }),
        }
    }

    pub(crate) fn sign(statement: Statement, secret: [u8; 32]) -> SignedStatement {
        let keypair = Keypair::from_secret_key(
            &Secp256k1::new(),
            &SecretKey::from_byte_array(secret).unwrap(),
        );
        let signature = Secp256k1::new().sign_schnorr_no_aux_rand(&statement.digest().unwrap(), &keypair);
        SignedStatement {
            statement,
            signature: signature.to_string(),
        }
    }

    /// The oracle's reference vector, from an independent implementation of the encoding.
    #[test]
    fn digest_matches_the_oracle_vector() {
        assert_eq!(statement().message().unwrap().len(), 304);
        assert_eq!(
            hex::encode(statement().digest().unwrap()),
            "2b37e13098210b2216a8b101d8a6e16420101c9acb954c1a53d43c0e990303f4"
        );
    }

    #[test]
    fn outcomes_follow_the_oracle_order() {
        assert_eq!(
            ranking_outcomes(3, 1),
            vec![vec![0], vec![1], vec![2], vec![0, 1, 2]]
        );
        assert_eq!(
            ranking_outcomes(3, 2),
            vec![
                vec![0, 1],
                vec![0, 2],
                vec![1, 0],
                vec![1, 2],
                vec![2, 0],
                vec![2, 1],
                vec![0, 1, 2]
            ]
        );
        let messages = statement().outcome_messages();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1], 1u64.to_be_bytes().to_vec());
        assert_eq!(messages[3].len(), 24);
    }

    #[test]
    fn signatures_verify_only_for_the_statement_and_key() {
        let signed = sign(statement(), [7; 32]);
        let key = |secret: [u8; 32]| {
            Keypair::from_secret_key(
                &Secp256k1::new(),
                &SecretKey::from_byte_array(secret).unwrap(),
            )
            .x_only_public_key()
            .0
        };
        assert_eq!(signed.verify(&key([7; 32])), Ok(()));
        assert_eq!(
            signed.verify(&key([8; 32])),
            Err(StatementError::BadSignature)
        );
        let mut changed = signed.clone();
        changed.statement.expiry += 1;
        assert_eq!(
            changed.verify(&key([7; 32])),
            Err(StatementError::BadSignature)
        );
        let mut truncated = signed;
        truncated.signature.truncate(126);
        assert_eq!(
            truncated.verify(&key([7; 32])),
            Err(StatementError::Malformed)
        );
    }

    #[test]
    fn unknown_kinds_and_fields_are_refused() {
        let json = serde_json::to_value(sign(statement(), [7; 32])).unwrap();
        assert_eq!(json["statement"]["outcomes"]["kind"], "ranking");
        assert_eq!(json["statement"]["terms"]["kind"], "observation");
        type Change = fn(&mut serde_json::Value);
        let refused: &[Change] = &[
            |j| j["statement"]["outcomes"]["kind"] = "tiers".into(),
            |j| j["statement"]["terms"]["kind"] = "tides".into(),
            |j| j["statement"]["outcomes"]["payout"] = 3.into(),
            |j| j["statement"]["terms"]["tide_height"] = 3.into(),
            |j| j["statement"]["premium"] = 3.into(),
        ];
        for change in refused {
            let mut candidate = json.clone();
            change(&mut candidate);
            assert!(serde_json::from_value::<SignedStatement>(candidate).is_err());
        }
    }

    #[test]
    fn terms_compare_bit_for_bit() {
        let Terms::Observation(terms) = statement().terms;
        let mut zero = terms.clone();
        zero.lines[0].upper = 0.0;
        let mut negative_zero = terms.clone();
        negative_zero.lines[0].upper = -0.0;
        assert!(terms.same_as(&terms.clone()));
        assert!(!zero.same_as(&negative_zero));
        let mut boundary = terms.clone();
        boundary.targets = vec!["KORDK".into(), "SAW".into()];
        assert!(!terms.same_as(&boundary));
    }
}
