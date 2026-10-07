//! The recovery page's side of `coordinator-recover`: inspect entries and build claims in the
//! browser, from the player's nsec, the recovery file and what relays and Esplora return.
//!
//! The page's script does the fetching (relays over WebSocket, Esplora and the oracle over HTTP)
//! and hands the results in as JSON; everything that decides or signs happens here. Arkade
//! refunds and unrolls stay in the CLI: they need arkd's gRPC API.

use bitcoin::{Address, FeeRate, Network};
use coordinator_recover::chain::Answer;
use coordinator_recover::{attestation, defaults, parse_network, spec::Kit, Identity, Session};
use serde_json::{json, Value};
use std::str::FromStr;
use uuid::Uuid;

/// Most chain lookups the page answers per round, so a hostile record cannot make it fetch
/// without end.
const MAX_QUERIES: usize = 64;

pub struct RecoveryCore {
    session: Session,
}

impl RecoveryCore {
    pub fn new(nsec: &str, network: &str) -> Result<Self, String> {
        let identity = Identity::from_nsec(nsec).map_err(|e| e.to_string())?;
        let network = parse_network(network).map_err(|e| e.to_string())?;
        Ok(Self {
            session: Session::new(identity, network),
        })
    }

    pub fn network(&self) -> Network {
        self.session.network()
    }

    /// This build's coordinator recovery key for the session's network and its default relays:
    /// `{coordinator_pubkey, relays}`, the key null when the build has none.
    pub fn defaults(&self) -> String {
        let defaults = defaults::built_in();
        json!({
            "coordinator_pubkey": defaults
                .coordinator(self.network())
                .map(|key| key.to_hex()),
            "relays": defaults.relays,
        })
        .to_string()
    }

    pub fn set_coordinator(&mut self, pubkey: &str) -> Result<(), String> {
        let pubkey = coordinator_recover::spec::parse_pubkey(pubkey, "coordinator pubkey")
            .map_err(|e| e.to_string())?;
        self.session.set_coordinator(pubkey);
        Ok(())
    }

    /// Add the recovery file; returns the relays it names.
    pub fn add_kit(&mut self, json: &str) -> Result<Vec<String>, String> {
        let kit = Kit::parse(json).map_err(|e| e.to_string())?;
        self.session.add_kit(kit).map_err(|e| e.to_string())?;
        Ok(self.session.kit_relays())
    }

    /// The relay filter for this player's records, as JSON.
    pub fn player_filter(&self) -> Result<String, String> {
        self.session
            .player_filter()
            .map(|filter| filter.to_string())
            .map_err(|e| e.to_string())
    }

    /// Add events, a JSON array, as relays returned them. Returns how many parsed.
    pub fn add_events(&mut self, json: &str) -> Result<usize, String> {
        let events = parse_events(json)?;
        let count = events.len();
        self.session.add_events(events);
        Ok(count)
    }

    /// Decrypt what was found. Returns `{entries, competition_filter, warnings}`.
    pub fn load(&mut self) -> Result<String, String> {
        self.session.load().map_err(|e| e.to_string())?;
        Ok(json!({
            "entries": self.session.entries().count(),
            "competition_filter": self.session.competition_filter(),
            "warnings": self.session.warnings,
        })
        .to_string())
    }

    /// The attestations still wanted: `[{competition_id, event_id, filters}]`.
    pub fn attestation_requests(&self) -> String {
        Value::Array(
            self.session
                .attestations_wanted()
                .into_iter()
                .map(|(competition_id, event_id)| {
                    json!({
                        "competition_id": competition_id,
                        "filters": attestation::relay_filters(&event_id),
                        "event_id": event_id,
                    })
                })
                .collect(),
        )
        .to_string()
    }

    /// Offer the oracle API's event JSON for a competition; true if it held the attestation.
    pub fn offer_oracle_event(&mut self, competition_id: &str, json: &str) -> Result<bool, String> {
        let competition_id = Uuid::parse_str(competition_id).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        Ok(self
            .session
            .offer_attestation(competition_id, &attestation::candidates_in_json(&value)))
    }

    /// Offer relay events that may hold attestations.
    pub fn offer_attestation_events(&mut self, json: &str) -> Result<(), String> {
        let events = parse_events(json)?;
        self.session.offer_attestation_events(&events);
        Ok(())
    }

