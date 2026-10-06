//! The dlctix contract options a competition's contract is built with, and the bounds a player's
//! signer accepts.
//!
//! dlctix 0.2.0 adds two options to `ContractParameters`, both off by default:
//!
//! - `outcome_bound_splits` commits each outcome in its winners' split scripts, so every outcome
//!   has its own outcome transaction and split sighashes;
//! - `anchor` appends a pay-to-anchor (P2A) output to every outcome, expiry and split
//!   transaction, so anyone can fee-bump them with a CPFP child.
//!
//! New competitions turn both on ([`ContractOptions::NEW`]). A competition keeps the options it
//! was created with: competitions stored before these options existed have none, and build their
//! contracts as dlctix 0.1.0 did ([`ContractOptions::LEGACY`]). A stored contract is never
//! rebuilt with other options, since that would change its transactions.
use crate::payout::dlctix::{
    anchor::{AnchorParams, ANCHOR_OUTPUT_WEIGHT, P2A_DUST_VALUE},
    bitcoin::Amount,
    ContractParameters, Outcome, PlayerIndex,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The anchor value new contracts carry: the P2A dust limit, the least a relayable anchor holds.
pub const NEW_ANCHOR_VALUE: Amount = P2A_DUST_VALUE;

/// The largest anchor a player's signer accepts. The anchor leaves the pot, so the coordinator
/// cannot use it to move more than a few hundred sats out of a contract.
pub const MAX_ANCHOR_VALUE: Amount = Amount::from_sat(1_000);

/// The options of a competition's contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractOptions {
    /// Bind each winner's split leaf to its outcome.
    #[serde(default)]
    pub outcome_bound_splits: bool,
    /// The pay-to-anchor output every outcome, expiry and split transaction carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<AnchorParams>,
}

impl ContractOptions {
    /// Contracts of competitions stored before the options existed: dlctix 0.1.0's transactions.
    pub const LEGACY: Self = Self {
        outcome_bound_splits: false,
        anchor: None,
    };

    /// What new competitions build their contracts with.
    pub const NEW: Self = Self {
        outcome_bound_splits: true,
        anchor: Some(AnchorParams {
            value: NEW_ANCHOR_VALUE,
        }),
    };

    /// The options `params` were built with.
    pub fn of(params: &ContractParameters) -> Self {
        Self {
            outcome_bound_splits: params.outcome_bound_splits,
            anchor: params.anchor,
        }
    }

    /// Set these options on `params`. Only for contracts being built: a stored contract keeps
    /// the options it was signed with.
    pub fn apply(self, params: &mut ContractParameters) {
        params.outcome_bound_splits = self.outcome_bound_splits;
        params.anchor = self.anchor;
    }

    /// Whether a competition paying `places` places may be created with these options: an
    /// anchor within the bounds players accept, and splits bound to their outcome when more than
    /// one place is paid, since its outcomes then share winners.
    pub fn check_for_places(&self, places: usize) -> Result<(), OptionsError> {
        if let Some(anchor) = self.anchor {
            if anchor.value < P2A_DUST_VALUE || anchor.value > MAX_ANCHOR_VALUE {
                return Err(OptionsError::AnchorValue(anchor.value));
            }
        }
        if places > 1 && !self.outcome_bound_splits {
            return Err(OptionsError::UnboundSplits);
        }
        Ok(())
    }

    /// What the anchor adds to resolving a contract through its outcome transaction: the
    /// anchor output's virtual bytes, and its value, which leaves the pot. Zero without one.
    pub fn outcome_anchor_cost(&self) -> AnchorCost {
        match self.anchor {
            Some(anchor) => AnchorCost {
                vbytes: ANCHOR_OUTPUT_WEIGHT.to_vbytes_ceil(),
                sats: anchor.value.to_sat(),
            },
            None => AnchorCost::default(),
        }
    }
}

/// The on-chain cost an anchor adds to one transaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorCost {
    /// The anchor output's size, paid at the transaction's fee rate.
    pub vbytes: u64,
    /// The anchor's value.
    pub sats: u64,
}

/// Why a player's signer refuses a contract's options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionsError {
    /// The anchor is below the P2A dust limit or above [`MAX_ANCHOR_VALUE`].
    AnchorValue(Amount),
    /// Two attestation outcomes pay the same winners, so their split transactions must be bound
    /// to their outcome.
    UnboundSplits,
}

impl std::fmt::Display for OptionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OptionsError::AnchorValue(value) => write!(
                f,
                "the contract's {} sat anchor is outside {}..={} sats",
                value.to_sat(),
                P2A_DUST_VALUE.to_sat(),
                MAX_ANCHOR_VALUE.to_sat()
            ),
            OptionsError::UnboundSplits => write!(
                f,
                "outcomes share winners, so the contract must bind splits to their outcome"
            ),
        }
    }
}

impl std::error::Error for OptionsError {}

