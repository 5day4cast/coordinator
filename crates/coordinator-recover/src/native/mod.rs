//! The CLI's I/O: Nostr relays, Esplora, the oracle's API and Arkade.

pub mod ark;
pub mod esplora;
pub mod relays;
mod ws;

use std::time::Duration;

use bitcoin::Network;
use nostr::PublicKey;
use serde_json::Value;

use crate::attestation;
use crate::Session;
use esplora::Esplora;

/// Find who published this player's records: try each candidate coordinator recovery key, with
/// the network it is for when that is known, until one has a wallet or entry record of theirs on
/// `relays` or in the recovery file. The session keeps that key and the network: the
/// candidate's, else the one the records name. With no records anywhere it keeps the first
/// candidate, and [`load`] says no wallet backup was found. Failures of single relays are
/// returned as warnings.
pub async fn find_player(
    session: &mut Session,
    relays: &[String],
    candidates: &[(PublicKey, Option<Network>)],
) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    let mut fetched: Vec<PublicKey> = Vec::new();
    for (coordinator, network) in candidates {
        session.set_coordinator(*coordinator);
        if !relays.is_empty() && !fetched.contains(coordinator) {
            let filter = session.player_filter().map_err(|e| e.to_string())?;
            let (events, failed) = relays::fetch(relays, &[filter]).await;
            warnings.extend(failed);
            session.add_events(events);
            fetched.push(*coordinator);
        }
        let recorded = session.recorded_networks();
        match (network, recorded.as_slice()) {
            (Some(network), recorded) if recorded.contains(network) => {
                session.set_network(*network);
                return Ok(warnings);
            }
            (None, [network]) => {
                session.set_network(*network);
                return Ok(warnings);
            }
            (None, [_, _, ..]) => {
                let names: Vec<String> = recorded.iter().map(ToString::to_string).collect();
                return Err(format!(
                    "this nsec has records on {}: pass --network to choose one",
                    names.join(" and ")
                ));
            }
            _ => {}
        }
    }
    if let Some((coordinator, network)) = candidates.first() {
        session.set_coordinator(*coordinator);
        if let Some(network) = network {
            session.set_network(*network);
        }
    }
    Ok(warnings)
}

/// Decrypt this player's records, found with [`find_player`], then fetch and read the contracts
/// of their competitions. Failures of single relays are returned as warnings.
pub async fn load(session: &mut Session, relays: &[String]) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
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
    // Only for saying about when a block comes: a failure leaves the estimates out.
    if session.chain.block_interval.is_none() {
        if let Ok(seconds) = esplora.block_interval(tip, median_time_past, 144).await {
            session.chain.set_block_interval(seconds);
        }
    }
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
