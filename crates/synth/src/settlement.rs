//! How a competition settled: which outcome decided it, and what each player was owed.
//!
//! A contract lists its outcomes and, for each, the weights its players are paid in. The oracle
//! reveals the discrete log of one outcome's locking point; which point that is decides the
//! outcome. The coordinator pays each winner their weight's share of the funding value over
//! Lightning, so that is what a player is owed here.

use std::collections::BTreeMap;

use dlctix::secp::{MaybePoint, MaybeScalar};
use dlctix::{ContractParameters, EventLockingConditions, Outcome};

use crate::client::competitions::CompetitionResponse;

/// A contract's outcome, once one has decided it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Decided {
    /// The oracle attested to the outcome at this index.
    Attested(usize),
    /// The oracle never attested, and the contract's expiry terms paid out.
    Expired,
}

/// What one player in the contract was owed under the deciding outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Share {
    /// The player's entry key, as the contract lists it.
    pub pubkey: String,
    /// Their share of the funding value, in percent; 0 for a player the outcome does not pay.
    pub weight: u64,
    pub owed_sats: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settlement {
    /// What the contract holds, and what the payouts are shares of.
    pub pot_sats: u64,
    /// None until the oracle attests or the contract expires.
    pub decided: Option<Decided>,
    /// One per player, in contract order.
    pub shares: Vec<Share>,
}

impl Settlement {
    /// How the competition settled, read from its contract, announcement, and attestation. None
    /// before it has a contract.
    pub fn of(competition: &CompetitionResponse) -> Option<Settlement> {
        let params: ContractParameters =
            serde_json::from_value(competition.contract_parameters.clone()?).ok()?;
        let decided = if competition.expiry_broadcasted_at.is_some() {
            Some(Decided::Expired)
        } else {
            match (&competition.attestation, &competition.event_announcement) {
                (Some(attestation), Some(announcement)) => {
                    let attestation: MaybeScalar =
                        serde_json::from_value(serde_json::Value::String(attestation.clone()))
                            .ok()?;
                    let announcement: EventLockingConditions =
                        serde_json::from_value(announcement.clone()).ok()?;
                    attested_outcome(attestation, &announcement.locking_points)
                        .map(Decided::Attested)
                }
                _ => None,
            }
        };
        let players: Vec<String> = params
            .players
            .iter()
            .map(|player| player.pubkey.to_string())
            .collect();
        let pot_sats = params.funding_value.to_sat();
        let weights = decided.as_ref().and_then(|decided| {
            let outcome = match decided {
                Decided::Attested(index) => Outcome::Attestation(*index),
                Decided::Expired => Outcome::Expiry,
            };
            params.outcome_payouts.get(&outcome)
        });
        Some(Settlement {
            pot_sats,
            decided,
            shares: shares(&players, weights, pot_sats),
        })
    }

    /// The players the deciding outcome pays.
    pub fn winners(&self) -> impl Iterator<Item = &Share> {
        self.shares.iter().filter(|share| share.owed_sats > 0)
    }
}

/// The outcome whose locking point the attestation opens.
fn attested_outcome(attestation: MaybeScalar, locking_points: &[MaybePoint]) -> Option<usize> {
    let point = attestation.base_point_mul();
    locking_points.iter().position(|locking| *locking == point)
}

/// Each player's share of the pot under `weights`, as the coordinator pays it: the weight is a
/// percentage of the funding value.
fn shares(players: &[String], weights: Option<&BTreeMap<usize, u64>>, pot_sats: u64) -> Vec<Share> {
    players
        .iter()
        .enumerate()
        .map(|(index, pubkey)| {
            let weight = weights
                .and_then(|weights| weights.get(&index))
                .copied()
                .unwrap_or(0);
            Share {
                pubkey: pubkey.clone(),
                weight,
                owed_sats: (u128::from(pot_sats) * u128::from(weight) / 100) as u64,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dlctix::secp::Scalar;

    fn scalar(byte: u8) -> Scalar {
        let mut bytes = [0u8; 32];
        bytes[31] = byte;
        Scalar::from_slice(&bytes).unwrap()
    }

    #[test]
    fn the_attestation_picks_the_outcome_whose_locking_point_it_opens() {
        let locking_points: Vec<MaybePoint> = (1..=4)
            .map(|byte| MaybeScalar::from(scalar(byte)).base_point_mul())
            .collect();
        assert_eq!(
            attested_outcome(MaybeScalar::from(scalar(4)), &locking_points),
            Some(3)
        );
        assert_eq!(
            attested_outcome(MaybeScalar::from(scalar(9)), &locking_points),
            None,
            "an attestation to another event decides nothing here"
        );
    }

    #[test]
    fn a_single_winner_is_owed_the_whole_pot_and_the_rest_nothing() {
        let players = ["a".to_string(), "b".to_string(), "c".to_string()];
        let weights = BTreeMap::from([(1, 100)]);
        let owed: Vec<u64> = shares(&players, Some(&weights), 3000)
            .iter()
            .map(|share| share.owed_sats)
            .collect();
        assert_eq!(owed, [0, 3000, 0]);
    }

    /// The lab's tie outcome: everyone paid, unevenly, which is what "3 of 3 paid out" meant.
    #[test]
    fn a_split_pays_every_player_their_weight() {
        let players = ["a".to_string(), "b".to_string(), "c".to_string()];
        let weights = BTreeMap::from([(0, 34), (1, 33), (2, 33)]);
        let owed: Vec<u64> = shares(&players, Some(&weights), 3000)
            .iter()
            .map(|share| share.owed_sats)
            .collect();
        assert_eq!(owed, [1020, 990, 990]);
    }

    /// A completed competition from the lab, as the coordinator serves it. It paid all three
    /// players because the oracle attested the tie, not because each of them won.
    #[test]
    fn a_lab_competition_settled_on_its_tie() {
        let competition: CompetitionResponse =
            serde_json::from_str(include_str!("fixtures/lab-competition.json")).unwrap();
        let settlement = Settlement::of(&competition).expect("it has a contract");
        assert_eq!(settlement.pot_sats, 3000);
        assert_eq!(settlement.decided, Some(Decided::Attested(3)));
        assert_eq!(
            settlement
                .shares
                .iter()
                .map(|share| share.owed_sats)
                .collect::<Vec<_>>(),
            [1020, 990, 990]
        );
    }

    #[test]
    fn nobody_is_owed_anything_before_an_outcome() {
        let players = ["a".to_string()];
        assert_eq!(shares(&players, None, 3000)[0].owed_sats, 0);
    }
}
