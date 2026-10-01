use super::*;

use bitcoin::consensus::encode::serialize_hex;
use bitcoin::{Amount, ScriptBuf, TxIn, TxOut};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type Reply = Result<Value, Value>;

struct MockElectrs {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl MockElectrs {
    async fn start(
        network: Network,
        reply: impl Fn(&Value) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("tcp://{}", listener.local_addr().unwrap());
        let reply = Arc::new(reply);
        let task = tokio::spawn(async move {
            let mut peers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let (stream, _) = connection.unwrap();
                        let reply = Arc::clone(&reply);
                        peers.spawn(async move {
                            let (read, mut write) = stream.into_split();
                            let mut lines = BufReader::new(read).lines();
                            while let Some(line) = lines.next_line().await.unwrap() {
                                let request: Value = serde_json::from_str(&line).unwrap();
                                let result = if request["method"] == "blockchain.block.header"
                                    && request["params"][0] == 0
                                {
                                    Ok(json!(serialize_hex(&genesis_block(network).header)))
                                } else {
                                    reply(&request)
                                };
                                let response = match result {
                                    Ok(result) => json!({"id": request["id"], "result": result}),
                                    Err(error) => json!({"id": request["id"], "error": error}),
                                };
                                let line = format!("{response}\n");
                                if write.write_all(line.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Some(result) = peers.join_next() => result.unwrap(),
                }
            }
        });
        Self { url, task }
    }

    async fn chain(&self) -> Electrum {
        Electrum::connect(&self.url, Network::Regtest)
            .await
            .unwrap()
    }
}

impl Drop for MockElectrs {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn unsupported() -> Reply {
    Err(json!({"code": -32601, "message": "method not found"}))
}

fn address() -> Address {
    Address::p2wsh(&ScriptBuf::new(), Network::Regtest)
}

fn transaction(previous_output: OutPoint) -> Transaction {
    Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output,
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(5_000),
            script_pubkey: address().script_pubkey(),
        }],
    }
}

fn header() -> Header {
    let genesis = genesis_block(Network::Regtest);
    Header {
        prev_blockhash: genesis.block_hash(),
        time: genesis.header.time + 600,
        ..genesis.header
    }
}

fn tip() -> Value {
    json!({"height": 1, "hex": serialize_hex(&header())})
}

#[tokio::test]
async fn connection_rejects_an_electrs_server_on_another_network() {
    let server = MockElectrs::start(Network::Bitcoin, |_| unsupported()).await;
    let error = Electrum::connect(&server.url, Network::Regtest)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("does not match"));
    assert!(
        Electrum::connect("https://example.invalid", Network::Regtest)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn raw_transaction_hash_must_match_the_requested_id() {
    let transaction = transaction(OutPoint::null());
    let wanted = transaction.compute_txid();
    let mut wrong = transaction.clone();
    wrong.output[0].value = Amount::from_sat(4_000);
    let server = MockElectrs::start(Network::Regtest, move |request| {
        if request["method"] == "blockchain.transaction.get" {
            Ok(json!(serialize_hex(&wrong)))
        } else {
            unsupported()
        }
    })
    .await;
    assert!(server
        .chain()
        .await
        .find_tx(&wanted)
        .await
        .unwrap_err()
        .to_string()
        .contains("hash mismatch"));
}

#[tokio::test]
async fn unknown_rpc_errors_are_not_treated_as_missing_transactions() {
    let txid = transaction(OutPoint::null()).compute_txid();
    for (message, missing) in [
        (
            "No such mempool or blockchain transaction. Use gettransaction for wallet transactions.",
            true,
        ),
        (
            "No such mempool transaction. Blockchain transactions are still in the process of being indexed. Use gettransaction for wallet transactions.",
            false,
        ),
        ("database read failed", false),
    ] {
        let server = MockElectrs::start(Network::Regtest, move |request| {
            if request["method"] == "blockchain.transaction.get" {
                Err(json!({"code": 2, "message": message}))
            } else {
                unsupported()
            }
        })
        .await;
        let result = server.chain().await.find_tx(&txid).await;
        if missing {
            assert!(result.unwrap().is_none());
        } else {
            assert!(result.is_err());
        }
    }
    assert!(!transaction_not_found(&electrum_client::Error::Protocol(
        json!({
            "code": 1,
            "message": "No such mempool or blockchain transaction. Use gettransaction for wallet transactions."
        })
    )));
}

#[tokio::test]
async fn boarding_outputs_check_raw_amount_script_and_confirmation_height() {
    for invalid in [
        None,
        Some("amount"),
        Some("script"),
        Some("height"),
        Some("hash"),
        Some("header"),
    ] {
        let mut funding = transaction(OutPoint::null());
        if invalid == Some("script") {
            funding.output[0].script_pubkey = ScriptBuf::new();
        }
        let txid = funding.compute_txid();
        let server = MockElectrs::start(Network::Regtest, move |request| {
            match request["method"].as_str().unwrap() {
                "blockchain.headers.subscribe" => Ok(tip()),
                "blockchain.block.header" => {
                    let mut response = header();
                    if invalid == Some("header") {
                        response.time += 1;
                    }
                    Ok(json!(serialize_hex(&response)))
                }
                "blockchain.scripthash.listunspent" => Ok(json!([{
                    "height": if invalid == Some("height") { 2 } else { 1 },
                    "tx_hash": txid,
                    "tx_pos": 0,
                    "value": if invalid == Some("amount") { 6_000 } else { 5_000 },
                }])),
                "blockchain.transaction.get" => {
                    let mut response = funding.clone();
                    if invalid == Some("hash") {
                        response.output[0].value = Amount::from_sat(4_000);
                    }
                    Ok(json!(serialize_hex(&response)))
                }
                _ => unsupported(),
            }
        })
        .await;
        let result = server.chain().await.find_outpoints(&address()).await;
        if invalid.is_some() {
            assert!(result.is_err(), "accepted invalid {invalid:?}");
        } else {
            let outputs = result.unwrap();
            assert_eq!(outputs.len(), 1);
            assert_eq!(outputs[0].outpoint, OutPoint::new(txid, 0));
            assert_eq!(outputs[0].amount, Amount::from_sat(5_000));
            assert_eq!(outputs[0].confirmations, 1);
            assert_eq!(
                outputs[0].confirmation_blocktime,
                Some(u64::from(header().time))
            );
            assert!(!outputs[0].is_spent);
        }
    }
}

#[tokio::test]
async fn output_status_requires_a_transaction_that_actually_spends_the_outpoint() {
    for actually_spends in [false, true] {
        let funding = transaction(OutPoint::null());
        let outpoint = OutPoint::new(funding.compute_txid(), 0);
        let candidate = transaction(if actually_spends {
            outpoint
        } else {
            OutPoint::null()
        });
        let candidate_id = candidate.compute_txid();
        let server = MockElectrs::start(Network::Regtest, move |request| {
            match request["method"].as_str().unwrap() {
                "blockchain.headers.subscribe" => Ok(tip()),
                "blockchain.scripthash.get_history" => Ok(json!([{
                    "tx_hash": candidate_id,
                    "height": 0,
                }])),
                "blockchain.scripthash.listunspent" => Ok(json!([])),
                "blockchain.transaction.get" => {
                    let transaction = if request["params"][0] == outpoint.txid.to_string() {
                        &funding
                    } else {
                        &candidate
                    };
                    Ok(json!(serialize_hex(transaction)))
                }
                _ => unsupported(),
            }
        })
        .await;
        let result = server
            .chain()
            .await
            .get_output_status(&outpoint.txid, 0)
            .await;
        if actually_spends {
            assert_eq!(result.unwrap().spend_txid, Some(candidate_id));
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn fee_estimates_use_sats_per_vbyte_and_reject_an_unavailable_estimate() {
    for rate in [0.00002, -1.0] {
        let server = MockElectrs::start(Network::Regtest, move |request| {
            if request["method"] == "blockchain.estimatefee" {
                Ok(json!(rate))
            } else {
                unsupported()
            }
        })
        .await;
        let result = server.chain().await.get_fee_rate().await;
        if rate > 0.0 {
            assert!((result.unwrap() - 2.0).abs() < f64::EPSILON * 4.0);
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn a_cancelled_caller_keeps_its_blocking_request_permit() {
    let server = MockElectrs::start(Network::Regtest, |_| unsupported()).await;
    let chain = Arc::new(server.chain().await);
    let held = Arc::clone(&chain.permits).try_acquire_owned().unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let worker_chain = Arc::clone(&chain);
    let caller = tokio::spawn(async move {
        worker_chain
            .with_rpc(move |_| {
                entered_tx.send(()).unwrap();
                // Dropping the sender also releases this fixture if an assertion fails.
                let _ = release_rx.recv();
                Ok(())
            })
            .await
    });
    entered_rx.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let result = chain.with_rpc(|_| Ok(())).await;
    assert!(result.unwrap_err().to_string().contains("busy"));
    drop(release_tx);
    drop(held);
}

#[tokio::test]
async fn transaction_status_uses_the_confirmation_header_and_preserves_mempool_status() {
    for height in [0, 1] {
        let funding = transaction(OutPoint::null());
        let txid = funding.compute_txid();
        let server = MockElectrs::start(Network::Regtest, move |request| {
            match request["method"].as_str().unwrap() {
                "blockchain.headers.subscribe" => Ok(tip()),
                "blockchain.block.header" => Ok(json!(serialize_hex(&header()))),
                "blockchain.scripthash.get_history" => Ok(json!([{
                    "tx_hash": txid,
                    "height": height,
                }])),
                "blockchain.transaction.get" => Ok(json!(serialize_hex(&funding))),
                _ => unsupported(),
            }
        })
        .await;
        let status = server.chain().await.get_tx_status(&txid).await.unwrap();
        assert_eq!(
            status.confirmed_at,
            (height > 0).then_some(i64::from(header().time))
        );
    }
}

#[tokio::test]
async fn many_separate_deposits_remain_available_for_boarding() {
    // More than the per-operation round-trip budget: separate synchronous transaction
    // lookups would reject this wallet forever, even though all deposits are valid.
    let mut transactions = HashMap::new();
    let mut unspent = Vec::new();
    let first_header = header();
    let second_header = Header {
        prev_blockhash: first_header.block_hash(),
        time: first_header.time + 600,
        ..first_header
    };
    for index in 0..1_100 {
        let mut transaction = transaction(OutPoint::null());
        transaction.lock_time = bitcoin::absolute::LockTime::from_consensus(index);
        let txid = transaction.compute_txid();
        transactions.insert(txid.to_string(), transaction);
        unspent.push(json!({
            "tx_hash": txid,
            "tx_pos": 0,
            "value": 5_000,
            "height": 1 + index % 2,
        }));
    }
    let server = MockElectrs::start(Network::Regtest, move |request| {
        match request["method"].as_str().unwrap() {
            "blockchain.headers.subscribe" => Ok(json!({
                "height": 2,
                "hex": serialize_hex(&second_header),
            })),
            "blockchain.block.header" => {
                let header = match request["params"][0].as_u64().unwrap() {
                    1 => first_header,
                    2 => second_header,
                    _ => return unsupported(),
                };
                Ok(json!(serialize_hex(&header)))
            }
            "blockchain.scripthash.listunspent" => Ok(json!(unspent)),
            "blockchain.transaction.get" => {
                let txid = request["params"][0].as_str().unwrap();
                Ok(json!(serialize_hex(transactions.get(txid).unwrap())))
            }
            _ => unsupported(),
        }
    })
    .await;
    let outputs = server
        .chain()
        .await
        .find_outpoints(&address())
        .await
        .unwrap();
    assert_eq!(outputs.len(), 1_100);
    assert_eq!(
        outputs
            .iter()
            .map(|output| output.amount.to_sat())
            .sum::<u64>(),
        5_500_000
    );
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.confirmations == 2)
            .count(),
        550
    );
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.confirmations == 1)
            .count(),
        550
    );
}
