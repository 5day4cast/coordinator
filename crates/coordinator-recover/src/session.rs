//! A recovery session: the records found so far, the keys they open, and the chain state.

use std::collections::BTreeMap;

use bitcoin::absolute::LockTime;
use bitcoin::{Amount, FeeRate, Network, ScriptBuf, Transaction};
use dlctix::hashlock::Preimage;
use dlctix::secp::MaybeScalar;
use dlctix::Outcome;
use nostr::{Event, PublicKey};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::attestation;
use crate::chain::ChainView;
use crate::contract::{parse_preimage, EntryContract};
use crate::escrow::Escrow;
use crate::fees::{bump_status, BumpStatus};
use crate::inspect::{self, EntryReport, EscrowState, Location, Stage};
use crate::spec::{self, CompetitionRecord, EntryRecord, Kit, WalletRecord};
use crate::{EntryKey, Error, Identity, Result, WalletSeed};

pub struct Session {
    identity: Identity,
    network: Network,
    coordinator: Option<PublicKey>,
    kits: Vec<Kit>,
    events: Vec<Event>,
    seed: Option<WalletSeed>,
    entries: BTreeMap<Uuid, EntryRecord>,
    competitions: BTreeMap<Uuid, CompetitionRecord>,
    /// By entry. An error says why the entry's contract cannot be used.
    contracts: BTreeMap<Uuid, std::result::Result<EntryContract, String>>,
    /// By competition, each checked against the contract's locking points.
    attestations: BTreeMap<Uuid, MaybeScalar>,
    escrow_states: BTreeMap<Uuid, EscrowState>,
    pub chain: ChainView,
    /// Records that could not be read, and other problems short of failing.
    pub warnings: Vec<String>,
}

/// One transaction of a claim, in broadcast order.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimTx {
    pub label: &'static str,
    #[serde(serialize_with = "serialize_tx")]
    pub tx: Transaction,
    pub bump: BumpStatus,
}

fn serialize_tx<S: serde::Serializer>(
    tx: &Transaction,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&bitcoin::consensus::encode::serialize_hex(tx))
}

/// What a claim broadcasts now, what is already done, and what it waits for.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimPlan {
    pub entry_id: Uuid,
    pub txs: Vec<ClaimTx>,
    /// Steps already on chain.
    pub done: Vec<String>,
    /// Steps in the mempool and not yet confirmed: the outcome, expiry or split transaction a
    /// child can still pay for, if it has an anchor.
    pub unconfirmed: Vec<ClaimTx>,
    /// Why the claim stops where it does, if it is not finished.
    pub waiting: Option<String>,
    /// By when the next step must confirm, if something else can take the money after that.
    pub deadline: Option<Deadline>,
}

/// When a step must have confirmed by, and what happens after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Deadline {
    /// Confirm before this block.
    pub height: Option<u32>,
    /// Confirm before this time, unix seconds.
    pub time: Option<u64>,
    pub reason: String,
}

