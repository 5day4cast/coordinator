//! The intent fee an Arkade server charges, from the fee programs it publishes in `/v1/info`.
//!
//! The server takes what an intent leaves over its outputs, and refuses one that leaves less
//! than this fee (`INTENT_INSUFFICIENT_FEE`). Since 2026-10-21 Arkade's Mutinynet server has
//! charged 0.2% of every VTXO input younger than about a day. Its program first said so in terms
//! of the input's `birth`; by 2026-11-18 it said so in terms of its `expiry` instead:
//!
//! ```text
//! inputType == 'vtxo' && expiry - now() > 518656.0 ? amount * 0.002 : 0.0
//! ```
//!
//! The coordinator does not know when its escrows expire: `/v1/info` no longer publishes the VTXO
//! tree lifetime, and the coordinator records when an escrow was funded, not when it expires. An
//! input priced with no expiry looks long expired to such a program, which then charges nothing,
//! and the server refuses the intent. So each input is priced under several assumed remaining
//! lifetimes, and under none, and charged the most any of them costs. That covers programs that
//! charge young inputs and programs that charge inputs close to expiry alike. Overpaying is safe,
//! as the server keeps whatever the intent leaves over its outputs; underpaying makes it refuse
//! the intent.

use std::time::{SystemTime, UNIX_EPOCH};

use ark_core::server::Info;
use bitcoin::Amount;

const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;

/// The remaining lifetimes, in seconds from now, each input is priced under. `None` leaves its
/// expiry unset, which the fee programs read as the UNIX epoch, so a program written in terms of
/// `birth` alone is evaluated as it always was.
const REMAINING_LIFETIMES: [Option<i64>; 6] = [
    None,
    Some(0),
    Some(HOUR),
    Some(DAY),
    Some(7 * DAY),
    Some(30 * DAY),
];

