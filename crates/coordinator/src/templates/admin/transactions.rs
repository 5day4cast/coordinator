//! Public transaction records with fees derived only from verified previous-output evidence.
use bitcoin::{hex::DisplayHex, Transaction};
use maud::{html, Markup};

pub fn transaction_diagram(
    tx: &Transaction,
    network: &str,
    explorer_url: &str,
    title: &str,
    status: &str,
    psbt: Option<&str>,
    previous: Option<&Transaction>,
) -> Markup {
    let txid = tx.compute_txid();
    let psbt = psbt
        .and_then(|text| text.parse::<bitcoin::Psbt>().ok())
        .filter(|p| p.unsigned_tx.compute_txid() == txid);
    let psbt_outputs: Vec<_> = psbt
        .as_ref()
        .map(|p| {
            p.iter_funding_utxos()
                .map(|out| out.ok().cloned())
                .collect()
        })
        .unwrap_or_default();
    let inputs: Vec<_> = tx
        .input
        .iter()
        .enumerate()
        .map(|(index, input)| {
            psbt_outputs.get(index).cloned().flatten().or_else(|| {
                previous
                    .filter(|p| p.compute_txid() == input.previous_output.txid)
                    .and_then(|p| p.output.get(input.previous_output.vout as usize))
                    .cloned()
            })
        })
        .collect();
    let outputs: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
    let total_input = inputs.iter().try_fold(0_u64, |total, output| {
        total.checked_add(output.as_ref()?.value.to_sat())
    });
    let fee = total_input.and_then(|total| total.checked_sub(outputs));
    let network = network.parse::<bitcoin::Network>().ok();
    let record = serde_json::json!({
        "txid": txid.to_string(),
        "status": status,
        "version": tx.version.0,
        "lock_time": tx.lock_time.to_consensus_u32(),
        "size_bytes": tx.total_size(),
        "vsize": tx.vsize(),
        "signals_rbf": tx.is_explicitly_rbf(),
        "input_sats": total_input,
        "output_sats": outputs,
        "fee_sats": fee,
        "fee_sat_vb": fee.map(|fee| fee as f64 / tx.vsize() as f64),
        "inputs": tx.input.iter().enumerate().map(|(index, input)| serde_json::json!({
            "previous_output": input.previous_output.to_string(),
            "value_sats": inputs[index].as_ref().map(|out| out.value.to_sat()),
            "sequence": input.sequence.to_consensus_u32(),
            "script_sig": input.script_sig.to_hex_string(),
            "witness": input.witness.iter().map(|item| item.to_lower_hex_string()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "outputs": tx.output.iter().enumerate().map(|(index, output)| serde_json::json!({
            "vout": index,
            "value_sats": output.value.to_sat(),
            "script_pubkey": output.script_pubkey.to_hex_string(),
            "address": network.and_then(|n| bitcoin::Address::from_script(&output.script_pubkey, n).ok()).map(|a| a.to_string()),
        })).collect::<Vec<_>>(),
    });
    html! {
        details.transaction-record {
            summary { strong { (title) " transaction" } " · " code { (txid.to_string().chars().take(8).collect::<String>()) "…" (txid.to_string().chars().rev().take(8).collect::<String>().chars().rev().collect::<String>()) } }
            div.transaction-detail {
                @if !explorer_url.is_empty() { a href=(format!("{}/tx/{txid}", explorer_url.trim_end_matches('/'))) rel="noreferrer" { "Open in explorer ↗" } }
                pre.transaction-json tabindex="0" aria-label=(format!("{title} decoded transaction JSON")) { code { (serde_json::to_string_pretty(&record).expect("public transaction JSON")) } }
                details { summary { "Raw transaction hex" } pre.transaction-hex tabindex="0" { code { (bitcoin::consensus::encode::serialize_hex(tx)) } } }
                p.note { "Null values are unknown. Fees require complete previous outputs. An RBF signal does not establish that replacement is safe for the contract." }
            }
        }
    }
}
