//! The CLI's I/O: Nostr relays, Esplora, the oracle's API and Arkade.

pub mod ark;
pub mod esplora;
pub mod relays;
mod ws;

use std::time::Duration;

use bitcoin::Network;
use serde_json::Value;

use crate::attestation;
use crate::Session;
use esplora::Esplora;

/// The coordinator's recovery key on each network, for a player with only their nsec. A key is
/// listed here once the coordinator has published it (`GET /api/v1/recovery/info`); until then,
/// pass `--coordinator-pubkey` or the recovery file, which names it.
const COORDINATOR_PUBKEYS: &[(Network, &str)] = &[];

pub fn default_coordinator_pubkey(network: Network) -> Option<&'static str> {
    COORDINATOR_PUBKEYS
        .iter()
        .find(|(known, _)| *known == network)
        .map(|(_, pubkey)| *pubkey)
}

/// Fetch this player's records from `relays` and decrypt them, then the contracts of their
/// competitions. Failures of single relays are returned as warnings.
pub async fn load(session: &mut Session, relays: &[String]) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    if !relays.is_empty() {
        let filter = session.player_filter().map_err(|e| e.to_string())?;
        let (events, failed) = relays::fetch(relays, &[filter]).await;
        warnings.extend(failed);
        session.add_events(events);
    }
    session.load().map_err(|e| e.to_string())?;
    if let Some(filter) = session.competition_filter().filter(|_| !relays.is_empty()) {
        let (events, failed) = relays::fetch(relays, &[filter]).await;
        warnings.extend(failed);
        session.add_events(events);
        session.load().map_err(|e| e.to_string())?;
    }
    Ok(warnings)
}

/// Find the attestation of every competition still waiting for one: on relays, then from the
/// oracle's API at `oracle`. Each is checked against the contract before it is kept.
pub async fn find_attestations(
    session: &mut Session,
    relays: &[String],
    oracle: Option<&str>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let wanted = session.attestations_wanted();
    if wanted.is_empty() {
        return warnings;
    }
    if !relays.is_empty() {
        let filters: Vec<Value> = wanted
            .iter()
            .flat_map(|(_, event_id)| attestation::relay_filters(event_id))
            .collect();
        let (events, failed) = relays::fetch(relays, &filters).await;
        warnings.extend(failed);
        session.offer_attestation_events(&events);
    }
    let Some(oracle) = oracle else {
        return warnings;
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    for (competition_id, event_id) in session.attestations_wanted() {
        let url = format!("{}/oracle/events/{event_id}", oracle.trim_end_matches('/'));
        let fetched = async {
            let response = client.get(&url).send().await.map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(response.status().to_string());
            }
            response.json::<Value>().await.map_err(|e| e.to_string())
        };
        match fetched.await {
            Ok(event) => {
                session.offer_attestation(competition_id, &attestation::candidates_in_json(&event));
            }
            Err(e) => warnings.push(format!("{url}: {e}")),
        }
    }
    warnings
}

/// Answer the chain lookups `run` needs, round after round, until it needs nothing more.
pub async fn settle_chain<T>(
    session: &mut Session,
    esplora: &Esplora,
    mut run: impl FnMut(&Session) -> T,
) -> Result<T, String> {
    let (tip, median_time_past) = esplora.tip().await?;
    session.chain.set_tip(tip, median_time_past);
    // Each round follows the money one transaction further; four reach a claimed win.
    for _ in 0..8 {
        let result = run(session);
        let missing = session.chain.missing();
        if missing.is_empty() {
            return Ok(result);
        }
        esplora.fill(&mut session.chain, missing).await?;
    }
    Err("the chain lookups did not settle".into())
}
