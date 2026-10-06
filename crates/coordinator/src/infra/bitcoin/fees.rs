//! Fee estimates from electrs' long-target baseline and current mempool.
//!
//! Start at the 144-block historical estimate, then raise it for current congestion.
//! Reserve 20% of each block for arriving transactions and the transaction being priced.
//! This is a local pricing policy, not Core's next-block estimate or a confirmation guarantee.
//! Contract callers add their existing margin. All three local RPC responses are required.

use super::{ElectrumApi, ElectrumClient, ECONOMY_FEE_TARGET, LND_FEE_RATE_FLOOR};
use anyhow::{anyhow, Context};
use electrum_client::Param;
use std::collections::HashMap;

/// One block holds at most one million virtual bytes. Leave room for new arrivals.
const VBYTES_PER_BLOCK: u64 = 800_000;

pub(super) fn estimates(
    client: &ElectrumClient,
    targets: &[u16],
) -> Result<HashMap<u16, f64>, anyhow::Error> {
    let histogram = client
        .raw_call("mempool.get_fee_histogram", std::iter::empty::<Param>())
        .context("cannot read electrs mempool fee histogram")?;
    let histogram: Vec<(f64, u64)> =
        serde_json::from_value(histogram).context("invalid electrs mempool fee histogram")?;
    let relay_btc_per_kvb = client
        .relay_fee()
        .context("cannot read electrs relay fee")?;
    let floor = relay_floor(relay_btc_per_kvb)?;
    validate_histogram(&histogram)?;
    let historical_btc_per_kvb = client
        .estimate_fee(usize::from(ECONOMY_FEE_TARGET))
        .context("cannot read electrs long-target fee estimate")?;
    let baseline = historical_baseline(historical_btc_per_kvb, floor)?;
    let rates = targets
        .iter()
        .map(|&target| Ok((target, rate_for_target(&histogram, baseline, target)?)))
        .collect::<Result<HashMap<_, _>, anyhow::Error>>()?;
    log::debug!(
        "electrs fee estimates: {ECONOMY_FEE_TARGET}-block baseline {baseline} sat/vB, \
         {} mempool buckets, floor {floor} sat/vB, {rates:?}",
        histogram.len()
    );
    Ok(rates)
}

fn relay_floor(btc_per_kvb: f64) -> Result<f64, anyhow::Error> {
    let sat_per_vb = btc_per_kvb_to_sat_per_vb(btc_per_kvb, "relay fee")?;
    // LND still signs and publishes wallet transactions, so retain its minimum too.
    // Round the relay floor up at the wallet's precision, never below the node's minimum.
    Ok(((sat_per_vb * 250.0).ceil() / 250.0)
        .max(LND_FEE_RATE_FLOOR.to_sat_per_kwu() as f64 / 250.0))
}

fn historical_baseline(btc_per_kvb: f64, floor: f64) -> Result<f64, anyhow::Error> {
    let estimate = btc_per_kvb_to_sat_per_vb(btc_per_kvb, "long-target estimate")?;
    if estimate == 0.0 {
        return Err(anyhow!("electrs returned a zero long-target fee estimate"));
    }
    Ok(estimate.max(floor))
}

fn btc_per_kvb_to_sat_per_vb(btc_per_kvb: f64, source: &str) -> Result<f64, anyhow::Error> {
    // Electrum's historical and relay fees are BTC/kvB. Histogram fees are sat/vB.
    let sat_per_vb = btc_per_kvb * 100_000.0;
    if !sat_per_vb.is_finite() || sat_per_vb < 0.0 || sat_per_vb >= u64::MAX as f64 / 250.0 {
        return Err(anyhow!("invalid electrs {source} {btc_per_kvb} BTC/kvB"));
    }
    Ok(sat_per_vb)
}

fn validate_histogram(histogram: &[(f64, u64)]) -> Result<(), anyhow::Error> {
    let mut previous = f64::INFINITY;
    let mut total = 0u64;
    for &(fee, vbytes) in histogram {
        if !fee.is_finite() || fee < 0.0 || fee >= u64::MAX as f64 / 250.0 || fee > previous {
            return Err(anyhow!("invalid or unordered electrs fee histogram"));
        }
        previous = fee;
        total = total
            .checked_add(vbytes)
            .ok_or_else(|| anyhow!("electrs fee histogram size overflows"))?;
    }
    Ok(())
}

