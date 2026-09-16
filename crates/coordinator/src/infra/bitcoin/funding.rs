use super::*;
use bitcoin::psbt::raw::ProprietaryKey;
use std::collections::HashSet;

fn leases_key() -> ProprietaryKey {
    ProprietaryKey {
        prefix: b"coordinator".to_vec(),
        subtype: 0,
        key: b"lnd-input-leases".to_vec(),
    }
}

impl LndWallet {
    /// The legacy template mode accepts unlocked wallet inputs and acquires
    /// leases for them. An explicit selection is exhaustive; an empty selection
    /// lets LND select coins. Foreign script inputs must not reach this RPC:
    /// LND's coin_select mode cannot estimate P2WSH witness weights.
    pub(super) async fn fund_psbt(
        &self,
        template: &Psbt,
        fee_rate: FeeRate,
    ) -> Result<Psbt, anyhow::Error> {
        let mut seen = HashSet::new();
        if template
            .unsigned_tx
            .input
            .iter()
            .any(|input| !seen.insert(input.previous_output))
        {
            return Err(anyhow!("Duplicate wallet funding input"));
        }
        let response: Value = self
            .post(
                "v2/wallet/psbt/fund",
                json!({
                    "psbt": BASE64.encode(template.serialize()),
                    "sat_per_vbyte": fee_rate.to_sat_per_vb_ceil().max(1).to_string(),
                    "min_confs": 1,
                    "spend_unconfirmed": false,
                    "change_type": "CHANGE_ADDRESS_TYPE_P2TR",
                    "custom_lock_id": BASE64.encode(rand::random::<[u8; 32]>()),
                }),
            )
            .await?;
        let leases = response["locked_utxos"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut psbt = match decode_psbt(&response["funded_psbt"]) {
            Ok(psbt) => psbt,
            Err(error) => {
                if let Err(release_error) = self.release_leases(&leases).await {
                    warn!(
                        "Failed to release inputs after invalid funded PSBT: {}",
                        release_error
                    );
                }
                return Err(error);
            }
        };
        // Keep lease ownership with the PSBT through signing and persistence.
        // LND preserves proprietary fields when signing/finalizing the packet.
        psbt.proprietary
            .insert(leases_key(), serde_json::to_vec(&leases)?);
        Ok(psbt)
    }

    /// Release only the leases acquired while creating this packet. Call on
    /// definite failures before publication; a publish timeout has an unknown
    /// outcome and must retain the leases until LND observes the spend/expiry.
    pub(super) async fn release_psbt_inputs(&self, psbt: &Psbt) -> Result<(), anyhow::Error> {
        let Some(encoded) = psbt.proprietary.get(&leases_key()) else {
            return Ok(());
        };
        self.release_leases(&serde_json::from_slice::<Vec<Value>>(encoded)?)
            .await
    }

    pub(super) async fn reserve_psbt_inputs_until(
        &self,
        psbt: &Psbt,
        deadline: u64,
    ) -> Result<(), anyhow::Error> {
        let Some(encoded) = psbt.proprietary.get(&leases_key()) else {
            // Foreign-only packets do not own wallet inputs. Older persisted
            // wallet packets without ownership metadata cannot be safely renewed.
            if psbt.inputs.iter().all(|input| {
                input
                    .witness_utxo
                    .as_ref()
                    .is_some_and(|output| output.script_pubkey.is_p2wsh())
            }) {
                return Ok(());
            }
            return Err(anyhow!(
                "PSBT has wallet inputs but no LND lease ownership metadata"
            ));
        };
        let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp())?;
        let duration = deadline
            .checked_sub(now)
            .filter(|duration| *duration > 0)
            .ok_or_else(|| anyhow!("Wallet reservation deadline must be in the future"))?;
        // LND converts seconds into Go's signed nanosecond Duration.
        if duration > (i64::MAX as u64) / 1_000_000_000 {
            return Err(anyhow!(
                "Wallet reservation deadline exceeds LND's supported duration"
            ));
        }
        let leases: Vec<Value> = serde_json::from_slice(encoded)?;
        for lease in leases {
            self.post::<Value>(
                "v2/wallet/utxos/lease",
                json!({ "id": lease["id"], "outpoint": lease["outpoint"], "expiration_seconds": duration.to_string() }),
            ).await?;
        }
        Ok(())
    }