impl Session {
    pub fn new(identity: Identity, network: Network) -> Self {
        Self {
            identity,
            network,
            coordinator: None,
            kits: Vec::new(),
            events: Vec::new(),
            seed: None,
            entries: BTreeMap::new(),
            competitions: BTreeMap::new(),
            contracts: BTreeMap::new(),
            attestations: BTreeMap::new(),
            escrow_states: BTreeMap::new(),
            chain: ChainView::default(),
            warnings: Vec::new(),
        }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    /// Change the network, before [`Session::load`]: entry keys are derived for it, and records
    /// for another network are refused.
    pub fn set_network(&mut self, network: Network) {
        self.network = network;
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn set_coordinator(&mut self, coordinator: PublicKey) {
        self.coordinator = Some(coordinator);
    }

    pub fn coordinator(&self) -> Result<PublicKey> {
        self.coordinator.ok_or(Error::NoCoordinator)
    }

    /// Add a recovery file. It must be this player's, and names the coordinator if nothing has.
    pub fn add_kit(&mut self, kit: Kit) -> Result<()> {
        let user = spec::parse_pubkey(&kit.user_pubkey, "user pubkey in the recovery file")?;
        if user != self.identity.pubkey() {
            return Err(Error::Invalid(
                "recovery file: it belongs to another account than this nsec".into(),
            ));
        }
        let coordinator = kit.coordinator()?;
        match self.coordinator {
            Some(known) if known != coordinator => {
                return Err(Error::Invalid(
                    "recovery file: it names another coordinator than --coordinator-pubkey".into(),
                ))
            }
            _ => self.coordinator = Some(coordinator),
        }
        if !kit.network.is_empty() && network(&kit.network)? != self.network {
            return Err(Error::Invalid(format!(
                "recovery file: it is for {}, not {}",
                kit.network, self.network
            )));
        }
        self.kits.push(kit);
        Ok(())
    }

    /// Relays named by the recovery files.
    pub fn kit_relays(&self) -> Vec<String> {
        let mut relays: Vec<String> = self
            .kits
            .iter()
            .flat_map(|kit| kit.relays.clone())
            .collect();
        relays.sort();
        relays.dedup();
        relays
    }

    /// The networks this player's wallet and entry records name, under the current coordinator
    /// key, from the relays' events and the recovery files: what the network is when nobody
    /// said. Records that do not decrypt are skipped; [`Session::load`] reports them.
    pub fn recorded_networks(&self) -> Vec<Network> {
        let Ok(coordinator) = self.coordinator() else {
            return Vec::new();
        };
        let blind = self.identity.blind_tag(&coordinator);
        let prefix = spec::entry_d_prefix(&blind);
        let wallet_d = spec::wallet_d(&blind);
        let ciphertexts = spec::newest_by_d(&self.events, &coordinator)
            .into_iter()
            .filter(|(d, event)| {
                spec::tag(event, "b") == Some(blind.as_str())
                    && (*d == wallet_d || d.starts_with(&prefix))
            })
            .map(|(_, event)| event.content)
            .chain(
                self.kits
                    .iter()
                    .flat_map(|kit| kit.wallet.iter().chain(&kit.entries).cloned()),
            );
        let mut networks = Vec::new();
        for ciphertext in ciphertexts {
            let Ok(plaintext) = self
                .identity
                .decrypt(&coordinator, &ciphertext, "player record")
            else {
                continue;
            };
            let named = serde_json::from_str::<Value>(&plaintext)
                .ok()
                .and_then(|record| record.get("network")?.as_str().map(network));
            if let Some(Ok(found)) = named {
                if !networks.contains(&found) {
                    networks.push(found);
                }
            }
        }
        networks
    }

    /// The relay filter for this player's wallet and entry records.
    pub fn player_filter(&self) -> Result<Value> {
        let coordinator = self.coordinator()?;
        Ok(spec::player_filter(
            &coordinator,
            &self.identity.blind_tag(&coordinator),
        ))
    }

    /// The relay filter for the contracts of this player's competitions, once entries are loaded.
    pub fn competition_filter(&self) -> Option<Value> {
        let coordinator = self.coordinator.as_ref()?;
        let ids: Vec<Uuid> = self.competition_ids();
        (!ids.is_empty()).then(|| spec::competition_filter(coordinator, &ids))
    }

    fn competition_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self.entries.values().map(|e| e.competition_id).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn add_events(&mut self, events: impl IntoIterator<Item = Event>) {
        for event in events {
            if !self.events.iter().any(|known| known.id == event.id) {
                self.events.push(event);
            }
        }
    }

    /// Decrypt everything found so far: the wallet seed, entry records, competitions, and each
    /// entry's contract. Run again after adding events; each run starts over.
    pub fn load(&mut self) -> Result<()> {
        let coordinator = self.coordinator()?;
        let blind = self.identity.blind_tag(&coordinator);
        let newest = spec::newest_by_d(&self.events, &coordinator);
        let ours = |d: &String, event: &Event| {
            spec::tag(event, "b") == Some(blind.as_str()) && d.starts_with(&blind)
        };
        self.warnings.clear();

        // The wallet: the relay's newest backup, then the recovery files'.
        let wallets: Vec<String> = newest
            .iter()
            .filter(|(d, event)| ours(d, event) && **d == spec::wallet_d(&blind))
            .map(|(_, event)| event.content.clone())
            .chain(self.kits.iter().filter_map(|kit| kit.wallet.clone()))
            .collect();
        let mut seed = None;
        for ciphertext in &wallets {
            match self.open_wallet(&coordinator, ciphertext) {
                Ok(opened) => {
                    seed = Some(opened);
                    break;
                }
                Err(e) => self.warnings.push(format!("a wallet backup: {e}")),
            }
        }
        self.seed = Some(seed.ok_or(Error::NoWallet)?);

        // Entries: every record, keeping each entry's latest.
        let prefix = spec::entry_d_prefix(&blind);
        let ciphertexts: Vec<String> = newest
            .iter()
            .filter(|(d, event)| ours(d, event) && d.starts_with(&prefix))
            .map(|(_, event)| event.content.clone())
            .chain(self.kits.iter().flat_map(|kit| kit.entries.clone()))
            .collect();
        self.entries.clear();
        for ciphertext in ciphertexts {
            match self.open_entry(&coordinator, &ciphertext) {
                Ok(entry) => match self.entries.get(&entry.entry_id) {
                    Some(known) if known.updated_at >= entry.updated_at => {}
                    _ => {
                        self.entries.insert(entry.entry_id, entry);
                    }
                },
                Err(e) => self.warnings.push(format!("an entry record: {e}")),
            }
        }

        // Competitions: the recovery files', then the relays', which are newer. A competition
        // still waiting for parts is only worth a warning if it is one of this player's.
        self.competitions.clear();
        let from_kits =
            spec::assemble_competitions(self.kits.iter().flat_map(|kit| kit.competitions.clone()));
        let from_relays = spec::competitions_from_events(&newest);
        let wanted = self.competition_ids();
        for (id, competition) in from_kits.into_iter().chain(from_relays) {
            match competition {
                Ok(competition) => {
                    let keep_older_contract = competition.contract().is_none()
                        && self
                            .competitions
                            .get(&id)
                            .is_some_and(|known| known.contract().is_some());
                    if !keep_older_contract {
                        self.competitions.insert(id, competition);
                    }
                }
                Err(e) if wanted.contains(&id) && !self.competitions.contains_key(&id) => {
                    self.warnings.push(format!("competition {id}: {e}"))
                }
                Err(_) => {}
            }
        }
        for id in self.competition_ids() {
            if let Some(attestation) = self
                .competitions
                .get(&id)
                .and_then(|c| c.attestation.clone())
            {
                self.offer_attestation(id, &attestation::candidates_in_text(&attestation));
            }
        }

        self.build_contracts();
        Ok(())
    }

    fn open_wallet(&self, coordinator: &PublicKey, ciphertext: &str) -> Result<WalletSeed> {
        let plaintext = self
            .identity
            .decrypt(coordinator, ciphertext, "wallet record")?;
        let record: WalletRecord =
            serde_json::from_str(&plaintext).map_err(|e| Error::Record(format!("wallet: {e}")))?;
        if network(&record.network)? != self.network {
            return Err(Error::Record(format!(
                "wallet: it is for {}, not {}",
                record.network, self.network
            )));
        }
        let backup = self
            .identity
            .decrypt_own(&record.wallet_blob, "wallet backup")?;
        WalletSeed::from_backup(&backup)
    }

    fn open_entry(&self, coordinator: &PublicKey, ciphertext: &str) -> Result<EntryRecord> {
        let plaintext = self
            .identity
            .decrypt(coordinator, ciphertext, "entry record")?;
        let entry: EntryRecord =
            serde_json::from_str(&plaintext).map_err(|e| Error::Record(format!("entry: {e}")))?;
        let same = |recorded: &str, key: &PublicKey| {
            spec::parse_pubkey(recorded, "pubkey").ok() == Some(*key)
        };
        if !same(&entry.user_pubkey, &self.identity.pubkey())
            || !same(&entry.coordinator_pubkey, coordinator)
        {
            return Err(Error::Record(format!(
                "entry {}: it names another player or coordinator",
                entry.entry_id
            )));
        }
        if network(&entry.network)? != self.network {
            return Err(Error::Record(format!(
                "entry {}: it is for {}, not {}",
                entry.entry_id, entry.network, self.network
            )));
        }
        Ok(entry)
    }

    fn build_contracts(&mut self) {
        let mut contracts = BTreeMap::new();
        for entry in self.entries.values() {
            let Some(competition) = self.competitions.get(&entry.competition_id) else {
                continue;
            };
            if competition.contract().is_none() {
                continue;
            }
            let built = self
                .entry_key(entry)
                .and_then(|key| EntryContract::new(entry, competition, &key))
                .map_err(|e| e.to_string());
            contracts.insert(entry.entry_id, built);
        }
        self.contracts = contracts;
    }

    pub fn entries(&self) -> impl Iterator<Item = &EntryRecord> {
        self.entries.values()
    }

    pub fn entry(&self, entry_id: Uuid) -> Result<&EntryRecord> {
        self.entries
            .get(&entry_id)
            .ok_or(Error::UnknownEntry(entry_id))
    }

    pub fn competition(&self, competition_id: Uuid) -> Option<&CompetitionRecord> {
        self.competitions.get(&competition_id)
    }

    /// The entry's key, refused unless it is the key the record says the entry has.
    pub fn entry_key(&self, entry: &EntryRecord) -> Result<EntryKey> {
        let seed = self.seed.as_ref().ok_or(Error::NoWallet)?;
        let key = seed.entry_key(self.network, entry.entry_id)?;
        if !key.matches(&entry.entry_pubkey) {
            return Err(Error::ForeignEntry(entry.entry_id));
        }
        Ok(key)
    }

    pub fn contract(&self, entry_id: Uuid) -> Option<std::result::Result<&EntryContract, &str>> {
        self.contracts
            .get(&entry_id)
            .map(|built| built.as_ref().map_err(String::as_str))
    }

    pub fn escrow(&self, entry_id: Uuid) -> Result<Option<Escrow>> {
        let entry = self.entry(entry_id)?;
        let key = self.entry_key(entry)?;
        Escrow::new(entry, &key)
    }

    pub fn set_escrow_state(&mut self, entry_id: Uuid, state: EscrowState) {
        self.escrow_states.insert(entry_id, state);
    }

    /// Competitions with a contract and no attestation yet, with their oracle event ids.
    pub fn attestations_wanted(&self) -> Vec<(Uuid, String)> {
        self.competition_ids()
            .into_iter()
            .filter(|id| !self.attestations.contains_key(id))
            .filter_map(|id| self.competitions.get(&id))
            .filter(|competition| competition.contract().is_some())
            .map(|competition| (competition.competition_id, competition.oracle_event_id()))
            .collect()
    }

    /// Keep the first of `candidates` that opens one of `competition_id`'s locking points.
    pub fn offer_attestation(&mut self, competition_id: Uuid, candidates: &[MaybeScalar]) -> bool {
        let Some((params, _)) = self
            .competitions
            .get(&competition_id)
            .and_then(CompetitionRecord::contract)
        else {
            return false;
        };
        match attestation::select(candidates, &params.event.locking_points) {
            Some(found) => {
                self.attestations.insert(competition_id, found);
                true
            }
            None => false,
        }
    }

    /// Try every value in relay events against every competition still waiting for one.
    pub fn offer_attestation_events(&mut self, events: &[Event]) {
        let candidates: Vec<MaybeScalar> = events
            .iter()
            .filter(|event| event.verify().is_ok())
            .flat_map(|event| attestation::candidates_in_text(&event.content))
            .collect();
        for (id, _) in self.attestations_wanted() {
            self.offer_attestation(id, &candidates);
        }
    }

    pub fn attestation(&self, competition_id: Uuid) -> Option<MaybeScalar> {
        self.attestations.get(&competition_id).copied()
    }

    /// Every entry's state. Lookups the chain view could not answer are in `chain.missing()`.
    pub fn inspect(&self, now: u64) -> Vec<EntryReport> {
        self.entries
            .values()
            .map(|entry| self.inspect_entry(entry, now))
            .collect()
    }

    fn inspect_entry(&self, entry: &EntryRecord, now: u64) -> EntryReport {
        let mut report = inspect::blank_report(entry);
        match self.contract(entry.entry_id) {
            Some(Ok(contract)) => {
                inspect::contract_report(
                    &mut report,
                    contract,
                    self.attestation(entry.competition_id),
                    self.ticket_preimage(entry, None).is_ok(),
                    &self.chain,
                    now,
                );
                return report;
            }
            Some(Err(reason)) => {
                report.location = Location::Unknown {
                    reason: format!("the contract cannot be used: {reason}"),
                };
                return report;
            }
            None => {}
        }
        match self.escrow(entry.entry_id) {
            Ok(Some(escrow)) => inspect::escrow_report(
                &mut report,
                &escrow,
                self.escrow_states.get(&entry.entry_id),
                now,
            ),
            Ok(None) => {
                if entry.escrow.is_some() {
                    report
                        .notes
                        .push("the entry's escrow was issued but never funded".into());
                }
                if entry.contract.is_some() {
                    report.location = Location::Unknown {
                        reason: "the entry is in a contract whose competition record was not found"
                            .into(),
                    };
                }
            }
            Err(e) => {
                report.location = Location::Unknown {
                    reason: e.to_string(),
                }
            }
        }
        report
    }

    fn ticket_preimage(&self, entry: &EntryRecord, given: Option<&str>) -> Result<Preimage> {
        let recorded = entry.ticket.as_ref().and_then(|t| t.preimage.as_deref());
        match given.or(recorded) {
            Some(preimage) => parse_preimage(preimage),
            None => Err(Error::NotYet(
                "the split transaction needs this player's ticket preimage, which the records do \
                 not hold: pass it with --ticket-preimage"
                    .into(),
            )),
        }
    }

    /// The transactions that move `entry_id`'s contract money forward now, skipping what is
    /// already on chain. `destination` and `fee_rate` are for the final claim to the player.
    ///
    /// A step is planned only once nodes take it: the split spends the outcome output with a
    /// relative locktime of `delta` blocks, and the win the split output with the same, so each
    /// waits until its parent has `delta` confirmations, and `waiting` says from which block.
    pub fn claim(
        &self,
        entry_id: Uuid,
        destination: Option<ScriptBuf>,
        fee_rate: FeeRate,
        ticket_preimage: Option<&str>,
    ) -> Result<ClaimPlan> {
        let entry = self.entry(entry_id)?;
        let contract = match self.contract(entry_id) {
            Some(Ok(contract)) => contract,
            Some(Err(reason)) => {
                return Err(Error::Contract {
                    entry: entry_id,
                    reason: reason.to_owned(),
                })
            }
            None => {
                return Err(Error::NotYet(format!(
                    "entry {entry_id} has no contract on record; if it has an escrow, use refund-escrow"
                )))
            }
        };
        let mut plan = ClaimPlan {
            entry_id,
            txs: Vec::new(),
            done: Vec::new(),
            unconfirmed: Vec::new(),
            waiting: None,
            deadline: None,
        };
        let delta = contract.delta();
        let reclaim = u32::from(delta) * 2;
        let preimage = self.ticket_preimage(entry, ticket_preimage);
        let presigned = |label: &'static str, tx: Transaction| ClaimTx {
            label,
            bump: bump_status(&tx, contract.signed().presigned_tx_fee(&tx)),
            tx,
        };
        // When the split opens, given the outcome (or expiry) transaction's height once it has
        // one.
        let split_opens = |label: &str, outcome_height: Option<u32>| {
            let opens = match outcome_height {
                Some(height) => format!(
                    "the split opens at {}",
                    self.chain.describe_height(height + u32::from(delta))
                ),
                None => {
                    format!("the split opens {delta} blocks after the {label} transaction confirms")
                }
            };
            match &preimage {
                Ok(_) => format!("{opens}; run claim again then"),
                Err(e) => format!("{opens}; {e}"),
            }
        };
        // Until it confirms, an attested outcome transaction competes with the expiry
        // transaction, which anyone can broadcast once the event expires.
        let expiry_deadline = |outcome: Outcome| -> Option<Deadline> {
            if outcome == Outcome::Expiry {
                return None;
            }
            let reason = "the expiry transaction, which pays every player an equal share, becomes \
                          valid then and can take the outcome transaction's place"
                .to_owned();
            match contract.expiry()? {
                LockTime::Blocks(height) => Some(Deadline {
                    height: Some(height.to_consensus_u32()),
                    time: None,
                    reason,
                }),
                LockTime::Seconds(time) => Some(Deadline {
                    height: None,
                    time: Some(u64::from(time.to_consensus_u32())),
                    reason,
                }),
            }
        };
        let claim_after_split =
            || format!("the claim to your address opens {delta} blocks after the split confirms");
        let reclaim_deadline = |what: &str, height: u32| Deadline {
            height: Some(height + reclaim),
            time: None,
            reason: format!(
                "the market maker can reclaim {what} from block {}, 2·delta after it confirmed",
                height + reclaim
            ),
        };
        match inspect::stage(contract, &self.chain) {
            Stage::Pending => {
                return Err(Error::NotYet(
                    "the chain state is incomplete; fetch what chain.missing() lists".into(),
                ))
            }
            Stage::FundingNotOnChain => {
                plan.waiting = Some("the funding transaction is not on chain".into())
            }
            Stage::FundingUnspent { .. } => {
                let attested = self
                    .attestation(entry.competition_id)
                    .and_then(|a| contract.attested_outcome(a).map(|outcome| (a, outcome)));
                let (outcome, tx, label) = match (attested, contract.expiry()) {
                    (Some((attestation, outcome)), _) => {
                        (outcome, contract.outcome_tx(attestation)?, "outcome")
                    }
                    (None, Some(expiry)) if inspect::expired(expiry, &self.chain) => {
                        (Outcome::Expiry, contract.expiry_tx()?, "expiry")
                    }
                    _ => {
                        plan.waiting =
                            Some("no attestation yet, and the event has not expired".into());
                        return Ok(plan);
                    }
                };
                if contract.win_condition(outcome).is_none() {
                    plan.waiting = Some(format!("the {outcome} outcome pays this player nothing"));
                    return Ok(plan);
                }
                plan.txs.push(presigned(label, tx));
                plan.deadline = expiry_deadline(outcome);
                plan.waiting = Some(split_opens(label, None));
            }
            Stage::FundingSpentElsewhere { by } => {
                plan.waiting = Some(format!(
                    "the funding output was spent by {by}, which is not this contract's"
                ))
            }
            Stage::NotPaid { outcome } => {
                plan.waiting = Some(format!("the {outcome} outcome pays this player nothing"))
            }
            Stage::OutcomeUnspent {
                outcome,
                txid,
                height,
            } => {
                let label = if outcome == Outcome::Expiry {
                    "expiry"
                } else {
                    "outcome"
                };
                match height {
                    Some(height) => {
                        plan.done.push(format!(
                            "{label} transaction {txid}, confirmed in block {height}"
                        ));
                        plan.deadline = Some(reclaim_deadline("the outcome output", height));
                    }
                    None => {
                        plan.done
                            .push(format!("{label} transaction {txid}, not confirmed yet"));
                        // Rebuilt from the contract, so a child can still pay for it.
                        let signed = match outcome {
                            Outcome::Expiry => contract.expiry_tx().ok(),
                            Outcome::Attestation(_) => self
                                .attestation(entry.competition_id)
                                .and_then(|attestation| contract.outcome_tx(attestation).ok()),
                        };
                        if let Some(tx) = signed.filter(|tx| tx.compute_txid() == txid) {
                            plan.unconfirmed.push(presigned(label, tx));
                        }
                        plan.deadline = expiry_deadline(outcome);
                    }
                }
                match (height, &preimage) {
                    (Some(height), Ok(preimage)) if self.chain.matured(height, delta) => {
                        plan.txs
                            .push(presigned("split", contract.split_tx(outcome, *preimage)?));
                        plan.waiting = Some(claim_after_split());
                    }
                    (Some(height), Err(e)) if self.chain.matured(height, delta) => {
                        plan.waiting = Some(e.to_string())
                    }
                    _ => plan.waiting = Some(split_opens(label, height)),
                }
            }
            Stage::OutcomeSpentElsewhere { txid, by } => {
                plan.done.push(format!("outcome transaction {txid}"));
                plan.waiting = Some(format!(
                    "the outcome output was spent by {by}, not the split: the market maker reclaimed it"
                ));
            }
            Stage::SplitUnspent {
                outcome, height, ..
            } => {
                plan.done.push("outcome and split transactions".into());
                match height {
                    Some(height) => {
                        plan.deadline = Some(reclaim_deadline("this player's split output", height))
                    }
                    // In the mempool: rebuilt with this player's preimage, so a child can still
                    // pay for it (any winner's preimage gives the same txid).
                    None => {
                        if let Ok(preimage) = &preimage {
                            if let Ok(tx) = contract.split_tx(outcome, *preimage) {
                                plan.unconfirmed.push(presigned("split", tx));
                            }
                        }
                    }
                }
                match height {
                    Some(height) if self.chain.matured(height, delta) => {
                        let destination = destination.ok_or_else(|| {
                            Error::Invalid("destination: pass --to <address>".into())
                        })?;
                        let preimage = preimage
                            .as_ref()
                            .map_err(|e| Error::NotYet(e.to_string()))?;
                        let key = self.entry_key(entry)?;
                        let tx =
                            contract.win_tx(outcome, *preimage, &key, destination, fee_rate)?;
                        let fee = contract
                            .win_output(outcome)
                            .and_then(|(_, value)| fee_paid(&tx, &[value]));
                        plan.txs.push(ClaimTx {
                            label: "win",
                            bump: bump_status(&tx, fee),
                            tx,
                        });
                    }
                    Some(height) => {
                        plan.waiting = Some(format!(
                            "the claim to your address opens at {}",
                            self.chain.describe_height(height + u32::from(delta))
                        ))
                    }
                    None => plan.waiting = Some(claim_after_split()),
                }
            }
            Stage::SplitSpent { outpoint, by, .. } => {
                plan.done.push(format!("{outpoint} was spent by {by}"));
            }
        }
        Ok(plan)
    }
}

/// What `tx` pays in fees, given its inputs' values in order.
fn fee_paid(tx: &Transaction, inputs: &[Amount]) -> Option<Amount> {
    let paid: Amount = tx.output.iter().map(|output| output.value).sum();
    inputs.iter().copied().sum::<Amount>().checked_sub(paid)
}

pub fn network(name: &str) -> Result<Network> {
    match name.trim().to_ascii_lowercase().as_str() {
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "signet" | "mutinynet" => Ok(Network::Signet),
        "testnet" | "testnet3" => Ok(Network::Testnet),
        "testnet4" => Ok(Network::Testnet4),
        "regtest" => Ok(Network::Regtest),
        other => Err(Error::Invalid(format!("network {other}"))),
    }
}