fn rate_for_target(
    histogram: &[(f64, u64)],
    baseline: f64,
    target: u16,
) -> Result<f64, anyhow::Error> {
    if target == 0 {
        return Err(anyhow!(
            "fee confirmation target must be at least one block"
        ));
    }
    let capacity = u64::from(target) * VBYTES_PER_BLOCK;
    let mut ahead = 0u64;
    for &(fee, vbytes) in histogram {
        ahead += vbytes; // The complete histogram was checked for overflow first.
        if ahead >= capacity {
            // Bid one sat/kWU above this bucket to avoid tying a full target window.
            // Each bucket's vbytes is its size, not the cumulative size of earlier buckets.
            return Ok((((fee * 250.0).floor() + 1.0) / 250.0).max(baseline));
        }
    }
    // The entire current mempool fits within the target, including an empty mempool.
    Ok(baseline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn the_observed_mutinynet_mempool_allows_the_three_player_pool() {
        let (client, server) = electrum_fixture(vec![
            (
                "mempool.get_fee_histogram",
                serde_json::json!({"result": [[3, 1636], [1, 0], [0, 858]]}),
            ),
            (
                "blockchain.relayfee",
                serde_json::json!({"result": 0.000001}),
            ),
            (
                "blockchain.estimatefee",
                serde_json::json!({"result": 0.00001029}),
            ),
        ]);
        let rates = estimates(&client, &[1, 2, 144]).unwrap();
        server.join().unwrap();
        let estimate = rates[&1];
        assert!((estimate - 1.029).abs() < 1e-12);
        assert_eq!(rates[&2], estimate);
        assert_eq!(rates[&144], estimate);
        let contract_rate = super::super::fee_rate_from_estimate(estimate).unwrap();
        assert_eq!(contract_rate.to_sat_per_kwu(), 320);
        assert_eq!(contract_rate.to_sat_per_vb_ceil(), 2);

        let check = crate::domain::KickoffCheck::evaluate(
            &crate::config::NetworkFeeSettings::default(),
            &crate::config::KickoffCheckSettings::default(),
            crate::domain::KickoffPool {
                players: 3,
                paid_places: 1,
                pot_sats: 15_000,
                paid_sats: 1794,
                template_min_players: 2,
            },
            contract_rate,
            bitcoin::FeeRate::from_sat_per_vb_u32(100),
            time::OffsetDateTime::now_utc(),
        )
        .unwrap();
        assert!(check.passed);
    }

    #[test]
    fn congested_targets_use_bucket_sizes_and_keep_block_headroom() {
        let histogram = [(10.0, 400_000), (5.0, 500_000), (2.0, 800_000)];
        validate_histogram(&histogram).unwrap();
        let floor = relay_floor(0.00001).unwrap();
        let baseline = historical_baseline(0.00001029, floor).unwrap();
        assert_eq!(rate_for_target(&histogram, baseline, 1).unwrap(), 5.004);
        assert_eq!(rate_for_target(&histogram, baseline, 2).unwrap(), 2.004);
        assert_eq!(rate_for_target(&histogram, baseline, 3).unwrap(), baseline);
        assert_eq!(
            rate_for_target(&[(9.5, VBYTES_PER_BLOCK)], baseline, 1).unwrap(),
            9.504
        );
    }

    #[test]
    fn the_historical_baseline_respects_wallet_and_relay_minimums() {
        let wallet_floor = relay_floor(0.000001).unwrap();
        assert_eq!(historical_baseline(0.000005, wallet_floor).unwrap(), 1.012);
        let relay_floor = relay_floor(0.00005).unwrap();
        assert_eq!(historical_baseline(0.00001029, relay_floor).unwrap(), 5.0);
        let baseline = historical_baseline(0.00003, wallet_floor).unwrap();
        assert_eq!(baseline, 3.0);
        assert_eq!(rate_for_target(&[], baseline, 1).unwrap(), 3.0);
    }

    #[test]
    fn empty_mempools_and_zero_size_buckets_respect_the_relay_floor() {
        let floor = relay_floor(0.00005).unwrap();
        assert_eq!(floor, 5.0);
        assert_eq!(rate_for_target(&[], floor, 1).unwrap(), floor);
        assert_eq!(rate_for_target(&[(100.0, 0)], floor, 1).unwrap(), floor);
        assert_eq!(relay_floor(1.013 / 100_000.0).unwrap(), 1.016);
        assert!(rate_for_target(&[], floor, 0).is_err());
    }

    #[test]
    fn invalid_local_fee_data_is_rejected() {
        for fee in [f64::NAN, f64::INFINITY, -1.0, f64::MAX] {
            assert!(relay_floor(fee).is_err());
            assert!(historical_baseline(fee, 1.012).is_err());
            assert!(validate_histogram(&[(fee, 1)]).is_err());
        }
        assert!(historical_baseline(0.0, 1.012).is_err());
        assert!(validate_histogram(&[(1.0, 10), (2.0, 10)]).is_err());
        assert!(validate_histogram(&[(2.0, u64::MAX), (1.0, 1)]).is_err());
    }

    fn electrum_fixture(
        responses: Vec<(&'static str, serde_json::Value)>,
    ) -> (ElectrumClient, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut input = BufReader::new(stream.try_clone().unwrap());
            for (method, mut response) in responses {
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], method);
                if method == "blockchain.estimatefee" {
                    assert_eq!(request["params"], serde_json::json!([144]));
                }
                response["id"] = request["id"].clone();
                writeln!(stream, "{response}").unwrap();
            }
        });
        let client = ElectrumClient::from_config(
            &format!("tcp://{address}"),
            super::super::ConfigBuilder::new()
                .timeout(Some(5))
                .retry(0)
                .build(),
        )
        .unwrap();
        (client, server)
    }

    #[test]
    fn estimates_read_the_electrum_histogram_and_convert_the_relay_fee_units() {
        let (client, server) = electrum_fixture(vec![
            (
                "mempool.get_fee_histogram",
                serde_json::json!({"result": [[3, 1636], [1, 0], [0, 858]]}),
            ),
            (
                "blockchain.relayfee",
                serde_json::json!({"result": 0.00005}),
            ),
            (
                "blockchain.estimatefee",
                serde_json::json!({"result": 0.00001029}),
            ),
        ]);
        let rates = estimates(&client, &[1, 6, 144]).unwrap();
        assert_eq!(rates, HashMap::from([(1, 5.0), (6, 5.0), (144, 5.0)]));
        server.join().unwrap();
    }

    #[test]
    fn missing_or_invalid_history_does_not_silently_use_the_minimum_fee() {
        for response in [
            serde_json::json!({"error": {"code": -1, "message": "no estimate"}}),
            serde_json::json!({"result": -1}),
            serde_json::json!({"result": 0}),
            serde_json::json!({"result": null}),
            serde_json::json!({"result": "invalid"}),
        ] {
            let (client, server) = electrum_fixture(vec![
                (
                    "mempool.get_fee_histogram",
                    serde_json::json!({"result": []}),
                ),
                (
                    "blockchain.relayfee",
                    serde_json::json!({"result": 0.000001}),
                ),
                ("blockchain.estimatefee", response),
            ]);
            assert!(estimates(&client, &[1]).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn unavailable_or_malformed_mempool_data_does_not_produce_a_fee() {
        for response in [
            serde_json::json!({"error": {"code": -32601, "message": "method not found"}}),
            serde_json::json!({"result": [[3, -1]]}),
            serde_json::json!({"result": [[3, 0.5]]}),
        ] {
            let (client, server) = electrum_fixture(vec![("mempool.get_fee_histogram", response)]);
            assert!(estimates(&client, &[1]).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn a_missing_relay_floor_does_not_turn_a_known_histogram_into_a_fee() {
        let (client, server) = electrum_fixture(vec![
            (
                "mempool.get_fee_histogram",
                serde_json::json!({"result": []}),
            ),
            (
                "blockchain.relayfee",
                serde_json::json!({"error": {"code": -1, "message": "node unavailable"}}),
            ),
        ]);
        assert!(estimates(&client, &[1]).is_err());
        server.join().unwrap();
    }
}