/// Check the options of a contract a player is asked to sign: an anchor, if any, within
/// `P2A_DUST_VALUE..=MAX_ANCHOR_VALUE`, and splits bound to their outcome whenever two
/// attestation outcomes pay the same set of winners, as every contract paying more than one
/// place does. Contracts without these options, as every competition stored before them built,
/// pass as long as no two outcomes share winners.
pub fn check_contract_options(params: &ContractParameters) -> Result<(), OptionsError> {
    let options = ContractOptions::of(params);
    options.check_for_places(1)?;
    if !options.outcome_bound_splits && outcomes_share_winners(params) {
        return Err(OptionsError::UnboundSplits);
    }
    Ok(())
}

/// Whether two attestation outcomes of `params` pay the same set of winners.
pub fn outcomes_share_winners(params: &ContractParameters) -> bool {
    let mut seen = BTreeSet::<Vec<PlayerIndex>>::new();
    params
        .outcome_payouts
        .iter()
        .filter(|(outcome, _)| matches!(outcome, Outcome::Attestation(_)))
        .any(|(_, weights)| !seen.insert(weights.keys().copied().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payout::dlctix::{
        bitcoin::FeeRate, secp::Scalar, EventLockingConditions, MarketMaker, Player,
    };
    use std::collections::BTreeMap;

    fn point(byte: u8) -> crate::payout::dlctix::secp::Point {
        Scalar::from_slice(&[byte; 32]).unwrap().base_point_mul()
    }

    /// Two players; one place pays either, or two places pay both in either order.
    fn params(places: usize) -> ContractParameters {
        let payouts = if places == 1 {
            BTreeMap::from([
                (Outcome::Attestation(0), BTreeMap::from([(0, 1)])),
                (Outcome::Attestation(1), BTreeMap::from([(1, 1)])),
            ])
        } else {
            BTreeMap::from([
                (Outcome::Attestation(0), BTreeMap::from([(0, 70), (1, 30)])),
                (Outcome::Attestation(1), BTreeMap::from([(0, 30), (1, 70)])),
            ])
        };
        ContractParameters {
            market_maker: MarketMaker { pubkey: point(1) },
            players: (0..2u8)
                .map(|i| Player {
                    pubkey: point(10 + i),
                    ticket_hash: [20 + i; 32],
                    payout_hash: [30 + i; 32],
                })
                .collect(),
            event: EventLockingConditions {
                locking_points: vec![point(40).into(), point(41).into()],
                expiry: None,
            },
            outcome_payouts: payouts,
            fee_rate: FeeRate::from_sat_per_vb_u32(1),
            funding_value: Amount::from_sat(100_000),
            relative_locktime_block_delta: 72,
            anchor: None,
            outcome_bound_splits: false,
        }
    }

    #[test]
    fn new_contracts_bind_splits_and_carry_a_dust_anchor() {
        let mut p = params(2);
        ContractOptions::NEW.apply(&mut p);
        assert!(p.outcome_bound_splits);
        assert_eq!(p.anchor.map(|a| a.value), Some(P2A_DUST_VALUE));
        assert_eq!(ContractOptions::of(&p), ContractOptions::NEW);
        p.validate().unwrap();
        check_contract_options(&p).unwrap();
    }

    #[test]
    fn legacy_one_place_contracts_still_pass() {
        let p = params(1);
        assert_eq!(ContractOptions::of(&p), ContractOptions::LEGACY);
        check_contract_options(&p).unwrap();
    }

    #[test]
    fn shared_winners_need_bound_splits() {
        let p = params(2);
        assert!(outcomes_share_winners(&p));
        assert_eq!(check_contract_options(&p), Err(OptionsError::UnboundSplits));
    }

    #[test]
    fn competitions_paying_two_places_need_bound_splits() {
        ContractOptions::NEW.check_for_places(2).unwrap();
        ContractOptions::LEGACY.check_for_places(1).unwrap();
        assert_eq!(
            ContractOptions::LEGACY.check_for_places(2),
            Err(OptionsError::UnboundSplits)
        );
    }

    #[test]
    fn anchors_outside_the_bounds_are_refused() {
        for sats in [239, 1_001] {
            let mut p = params(1);
            p.anchor = Some(AnchorParams {
                value: Amount::from_sat(sats),
            });
            assert!(matches!(
                check_contract_options(&p),
                Err(OptionsError::AnchorValue(_))
            ));
        }
    }

    #[test]
    fn options_round_trip_and_legacy_has_no_anchor() {
        assert_eq!(
            serde_json::to_string(&ContractOptions::LEGACY).unwrap(),
            r#"{"outcome_bound_splits":false}"#
        );
        let new: ContractOptions =
            serde_json::from_str(&serde_json::to_string(&ContractOptions::NEW).unwrap()).unwrap();
        assert_eq!(new, ContractOptions::NEW);
    }

    #[test]
    fn the_anchor_costs_its_output_and_its_value() {
        assert_eq!(
            ContractOptions::NEW.outcome_anchor_cost(),
            AnchorCost {
                vbytes: 13,
                sats: 240
            }
        );
        assert_eq!(
            ContractOptions::LEGACY.outcome_anchor_cost(),
            AnchorCost::default()
        );
    }
}