    pub fn set_tip(&mut self, height: u32, median_time_past: u64) {
        self.session.chain.set_tip(height, median_time_past);
    }

    /// The seconds between blocks lately, for saying about when a step opens.
    pub fn set_block_interval(&mut self, seconds: u32) {
        self.session.chain.set_block_interval(seconds);
    }

    /// Add answers to chain lookups, a JSON array of `{"tx": …}` / `{"outspend": …}`.
    pub fn add_chain(&mut self, json: &str) -> Result<(), String> {
        let answers: Vec<Answer> = serde_json::from_str(json).map_err(|e| e.to_string())?;
        for answer in answers {
            self.session.chain.answer(answer);
        }
        Ok(())
    }

    /// `{reports: [{report, text}], missing, warnings}`. Answer `missing` and ask again until
    /// it is empty.
    pub fn inspect(&self, now: u64) -> String {
        let reports: Vec<Value> = self
            .session
            .inspect(now)
            .into_iter()
            .map(|report| json!({"text": report.to_string(), "report": report}))
            .collect();
        let missing: Vec<_> = self
            .session
            .chain
            .missing()
            .into_iter()
            .take(MAX_QUERIES)
            .collect();
        json!({"reports": reports, "missing": missing, "warnings": self.session.warnings})
            .to_string()
    }

    /// `{plan, missing}` for one entry; `plan` is null until `missing` is answered.
    pub fn claim(
        &self,
        entry_id: &str,
        address: &str,
        fee_rate_sat_vb: f64,
        ticket_preimage: &str,
    ) -> Result<String, String> {
        let entry_id = Uuid::parse_str(entry_id).map_err(|e| e.to_string())?;
        let destination = match address.trim() {
            "" => None,
            address => Some(
                Address::from_str(address)
                    .map_err(|e| format!("{address}: {e}"))?
                    .require_network(self.network())
                    .map_err(|e| format!("{address}: {e}"))?
                    .script_pubkey(),
            ),
        };
        if !fee_rate_sat_vb.is_finite() || fee_rate_sat_vb < 1.0 {
            return Err("the fee rate must be at least 1 sat/vB".into());
        }
        let fee_rate = FeeRate::from_sat_per_kwu((fee_rate_sat_vb * 250.0).ceil() as u64);
        let preimage = Some(ticket_preimage.trim()).filter(|p| !p.is_empty());
        let plan = self
            .session
            .claim(entry_id, destination, fee_rate, preimage);
        let missing = self.session.chain.missing();
        if !missing.is_empty() {
            return Ok(json!({"plan": null, "missing": missing}).to_string());
        }
        let plan = plan.map_err(|e| e.to_string())?;
        let steps = |txs: &[coordinator_recover::ClaimTx]| -> Vec<Value> {
            txs.iter()
                .map(|step| {
                    json!({
                        "label": step.label,
                        "txid": step.tx.compute_txid(),
                        "hex": bitcoin::consensus::encode::serialize_hex(&step.tx),
                        "bump": step.bump.describe(),
                    })
                })
                .collect()
        };
        Ok(json!({
            "plan": {
                "steps": steps(&plan.txs),
                "unconfirmed": steps(&plan.unconfirmed),
                "done": plan.done,
                "waiting": plan.waiting,
                "deadline": plan.deadline,
            },
            "missing": [],
        })
        .to_string())
    }
}

