//! Reuse public DLC derivations across participants. Authorization is checked on every request.
use coordinator_escrow::payout::{self, ContractCommitment, PayoutError, SigningRequirement};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

pub(crate) type Requirements = Vec<([u8; 32], SigningRequirement)>;
const MAX_CONTRACTS: usize = 4;
const MAX_BYTES: usize = 8 * 1024 * 1024;

struct Entry {
    digest: String,
    requirements: Arc<Requirements>,
    bytes: usize,
}
#[derive(Default)]
struct State {
    entries: VecDeque<Entry>,
    bytes: usize,
}

/// Keyed by every contract parameter and the actual funding outpoint. Contains no private keys,
/// nonces, permits or participant authorization. Failures and oversized results are not kept.
#[derive(Default)]
pub(crate) struct SigningRequirements(Mutex<State>);
impl SigningRequirements {
    pub(crate) fn get(
        &self,
        contract: &ContractCommitment,
    ) -> Result<Arc<Requirements>, PayoutError> {
        let digest = payout::contract_digest(contract)?;
        let mut state = self.0.lock().map_err(|_| {
            PayoutError::ContractMismatch("Signing requirement cache lock poisoned".into())
        })?;
        if let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.digest == digest)
        {
            let entry = state.entries.remove(index).expect("located cache entry");
            let requirements = entry.requirements.clone();
            state.entries.push_back(entry);
            return Ok(requirements);
        }
        // Serialize derivation so concurrent requests cannot duplicate a large rebuild.
        let requirements = Arc::new(payout::signing_requirements(contract)?);
        let bytes = std::mem::size_of::<Entry>()
            + std::mem::size_of::<Requirements>()
            + 2 * std::mem::size_of::<usize>()
            + digest.capacity()
            + requirements.capacity() * std::mem::size_of::<([u8; 32], SigningRequirement)>()
            + requirements
                .iter()
                .map(|(_, item)| item.signers.capacity() * 33)
                .sum::<usize>();
        if bytes <= MAX_BYTES {
            while state.entries.len() >= MAX_CONTRACTS || state.bytes > MAX_BYTES - bytes {
                if let Some(entry) = state.entries.pop_front() {
                    state.bytes -= entry.bytes;
                }
            }
            state.bytes += bytes;
            state.entries.push_back(Entry {
                digest,
                requirements: requirements.clone(),
                bytes,
            });
        }
        Ok(requirements)
    }
}