    async fn release_leases(&self, leases: &[Value]) -> Result<(), anyhow::Error> {
        let mut first_error = None;
        for lease in leases {
            let result = self
                .post::<Value>(
                    "v2/wallet/utxos/release",
                    json!({ "id": lease["id"], "outpoint": lease["outpoint"] }),
                )
                .await;
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn fee_for_weight(weight: u64, fee_rate: FeeRate) -> Result<u64, anyhow::Error> {
    weight
        .div_ceil(4)
        .checked_mul(fee_rate.to_sat_per_vb_ceil().max(1))
        .ok_or_else(|| anyhow!("Transaction fee overflow"))
}

fn foreign_totals(
    selected: &[OutPoint],
    foreign: &[ForeignUtxo],
) -> Result<(u64, u64), anyhow::Error> {
    let mut seen = HashSet::new();
    for outpoint in selected {
        if !seen.insert(*outpoint) {
            return Err(anyhow!("Duplicate selected input {}", outpoint));
        }
    }
    let mut value = 0u64;
    let mut satisfaction_weight = 0u64;
    for utxo in foreign {
        if !seen.insert(utxo.outpoint) {
            return Err(anyhow!("Duplicate funding input {}", utxo.outpoint));
        }
        let input = &utxo.psbt;
        let output = input
            .witness_utxo
            .as_ref()
            .ok_or_else(|| anyhow!("Missing witness UTXO for {}", utxo.outpoint))?;
        let script = input
            .witness_script
            .as_ref()
            .ok_or_else(|| anyhow!("Missing witness script for {}", utxo.outpoint))?;
        if script.to_p2wsh() != output.script_pubkey {
            return Err(anyhow!(
                "Witness script does not match UTXO {}",
                utxo.outpoint
            ));
        }
        if let Some(previous) = &input.non_witness_utxo {
            if previous.compute_txid() != utxo.outpoint.txid
                || previous.output.get(utxo.outpoint.vout as usize) != Some(output)
            {
                return Err(anyhow!(
                    "Previous transaction does not match UTXO {}",
                    utxo.outpoint
                ));
            }
        }
        if !input.partial_sigs.is_empty()
            || input.final_script_witness.is_some()
            || input.final_script_sig.is_some()
        {
            return Err(anyhow!(
                "Cannot fund an already signed foreign input {}",
                utxo.outpoint
            ));
        }
        value = value
            .checked_add(output.value.to_sat())
            .ok_or_else(|| anyhow!("Foreign input value overflow"))?;
        satisfaction_weight = satisfaction_weight
            .checked_add(utxo.satisfaction_weight.to_wu())
            .ok_or_else(|| anyhow!("Foreign input weight overflow"))?;
    }
    Ok((value, satisfaction_weight))
}

/// Return a foreign-only packet when no wallet contribution is needed.
fn fund_foreign_only(
    target: &TxOut,
    change_script: &ScriptBuf,
    foreign: &[ForeignUtxo],
    foreign_value: u64,
    satisfaction_weight: u64,
    fee_rate: FeeRate,
) -> Result<Option<Psbt>, anyhow::Error> {
    let mut psbt = BitcoinClient::template_psbt(vec![target.clone()], &[], foreign.to_vec())?;
    // An unsigned transaction omits the segwit marker/flag and empty witness
    // counts. Miniscript's satisfaction weight is a delta from empty witnesses.
    let witness_weight = satisfaction_weight
        .checked_add(foreign.len() as u64)
        .and_then(|weight| weight.checked_add(2))
        .ok_or_else(|| anyhow!("Foreign input weight overflow"))?;
    let required_fee =
        fee_for_weight(psbt.unsigned_tx.weight().to_wu() + witness_weight, fee_rate)?;
    let Some(surplus) = foreign_value.checked_sub(target.value.to_sat()) else {
        return Ok(None);
    };
    if surplus < required_fee {
        return Ok(None);
    }

    let mut change = TxOut::minimal_non_dust(change_script.clone());
    psbt.unsigned_tx.output.push(change.clone());
    let fee_with_change =
        fee_for_weight(psbt.unsigned_tx.weight().to_wu() + witness_weight, fee_rate)?;
    if let Some(change_value) = surplus
        .checked_sub(fee_with_change)
        .filter(|value| *value >= change.value.to_sat())
    {
        change.value = Amount::from_sat(change_value);
        psbt.unsigned_tx.output[1] = change;
        psbt.outputs.push(Default::default());
    } else {
        psbt.unsigned_tx.output.pop();
    }
    Ok(Some(psbt))
}

/// Combine the wallet contribution with the original escrow inputs before any
/// party signs. The reserved change output guarantees somewhere to return the
/// padding needed when the wallet contribution alone would be a dust output.
fn combine_foreign(
    psbt: &mut Psbt,
    target: &TxOut,
    contribution: &TxOut,
    reserved_change: &TxOut,
    foreign: Vec<ForeignUtxo>,
    foreign_value: u64,
    foreign_fee: u64,
) -> Result<(), anyhow::Error> {
    let target_index = psbt
        .unsigned_tx
        .output
        .iter()
        .position(|output| output == contribution)
        .ok_or_else(|| anyhow!("LND omitted the wallet contribution output"))?;
    let change_index = psbt
        .unsigned_tx
        .output
        .iter()
        .position(|output| output == reserved_change)
        .ok_or_else(|| anyhow!("LND omitted the reserved change output"))?;
    let padding = foreign_value
        .checked_add(contribution.value.to_sat())
        .and_then(|value| value.checked_sub(target.value.to_sat()))
        .and_then(|value| value.checked_sub(foreign_fee))
        .ok_or_else(|| anyhow!("Insufficient wallet contribution"))?;
    let change_value = reserved_change
        .value
        .to_sat()
        .checked_add(padding)
        .ok_or_else(|| anyhow!("Change value overflow"))?;
    psbt.unsigned_tx.output[target_index] = target.clone();
    psbt.unsigned_tx.output[change_index].value = Amount::from_sat(change_value);
    for input in foreign {
        psbt.unsigned_tx.input.push(TxIn {
            previous_output: input.outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        });
        psbt.inputs.push(input.psbt);
    }
    Ok(())
}

impl LndWallet {
    pub(super) async fn fund_with_foreign(
        &self,
        network: Network,
        target: TxOut,
        fee_rate: FeeRate,
        selected: Vec<OutPoint>,
        foreign: Vec<ForeignUtxo>,
    ) -> Result<Psbt, anyhow::Error> {
        let (foreign_value, satisfaction_weight) = foreign_totals(&selected, &foreign)?;
        if target.value < target.script_pubkey.minimal_non_dust() {
            return Err(anyhow!("Funding output is below the dust threshold"));
        }
        if foreign.is_empty() {
            let template = BitcoinClient::template_psbt(vec![target], &selected, vec![])?;
            return self.fund_psbt(&template, fee_rate).await;
        }

        let change_script = self.next_address(network).await?.script_pubkey();
        if selected.is_empty() {
            if let Some(psbt) = fund_foreign_only(
                &target,
                &change_script,
                &foreign,
                foreign_value,
                satisfaction_weight,
                fee_rate,
            )? {
                return Ok(psbt);
            }
        }

        // Each additional segwit input contributes 164 base WU plus an empty
        // witness count and its satisfaction delta. Reserve the maximum CompactSize
        // input-count growth (32 WU), plus one vbyte for LND fee rounding.
        let extra_weight = (foreign.len() as u64)
            .checked_mul(165)
            .and_then(|weight| weight.checked_add(satisfaction_weight))
            .and_then(|weight| weight.checked_add(36))
            .ok_or_else(|| anyhow!("Foreign input weight overflow"))?;
        let foreign_fee = fee_for_weight(extra_weight, fee_rate)?;
        let contribution_value = target
            .value
            .to_sat()
            .checked_add(foreign_fee)
            .ok_or_else(|| anyhow!("Funding amount overflow"))?
            .saturating_sub(foreign_value)
            .max(target.script_pubkey.minimal_non_dust().to_sat());
        let contribution = TxOut {
            value: Amount::from_sat(contribution_value),
            script_pubkey: target.script_pubkey.clone(),
        };
        let reserved_change = TxOut::minimal_non_dust(change_script);
        let template = BitcoinClient::template_psbt(
            vec![contribution.clone(), reserved_change.clone()],
            &selected,
            vec![],
        )?;
        let mut funded = self.fund_psbt(&template, fee_rate).await?;
        if let Err(error) = combine_foreign(
            &mut funded,
            &target,
            &contribution,
            &reserved_change,
            foreign,
            foreign_value,
            foreign_fee,
        ) {
            if let Err(release_error) = self.release_psbt_inputs(&funded).await {
                warn!(
                    "Failed to release inputs after funding error: {}",
                    release_error
                );
            }
            return Err(error);
        }
        Ok(funded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::escrow::create_escrow_descriptor;
    use axum::{routing::post, Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{net::TcpListener, sync::mpsc, time::timeout};

    fn foreign(value: u64) -> ForeignUtxo {
        let secp = Secp256k1::new();
        let coordinator =
            PublicKey::new(SecretKey::from_slice(&[1; 32]).unwrap().public_key(&secp));
        let user = PublicKey::new(SecretKey::from_slice(&[2; 32]).unwrap().public_key(&secp));
        let descriptor = create_escrow_descriptor(&coordinator, &user, &[3; 32]).unwrap();
        let output = TxOut {
            value: Amount::from_sat(value),
            script_pubkey: descriptor.script_pubkey(),
        };
        let previous = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![output.clone()],
        };
        ForeignUtxo {
            outpoint: OutPoint {
                txid: previous.compute_txid(),
                vout: 0,
            },
            psbt: Input {
                witness_utxo: Some(output),
                non_witness_utxo: Some(previous),
                witness_script: Some(descriptor.explicit_script().unwrap()),
                ..Default::default()
            },
            satisfaction_weight: descriptor.max_weight_to_satisfy().unwrap(),
        }
    }

    fn target(value: u64, marker: u8) -> TxOut {
        let script = ScriptBuf::from_hex(&format!("5120{}", hex::encode([marker; 32]))).unwrap();
        TxOut {
            value: Amount::from_sat(value),
            script_pubkey: script,
        }
    }

    #[test]
    fn escrow_surplus_funds_without_wallet_inputs_and_pays_witness_fee() {
        let foreign = foreign(22_000);
        let (value, weight) = foreign_totals(&[], std::slice::from_ref(&foreign)).unwrap();
        let rate = FeeRate::from_sat_per_vb_unchecked(3);
        let funded = fund_foreign_only(
            &target(20_000, 4),
            &target(0, 5).script_pubkey,
            std::slice::from_ref(&foreign),
            value,
            weight,
            rate,
        )
        .unwrap()
        .unwrap();
        assert_eq!(funded.unsigned_tx.input.len(), 1);
        assert_eq!(funded.unsigned_tx.output[0], target(20_000, 4));
        assert_eq!(funded.inputs[0], foreign.psbt);
        assert_eq!(funded.unsigned_tx.output.len(), 2);
        let mut signed = funded.unsigned_tx.clone();
        signed.input[0].witness = Witness::from_slice(&[
            vec![],
            vec![0; 72],
            vec![0; 72],
            foreign.psbt.witness_script.unwrap().into_bytes(),
        ]);
        assert!(
            funded.fee().unwrap().to_sat()
                >= fee_for_weight(signed.weight().to_wu(), rate).unwrap()
        );
    }

    #[test]
    fn escrow_without_fee_surplus_requires_wallet_contribution() {
        let foreign = foreign(20_000);
        let (value, weight) = foreign_totals(&[], std::slice::from_ref(&foreign)).unwrap();
        assert!(fund_foreign_only(
            &target(20_000, 4),
            &target(0, 5).script_pubkey,
            &[foreign],
            value,
            weight,
            FeeRate::from_sat_per_vb_unchecked(3),
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn combining_escrow_preserves_scripts_and_returns_dust_padding() {
        let foreign = foreign(20_000);
        let target_output = target(20_000, 4);
        let contribution = TxOut {
            value: Amount::from_sat(330),
            ..target_output.clone()
        };
        let change = TxOut::minimal_non_dust(target(0, 5).script_pubkey);
        // LND may sort the outputs. Locate outputs by their complete contents.
        let mut funded = BitcoinClient::template_psbt(
            vec![change.clone(), contribution.clone()],
            &[OutPoint::null()],
            vec![],
        )
        .unwrap();
        funded.inputs[0].witness_utxo = Some(target(5_000, 6));
        let wallet_fee = funded.fee().unwrap();
        combine_foreign(
            &mut funded,
            &target_output,
            &contribution,
            &change,
            vec![foreign.clone()],
            20_000,
            200,
        )
        .unwrap();
        assert_eq!(funded.unsigned_tx.output[1], target_output);
        assert_eq!(
            funded.unsigned_tx.output[0].value,
            change.value + Amount::from_sat(130)
        );
        assert_eq!(funded.inputs[1], foreign.psbt);
        assert_eq!(funded.fee().unwrap(), wallet_fee + Amount::from_sat(200));
    }

    #[test]
    fn duplicate_or_mismatched_foreign_inputs_are_rejected() {
        let mut foreign = foreign(20_000);
        assert!(foreign_totals(&[foreign.outpoint], &[foreign.clone()]).is_err());
        foreign.psbt.witness_utxo.as_mut().unwrap().value = Amount::from_sat(1);
        assert!(foreign_totals(&[], &[foreign.clone()]).is_err());
        foreign.psbt.witness_script = Some(ScriptBuf::new());
        assert!(foreign_totals(&[], &[foreign]).is_err());
    }

    fn test_wallet(base_url: String) -> LndWallet {
        let path = std::env::temp_dir().join(format!(
            "coordinator-test-{}.macaroon",
            uuid::Uuid::now_v7()
        ));
        fs::write(&path, [1, 2, 3]).unwrap();
        let wallet = LndWallet::new(&LnSettings {
            base_url,
            macaroon_file_path: path.to_string_lossy().into_owned(),
            tls_cert_path: None,
            ..Default::default()
        });
        fs::remove_file(path).unwrap();
        wallet.unwrap()
    }

    #[tokio::test]
    async fn selected_wallet_funding_acquires_and_releases_its_own_leases() {
        let template =
            BitcoinClient::template_psbt(vec![target(20_000, 4)], &[OutPoint::null()], vec![])
                .unwrap();
        let lease = json!({ "id": BASE64.encode([9; 32]), "outpoint": { "txid_str": OutPoint::null().txid.to_string(), "output_index": u32::MAX } });
        let response = json!({ "funded_psbt": BASE64.encode(template.serialize()), "locked_utxos": [lease.clone()] });
        let (requests, mut received) = mpsc::channel(3);
        let funding_requests = requests.clone();
        let renewal_requests = requests.clone();
        let app = Router::new()
            .route(
                "/v2/wallet/psbt/fund",
                post(move |Json(body): Json<Value>| {
                    let requests = funding_requests.clone();
                    let response = response.clone();
                    async move {
                        requests.send(body).await.unwrap();
                        Json(response)
                    }
                }),
            )
            .route(
                "/v2/wallet/utxos/lease",
                post(move |Json(body): Json<Value>| {
                    let requests = renewal_requests.clone();
                    async move {
                        requests.send(body).await.unwrap();
                        Json(json!({}))
                    }
                }),
            )
            .route(
                "/v2/wallet/utxos/release",
                post(move |Json(body): Json<Value>| {
                    let requests = requests.clone();
                    async move {
                        requests.send(body).await.unwrap();
                        Json(json!({}))
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let wallet = test_wallet(format!("http://{}", listener.local_addr().unwrap()));
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap()
        });
        let funded = wallet
            .fund_psbt(&template, FeeRate::from_sat_per_vb_unchecked(2))
            .await
            .unwrap();
        assert!(wallet.reserve_psbt_inputs_until(&funded, 0).await.is_err());
        let deadline = time::OffsetDateTime::now_utc().unix_timestamp() as u64 + 3600;
        wallet
            .reserve_psbt_inputs_until(&funded, deadline)
            .await
            .unwrap();
        wallet.release_psbt_inputs(&funded).await.unwrap();
        let request = timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(request.get("coin_select").is_none());
        assert_eq!(decode_psbt(&request["psbt"]).unwrap(), template);
        assert_eq!(
            BASE64
                .decode(request["custom_lock_id"].as_str().unwrap())
                .unwrap()
                .len(),
            32
        );
        let renewal = timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renewal["id"], lease["id"]);
        assert_eq!(renewal["outpoint"], lease["outpoint"]);
        assert!((3595..=3600).contains(&json_u64(&renewal["expiration_seconds"]).unwrap()));
        assert_eq!(
            timeout(Duration::from_secs(2), received.recv())
                .await
                .unwrap()
                .unwrap(),
            lease
        );
        cancel.cancel();
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn wallet_mutations_are_not_retried_after_uncertain_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let app = Router::new().route(
            "/spend",
            post(move || {
                handler_calls.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({ "message": "response unavailable" })),
                    )
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let wallet = test_wallet(format!("http://{}", listener.local_addr().unwrap()));
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap()
        });
        assert!(wallet.post::<Value>("spend", json!({})).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cancel.cancel();
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    /// Requires an isolated regtest LND with at least two confirmed wallet
    /// UTXOs. This test broadcasts transactions only after checking the network.
    #[tokio::test]
    #[ignore = "requires COORDINATOR_TEST_LND_URL and COORDINATOR_TEST_LND_MACAROON on isolated regtest"]
    async fn live_lnd_funds_signs_and_publishes_external_escrow() {
        let wallet = LndWallet::new(&LnSettings {
            base_url: std::env::var("COORDINATOR_TEST_LND_URL").unwrap(),
            macaroon_file_path: std::env::var("COORDINATOR_TEST_LND_MACAROON").unwrap(),
            tls_cert_path: std::env::var("COORDINATOR_TEST_LND_CERT").ok(),
            ..Default::default()
        })
        .unwrap();
        let info: Value = wallet.get("v1/getinfo").await.unwrap();
        assert_eq!(info["chains"][0]["chain"], "bitcoin");
        assert_eq!(info["chains"][0]["network"], "regtest");
        let confirmed: Vec<_> = wallet
            .list_unspent()
            .await
            .unwrap()
            .into_iter()
            .filter(WalletUtxo::is_confirmed)
            .collect();
        assert!(
            confirmed.len() >= 2,
            "Need two confirmed regtest wallet UTXOs"
        );
        let destination = wallet.next_address(Network::Regtest).await.unwrap();
        let target = TxOut {
            value: Amount::from_sat(20_000),
            script_pubkey: destination.script_pubkey(),
        };
        let rate = FeeRate::from_sat_per_vb_unchecked(2);

        // Explicit selections use LND's legacy PSBT mode, which must accept
        // an unlocked output and return the lease that we then release.
        let selected =
            BitcoinClient::template_psbt(vec![target.clone()], &[confirmed[0].outpoint], vec![])
                .unwrap();
        let selected = wallet.fund_psbt(&selected, rate).await.unwrap();
        assert_eq!(selected.unsigned_tx.input.len(), 1);
        assert_eq!(
            selected.unsigned_tx.input[0].previous_output,
            confirmed[0].outpoint
        );
        let deadline = time::OffsetDateTime::now_utc().unix_timestamp() as u64 + 3 * 86400;
        wallet
            .reserve_psbt_inputs_until(&selected, deadline)
            .await
            .unwrap();
        let leases: Value = wallet
            .post("v2/wallet/utxos/leases", json!({}))
            .await
            .unwrap();
        let owned: Vec<Value> =
            serde_json::from_slice(selected.proprietary.get(&leases_key()).unwrap()).unwrap();
        let renewed = leases["locked_utxos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|lease| {
                lease["id"] == owned[0]["id"] && lease["outpoint"] == owned[0]["outpoint"]
            })
            .expect("Renewed lease must remain owned by this PSBT");
        assert!(json_u64(&renewed["expiration"]).unwrap() >= deadline);
        wallet.release_psbt_inputs(&selected).await.unwrap();

        let mut escrow = foreign(20_000);
        let template = BitcoinClient::template_psbt(
            vec![escrow.psbt.witness_utxo.clone().unwrap()],
            &[],
            vec![],
        )
        .unwrap();
        let funded = wallet.fund_psbt(&template, rate).await.unwrap();
        let escrow_tx = wallet
            .finalize_psbt(&funded)
            .await
            .unwrap()
            .extract_tx()
            .unwrap();
        wallet
            .publish(&escrow_tx, "coordinator regtest escrow")
            .await
            .unwrap();
        let escrow_index = escrow_tx
            .output
            .iter()
            .position(|output| Some(output) == escrow.psbt.witness_utxo.as_ref())
            .unwrap();
        escrow.outpoint = OutPoint {
            txid: escrow_tx.compute_txid(),
            vout: escrow_index as u32,
        };
        escrow.psbt.non_witness_utxo = Some(escrow_tx);

        let mut funded = wallet
            .fund_with_foreign(Network::Regtest, target.clone(), rate, vec![], vec![escrow])
            .await
            .unwrap();
        assert!(funded.unsigned_tx.input.len() >= 2);
        assert!(funded.unsigned_tx.output.contains(&target));
        funded = wallet.sign_psbt(&funded).await.unwrap();
        let tx = funded.unsigned_tx.clone();
        let secp = Secp256k1::new();
        for (index, input) in funded.inputs.iter_mut().enumerate() {
            let Some(script) = input.witness_script.as_ref() else {
                continue;
            };
            let digest = SighashCache::new(&tx)
                .p2wsh_signature_hash(
                    index,
                    script,
                    input.witness_utxo.as_ref().unwrap().value,
                    EcdsaSighashType::All,
                )
                .unwrap();
            for secret in [[1; 32], [2; 32]] {
                let secret = SecretKey::from_slice(&secret).unwrap();
                input.partial_sigs.insert(
                    PublicKey::new(secret.public_key(&secp)),
                    ecdsa::Signature {
                        signature: secp
                            .sign_ecdsa(&Message::from_digest(digest.to_byte_array()), &secret),
                        sighash_type: EcdsaSighashType::All,
                    },
                );
            }
        }
        BitcoinClient::finalize_escrow_inputs(&mut funded).unwrap();
        let finalized = wallet.finalize_psbt(&funded).await.unwrap();
        let fee = finalized.fee().unwrap();
        let tx = finalized.extract_tx().unwrap();
        assert!(fee.to_sat() >= fee_for_weight(tx.weight().to_wu(), rate).unwrap());
        wallet
            .publish(&tx, "coordinator regtest foreign funding")
            .await
            .unwrap();
        println!(
            "Published external escrow funding {} with fee {} sats, {} vB",
            tx.compute_txid(),
            fee.to_sat(),
            tx.vsize()
        );
    }
}