/// What `info`'s fee programs charge for offchain inputs of `inputs`, each born at `birth`
/// (UNIX seconds), and for offchain outputs of `outputs`. Each input is charged the most its
/// program asks under any of [`REMAINING_LIFETIMES`], since its expiry is unknown. Each fee
/// rounds up on its own, so the sum is never below the server's.
pub fn intent_fee(
    info: &Info,
    inputs: impl IntoIterator<Item = Amount>,
    birth: i64,
    outputs: impl IntoIterator<Item = Amount>,
) -> Result<Amount, ark_fees::Error> {
    let Some(fees) = info.fees.as_ref() else {
        return Ok(Amount::ZERO);
    };
    let programs = &fees.intent_fee;
    let estimator = ark_fees::Estimator::new(ark_fees::Config {
        intent_offchain_input_program: programs.offchain_input.clone().unwrap_or_default(),
        intent_onchain_input_program: programs.onchain_input.clone().unwrap_or_default(),
        intent_offchain_output_program: programs.offchain_output.clone().unwrap_or_default(),
        intent_onchain_output_program: programs.onchain_output.clone().unwrap_or_default(),
    })?;
    // The programs' `now()` is the wall clock in UNIX seconds, so expiries count from it too.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let mut total = Amount::ZERO;
    for amount in inputs {
        let mut most = 0;
        for remaining in REMAINING_LIFETIMES {
            let fee = estimator.eval_offchain_input(ark_fees::OffchainInput {
                amount: amount.to_sat(),
                expiry: remaining.map(|remaining| now + remaining),
                birth: Some(birth),
                input_type: ark_fees::VtxoType::Vtxo,
                weight: 0.0,
            })?;
            most = most.max(fee.to_satoshis());
        }
        total += Amount::from_sat(most);
    }
    for amount in outputs {
        let fee = estimator.eval_offchain_output(ark_fees::Output {
            amount: amount.to_sat(),
            script: String::new(),
        })?;
        total += Amount::from_sat(fee.to_satoshis());
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{keypair, mock_info};
    use ark_core::server::{FeeInfo, IntentFeeInfo};

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn young_escrows_pay_the_servers_input_fee_rounded_up_per_input() {
        let mut info = mock_info(&keypair(1));
        assert_eq!(
            intent_fee(&info, [Amount::from_sat(5_382)], now(), []).unwrap(),
            Amount::ZERO,
            "a server without fee programs charges nothing"
        );
        info.fees = Some(FeeInfo {
            intent_fee: IntentFeeInfo {
                offchain_input: Some(
                    "inputType == 'vtxo' && now() - birth < 86400.0 ? amount * 0.002 : 0.0".into(),
                ),
                offchain_output: Some("0.0".into()),
                ..Default::default()
            },
            tx_fee_rate: "0".into(),
        });
        // The pool the server refused on 2026-10-21: 20 escrows of 5,382 sats, 216 required.
        let escrows = vec![Amount::from_sat(5_382); 20];
        let pool = Amount::from_sat(100_000);
        let fee = intent_fee(&info, escrows.clone(), now(), [pool]).unwrap();
        assert_eq!(fee, Amount::from_sat(220));
        assert!(fee >= Amount::from_sat(216));
        // A day later the same escrows are free.
        assert_eq!(
            intent_fee(&info, escrows, now() - 2 * 86_400, [pool]).unwrap(),
            Amount::ZERO
        );
    }

    /// The offchain input program Arkade's Mutinynet server published by 2026-11-18.
    const EXPIRY_PROGRAM: &str =
        "inputType == 'vtxo' && expiry - now() > 518656.0 ? amount * 0.002 : 0.0";

    /// A server charging `offchain_input` for offchain inputs and nothing else.
    fn charging(offchain_input: &str) -> Info {
        let mut info = mock_info(&keypair(1));
        info.fees = Some(FeeInfo {
            intent_fee: IntentFeeInfo {
                offchain_input: Some(offchain_input.into()),
                offchain_output: Some("0.0".into()),
                onchain_input: Some("0.0".into()),
                onchain_output: Some("0.0".into()),
            },
            tx_fee_rate: "0".into(),
        });
        info
    }

    #[test]
    fn escrows_of_unknown_expiry_pay_a_program_written_in_terms_of_expiry() {
        // Priced with no expiry, an escrow looks long expired to this program and pays nothing,
        // so the server refused the kickoff (INTENT_INSUFFICIENT_FEE).
        let estimator = ark_fees::Estimator::new(ark_fees::Config {
            intent_offchain_input_program: EXPIRY_PROGRAM.into(),
            ..Default::default()
        })
        .unwrap();
        let unexpiring = estimator
            .eval_offchain_input(ark_fees::OffchainInput {
                amount: 108_000,
                expiry: None,
                birth: Some(now()),
                input_type: ark_fees::VtxoType::Vtxo,
                weight: 0.0,
            })
            .unwrap();
        assert_eq!(unexpiring.to_satoshis(), 0);

        let info = charging(EXPIRY_PROGRAM);
        let pool = Amount::from_sat(100_000);
        // 0.2% of 108,000 sats.
        assert_eq!(
            intent_fee(&info, [Amount::from_sat(108_000)], now(), [pool]).unwrap(),
            Amount::from_sat(216)
        );
        // The program ignores birth, so the fee does too.
        assert_eq!(
            intent_fee(&info, [Amount::from_sat(108_000)], now() - 2 * DAY, [pool]).unwrap(),
            Amount::from_sat(216)
        );
        // Split over 20 escrows, each rounds 10.8 sats up to 11.
        assert_eq!(
            intent_fee(&info, vec![Amount::from_sat(5_400); 20], now(), [pool]).unwrap(),
            Amount::from_sat(220)
        );
    }

    #[test]
    fn each_input_pays_the_most_any_assumed_age_costs() {
        let escrow = [Amount::from_sat(108_000)];
        let pool = [Amount::from_sat(100_000)];
        // October's program, in terms of birth, still charges young escrows.
        let by_birth =
            charging("inputType == 'vtxo' && now() - birth < 86400.0 ? amount * 0.002 : 0.0");
        assert_eq!(
            intent_fee(&by_birth, escrow, now(), pool).unwrap(),
            Amount::from_sat(216)
        );
        // A program charging inputs within two hours of expiry, but not expired ones, charges
        // the escrow assumed to have an hour left.
        let near_expiry = charging("expiry > now() && expiry - now() < 7200.0 ? 100.0 : 0.0");
        assert_eq!(
            intent_fee(&near_expiry, escrow, now(), pool).unwrap(),
            Amount::from_sat(100)
        );
        // A free server charges nothing at any age.
        assert_eq!(
            intent_fee(&charging("0.0"), escrow, now(), pool).unwrap(),
            Amount::ZERO
        );
    }
}
