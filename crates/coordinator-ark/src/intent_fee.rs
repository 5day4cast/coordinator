//! The intent fee an Arkade server charges, from the fee programs it publishes in `/v1/info`.
//!
//! The server takes what an intent leaves over its outputs, and refuses one that leaves less
//! than this fee (`INTENT_INSUFFICIENT_FEE`). Since 2026-10-21 Arkade's Mutinynet server
//! charges 0.2% of every VTXO input younger than a day.

use ark_core::server::Info;
use bitcoin::Amount;

/// What `info`'s fee programs charge for offchain inputs of `inputs`, each born at `birth`
/// (UNIX seconds), and for offchain outputs of `outputs`. Each one rounds up on its own, so
/// the sum is never below the server's.
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
    let mut total = Amount::ZERO;
    for amount in inputs {
        let fee = estimator.eval_offchain_input(ark_fees::OffchainInput {
            amount: amount.to_sat(),
            expiry: None,
            birth: Some(birth),
            input_type: ark_fees::VtxoType::Vtxo,
            weight: 0.0,
        })?;
        total += Amount::from_sat(fee.to_satoshis());
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
}