fn parse_events(json: &str) -> Result<Vec<nostr::Event>, String> {
    let values: Vec<Value> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    // A relay's malformed event is skipped, not fatal.
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use wasm_bindgen::prelude::*;

    /// The recovery page's handle on a session. It holds the nsec's key pair, never the nsec
    /// as text, and has no way to export it.
    #[wasm_bindgen]
    pub struct RecoveryTool(super::RecoveryCore);

    fn error(message: String) -> JsValue {
        JsValue::from_str(&message)
    }

    #[wasm_bindgen]
    impl RecoveryTool {
        #[wasm_bindgen(constructor)]
        pub fn new(nsec: &str, network: &str) -> Result<RecoveryTool, JsValue> {
            super::RecoveryCore::new(nsec, network)
                .map(RecoveryTool)
                .map_err(error)
        }

        #[wasm_bindgen(js_name = setCoordinator)]
        pub fn set_coordinator(&mut self, pubkey: &str) -> Result<(), JsValue> {
            self.0.set_coordinator(pubkey).map_err(error)
        }

        /// Returns the relays the file names, as a JSON array.
        #[wasm_bindgen(js_name = addKit)]
        pub fn add_kit(&mut self, json: &str) -> Result<String, JsValue> {
            self.0
                .add_kit(json)
                .map(|relays| serde_json::Value::from(relays).to_string())
                .map_err(error)
        }

        #[wasm_bindgen(js_name = playerFilter)]
        pub fn player_filter(&self) -> Result<String, JsValue> {
            self.0.player_filter().map_err(error)
        }

        #[wasm_bindgen(js_name = addEvents)]
        pub fn add_events(&mut self, json: &str) -> Result<usize, JsValue> {
            self.0.add_events(json).map_err(error)
        }

        pub fn load(&mut self) -> Result<String, JsValue> {
            self.0.load().map_err(error)
        }

        #[wasm_bindgen(js_name = attestationRequests)]
        pub fn attestation_requests(&self) -> String {
            self.0.attestation_requests()
        }

        #[wasm_bindgen(js_name = offerOracleEvent)]
        pub fn offer_oracle_event(
            &mut self,
            competition_id: &str,
            json: &str,
        ) -> Result<bool, JsValue> {
            self.0
                .offer_oracle_event(competition_id, json)
                .map_err(error)
        }

        #[wasm_bindgen(js_name = offerAttestationEvents)]
        pub fn offer_attestation_events(&mut self, json: &str) -> Result<(), JsValue> {
            self.0.offer_attestation_events(json).map_err(error)
        }

        #[wasm_bindgen(js_name = setTip)]
        pub fn set_tip(&mut self, height: u32, median_time_past: f64) {
            self.0.set_tip(height, median_time_past as u64);
        }

        #[wasm_bindgen(js_name = setBlockInterval)]
        pub fn set_block_interval(&mut self, seconds: u32) {
            self.0.set_block_interval(seconds);
        }

        /// This build's coordinator recovery key and relays, as JSON.
        pub fn defaults(&self) -> String {
            self.0.defaults()
        }

        #[wasm_bindgen(js_name = addChain)]
        pub fn add_chain(&mut self, json: &str) -> Result<(), JsValue> {
            self.0.add_chain(json).map_err(error)
        }

        pub fn inspect(&self, now: f64) -> String {
            self.0.inspect(now as u64)
        }

        pub fn claim(
            &self,
            entry_id: &str,
            address: &str,
            fee_rate_sat_vb: f64,
            ticket_preimage: &str,
        ) -> Result<String, JsValue> {
            self.0
                .claim(entry_id, address, fee_rate_sat_vb, ticket_preimage)
                .map_err(error)
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::RecoveryTool;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_the_page_flow_without_records() {
        let mut core = RecoveryCore::new(&"01".repeat(32), "signet").unwrap();
        assert!(core.player_filter().is_err());
        core.set_coordinator("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
            .unwrap();
        let filter: Value = serde_json::from_str(&core.player_filter().unwrap()).unwrap();
        assert_eq!(filter["kinds"], json!([30078]));
        assert_eq!(filter["#b"].as_array().unwrap().len(), 1);
        assert_eq!(core.add_events("[{\"not\":\"an event\"}]").unwrap(), 0);
        // No wallet backup anywhere: the page says so.
        assert!(core.load().unwrap_err().contains("No wallet backup"));
        let inspected: Value = serde_json::from_str(&core.inspect(0)).unwrap();
        assert_eq!(inspected["reports"], json!([]));
        assert_eq!(inspected["missing"], json!([]));
        assert!(RecoveryCore::new("nsec1bad", "signet").is_err());
        assert!(RecoveryCore::new(&"01".repeat(32), "litecoin").is_err());
    }

    #[test]
    fn offers_the_built_in_relays() {
        let core = RecoveryCore::new(&"01".repeat(32), "signet").unwrap();
        let defaults: Value = serde_json::from_str(&core.defaults()).unwrap();
        assert_eq!(
            defaults["relays"],
            json!(coordinator_recover::defaults::built_in().relays)
        );
        assert!(!defaults["relays"].as_array().unwrap().is_empty());
    }
}
