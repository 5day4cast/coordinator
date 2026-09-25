use super::{
    keymeld_trust::trusted_assignment,
    keys::{EntryKey, WalletSeed},
    EncryptedWalletBackup, EntryRegistration, PayoutRelease, WalletError,
};
use crate::nostr::{CustomSigner, NostrClientCore};
use ::nostr::NostrSigner;
use coordinator_core::{
    keymeld::{
        ark, payout, payout_protocol, prepare_payout_registration, prepare_registration,
        PayoutPolicy, PreparedRegistration,
    },
    RegistrationAssignment,
};
use dlctix::{
    bitcoin::{
        ecdsa,
        hashes::Hash,
        script::Instruction,
        secp256k1::{All, Message, Secp256k1},
        sighash::{EcdsaSighashType, SighashCache},
        Network, OutPoint, Psbt, PublicKey, ScriptBuf, TxOut,
    },
    musig2::{AggNonce, PartialSignature, PubNonce},
    secp::{MaybeScalar, Point},
    ContractParameters, EventLockingConditions, NonceSharingRound, Outcome, SigMap, SigningSession,
    TicketedDLC,
};
use lightning_invoice::Bolt11Invoice;
use rand_chacha::{rand_core::SeedableRng, ChaCha20Rng};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::str::FromStr;
use uuid::Uuid;
use zeroize::Zeroizing;

pub struct DlcWalletCore {
    seed: WalletSeed,
    network: Network,
    nostr_client: NostrClientCore,
    secp: Secp256k1<All>,
    contracts: HashMap<Uuid, EntryContract>,
    /// Remember every signed nonce context, even if an entry's contract is replaced.
    signed_aggregates: HashMap<(Uuid, OutPoint, [u8; 32]), [u8; 32]>,
}

/// A contract the user has checked and agreed to sign for one entry.
struct EntryContract {
    dlc: TicketedDLC,
    /// Digest of the accepted `ContractParameters`; part of the nonce seed.
    params_digest: [u8; 32],
}

/// Explicit choices collected before the ticket payment QR is displayed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutConsent {
    pub competition_id: Uuid,
    pub lightning_address: Option<String>,
    pub allow_invoice_fallback: bool,
    pub release_entry_key_after_payment: bool,
    pub ticket_invoice: String,
    pub ticket_amount_sats: u64,
    pub expected_funding_sats: u64,
    pub expected_player_count: usize,
    pub expected_winner_count: usize,
    pub expected_relative_locktime_delta: u16,
    pub max_fee_rate_sat_vb: u64,
    pub oracle_announcement: EventLockingConditions,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutInvoiceConsent {
    pub entry_id: Uuid,
    pub competition_id: Uuid,
    pub expected_pubkey: String,
    pub invoice: String,
    pub context: payout_protocol::InvoiceAuthorizationContext,
    pub contract: payout::ContractCommitment,
    pub signatures: dlctix::ContractSignatures,
    pub attestation: String,
}

impl DlcWalletCore {
    pub fn create(nostr_client: &NostrClientCore, network: Network) -> Self {
        Self::with_seed(nostr_client, WalletSeed::generate(), network)
    }

    /// Restore a wallet from the backup produced by [`Self::encrypted_backup`].
    pub async fn load(
        nostr_client: &NostrClientCore,
        encrypted_backup: &str,
        network: Network,
    ) -> Result<Self, WalletError> {
        let signer = signer(nostr_client)?;
        let own_pubkey = signer.get_public_key().await.map_err(signer_error)?;
        let backup = Zeroizing::new(
            signer
                .nip44_decrypt(&own_pubkey, encrypted_backup)
                .await
                .map_err(signer_error)?,
        );
        let seed = WalletSeed::from_backup(&backup)?;
        Ok(Self::with_seed(nostr_client, seed, network))
    }

    fn with_seed(nostr_client: &NostrClientCore, seed: WalletSeed, network: Network) -> Self {
        Self {
            seed,
            network,
            nostr_client: nostr_client.clone(),
            secp: Secp256k1::new(),
            contracts: HashMap::new(),
            signed_aggregates: HashMap::new(),
        }
    }

    fn entry_key(&self, entry_id: Uuid) -> Result<EntryKey, WalletError> {
        self.seed.entry_key(&self.secp, self.network, entry_id)
    }

    /// The seed backup, NIP-44 encrypted to the user's own Nostr key. There
    /// is deliberately no way to choose the recipient: encrypting to a
    /// caller-supplied pubkey would hand that key's owner the seed.
    pub async fn encrypted_backup(&self) -> Result<EncryptedWalletBackup, WalletError> {
        let signer = signer(&self.nostr_client)?;
        let own_pubkey = signer.get_public_key().await.map_err(signer_error)?;
        let encrypted = signer
            .nip44_encrypt(&own_pubkey, &self.seed.to_backup())
            .await
            .map_err(signer_error)?;
        Ok(EncryptedWalletBackup {
            encrypted_bitcoin_private_key: encrypted,
            network: self.network.to_string(),
        })
    }

    pub fn entry_registration(&self, entry_id: Uuid) -> Result<EntryRegistration, WalletError> {
        let key = self.entry_key(entry_id)?;
        Ok(EntryRegistration {
            ephemeral_pubkey: key.point().to_string(),
            payout_hash: hex::encode(key.payout_hash()),
        })
    }

    /// Encrypt the entry key to the keymeld enclave assigned to this ticket.
    ///
    /// `prepare_registration` verifies a fresh Nitro attestation of the enclave
    /// key before encrypting, and binds the envelope to the session, manifest,
    /// slot and enclave epoch. The attestation policy comes from this build's
    /// pinned measurements, not from the coordinator (see `keymeld_trust`).
    pub async fn keymeld_registration(
        &self,
        entry_id: Uuid,
        assignment: &RegistrationAssignment,
    ) -> Result<PreparedRegistration, WalletError> {
        let assignment = trusted_assignment(assignment, self.network)?;
        let key = self.entry_key(entry_id)?;
        prepare_registration(&key.secret_bytes(), &assignment)
            .await
            .map_err(|e| WalletError::Keymeld(e.to_string()))
    }

    fn validate_payout_registration(
        &self,
        entry_id: Uuid,
        assignment: &RegistrationAssignment,
        consent: &PayoutConsent,
    ) -> Result<(), WalletError> {
        let reject = || {
            WalletError::Keymeld("Payout policy differs from the approved entry and ticket".into())
        };
        let policy: PayoutPolicy =
            serde_json::from_str(assignment.payout_policy.as_deref().ok_or_else(reject)?)
                .map_err(|_| reject())?;
        let terms = payout::ContractAuthorization::from_policy(&policy).map_err(|_| reject())?;
        let key = self.entry_key(entry_id)?;
        if consent.ticket_invoice.len() > 16 * 1024 {
            return Err(reject());
        }
        let invoice = Bolt11Invoice::from_str(&consent.ticket_invoice).map_err(|_| reject())?;
        let expected_payouts =
            expected_payouts(consent.expected_player_count, consent.expected_winner_count)?;
        let max_fee = dlctix::bitcoin::FeeRate::from_sat_per_vb(consent.max_fee_rate_sat_vb)
            .ok_or_else(reject)?;
        if terms.competition_id != consent.competition_id
            || terms.entry_id != entry_id
            || terms.network != self.network
            || terms.payout_hash != key.payout_hash()
            || terms.ticket_hash != invoice.payment_hash().to_byte_array()
            || invoice.network() != self.network
            || invoice.would_expire(std::time::Duration::from_secs(
                ::nostr::Timestamp::now().as_secs(),
            ))
            || terms.funding_value.to_sat() != consent.expected_funding_sats
            || terms.player_count != consent.expected_player_count
            || terms.outcome_payouts != expected_payouts
            || terms.event != consent.oracle_announcement
            || terms.event.locking_points.len() + 1 != expected_payouts.len()
            || terms.relative_locktime_block_delta != consent.expected_relative_locktime_delta
            || max_fee == dlctix::bitcoin::FeeRate::ZERO
            || terms.max_fee_rate > max_fee
            || consent.ticket_amount_sats == 0
            || invoice.amount_milli_satoshis() != consent.ticket_amount_sats.checked_mul(1000)
            || !consent.release_entry_key_after_payment
            || policy.release_entry_key_after_payment != consent.release_entry_key_after_payment
            || policy.automatic_lightning_address != consent.lightning_address
            || policy.allow_invoice_fallback != consent.allow_invoice_fallback
        {
            return Err(reject());
        }
        // Entering is the consent to an Arkade escrow: it must hold the entry key, fund only this
        // pool's market maker, refund by the contract's expiry, and charge at most the ticket's markup.
        if let Some(ark_escrow) = &policy.ark_escrow {
            let xonly = |point: Point| {
                ark::XOnlyPublicKey::from_slice(&point.serialize_xonly()).map_err(|_| reject())
            };
            ark::consented_escrow(
                ark_escrow,
                xonly(key.point())?,
                xonly(terms.market_maker.pubkey)?,
                terms.event.expiry,
            )
            .map_err(|_| reject())?;
            // A refund costs no more than entering did, and never the whole buy-in.
            if ark_escrow.max_refund_fee_sats > ark_escrow.max_fee_sats
                || ark_escrow.max_refund_fee_sats >= consent.ticket_amount_sats
            {
                return Err(reject());
            }
            let players = consent.expected_player_count as u64;
            let fees = ark_escrow
                .max_fee_sats
                .checked_mul(players)
                .ok_or_else(reject)?;
            let paid = consent
                .ticket_amount_sats
                .checked_mul(players)
                .ok_or_else(reject)?;
            if fees.saturating_add(consent.expected_funding_sats) > paid {
                return Err(reject());
            }
        }
        Ok(())
    }

    /// Verify entry-bound consent before any enclave lookup, then seal both
    /// secrets with the policy. The wallet v1 preimage derivation is unchanged.
    pub async fn keymeld_payout_registration(
        &self,
        entry_id: Uuid,
        assignment: &RegistrationAssignment,
        consent: &PayoutConsent,
    ) -> Result<PreparedRegistration, WalletError> {
        self.validate_payout_registration(entry_id, assignment, consent)?;
        let assignment = trusted_assignment(assignment, self.network)?;
        let key = self.entry_key(entry_id)?;
        let mut preimage = Zeroizing::new([0u8; 32]);
        hex::decode_to_slice(&*key.payout_preimage_hex(), &mut preimage[..])
            .map_err(|_| WalletError::DerivationFailed)?;
        prepare_payout_registration(&key.secret_bytes(), &preimage, &assignment)
            .await
            .map_err(|e| WalletError::Keymeld(e.to_string()))
    }

    /// Check that `invoice` pays exactly `amount_sats` on this wallet's network
    /// and has not expired, before anything is released or paid for it.
    pub fn validate_invoice(&self, invoice: &str, amount_sats: u64) -> Result<(), WalletError> {
        payout::validate_invoice(
            invoice,
            amount_sats,
            self.network,
            ::nostr::Timestamp::now().as_secs(),
        )
        .map(|_| ())
        .map_err(|e| WalletError::Invoice(e.to_string()))
    }

    /// The QR code of `invoice` as a `data:` URL, drawn only once the invoice
    /// passes [`Self::validate_invoice`]: what a phone scans is what was checked.
    pub fn invoice_qr(&self, invoice: &str, amount_sats: u64) -> Result<String, WalletError> {
        self.validate_invoice(invoice, amount_sats)?;
        super::qr::lightning_invoice_data_url(invoice)
    }

    /// Authorize one ordinary invoice; entry secrets remain inside WASM.
    /// Reconstruct and verify the completed contract and attested payout first.
    pub fn authorize_payout_invoice(
        &self,
        consent: PayoutInvoiceConsent,
    ) -> Result<payout_protocol::SignedInvoiceAuthorization, WalletError> {
        let reject = |e: String| WalletError::Keymeld(e);
        let key = self.entry_key(consent.entry_id)?;
        if consent.expected_pubkey.parse::<Point>().ok() != Some(key.point()) {
            return Err(WalletError::ForeignEntry(consent.entry_id));
        }
        let player = consent
            .contract
            .contract_parameters
            .players
            .iter()
            .find(|player| player.pubkey == key.point())
            .ok_or_else(|| reject("Entry is not in the payout contract".into()))?;
        if player.payout_hash != key.payout_hash() {
            return Err(reject("Payout contract substituted the entry hash".into()));
        }
        payout::verify_completed_contract(&consent.contract, &consent.signatures)
            .map_err(|e| reject(e.to_string()))?;
        let mut attestation = [0u8; 32];
        hex::decode_to_slice(&consent.attestation, &mut attestation)
            .map_err(|_| reject("Invalid payout attestation".into()))?;
        let outcome = payout::attested_outcome(&consent.contract.contract_parameters, &attestation)
            .map_err(|e| reject(e.to_string()))?;
        let owed = payout::owed_sats(
            &consent.contract.contract_parameters,
            &outcome,
            &key.point().serialize(),
        )
        .map_err(|e| reject(e.to_string()))?;
        let now = ::nostr::Timestamp::now().as_secs();
        if consent.context.entry_id != consent.entry_id
            || consent.context.competition_id != consent.competition_id
            || consent.context.invoice_digest != payout::invoice_digest(&consent.invoice)
            || consent.context.contract_digest
                != payout::contract_digest(&consent.contract).map_err(|e| reject(e.to_string()))?
            || Some(consent.context.amount_msat) != owed.checked_mul(1000)
            || consent.context.expires_at <= now
            || consent.context.expires_at > now.saturating_add(600)
        {
            return Err(reject(
                "Invoice authorization differs from the selected entry, contract or payout".into(),
            ));
        }
        payout::validate_invoice(&consent.invoice, owed, self.network, now)
            .map_err(|e| WalletError::Invoice(e.to_string()))?;
        payout_protocol::SignedInvoiceAuthorization::sign(&key.secret_bytes(), consent.context)
            .map_err(|e| reject(e.to_string()))
    }

    /// Release an entry's key and payout preimage for an off-chain payout.
    ///
    /// Refuses unless this wallet derives `expected_pubkey` for `entry_id`, so
    /// a wrong or server-substituted entry cannot extract any other key.
    pub fn payout_release(
        &self,
        entry_id: Uuid,
        expected_pubkey: &str,
    ) -> Result<PayoutRelease, WalletError> {
        let key = self.entry_key(entry_id)?;
        if expected_pubkey.parse::<Point>().ok() != Some(key.point()) {
            return Err(WalletError::ForeignEntry(entry_id));
        }
        Ok(PayoutRelease {
            ephemeral_private_key: key.secret_hex().to_string(),
            payout_preimage: key.payout_preimage_hex().to_string(),
        })
    }

    /// Accept contract parameters for an entry after checking that they
    /// include this entry's key with the payout hash the wallet generated.
    pub fn add_contract(
        &mut self,
        entry_id: Uuid,
        params: ContractParameters,
        funding_outpoint: OutPoint,
    ) -> Result<(), WalletError> {
        let key = self.entry_key(entry_id)?;
        let our_point = key.point();
        let player = params
            .players
            .iter()
            .find(|player| player.pubkey == our_point)
            .ok_or_else(|| WalletError::Contract("our entry key is not a player".into()))?;
        if player.payout_hash != key.payout_hash() {
            return Err(WalletError::Contract(
                "player payout hash does not match this entry".into(),
            ));
        }

        let params_digest = json_digest(&params)?;
        let dlc = TicketedDLC::new(params, funding_outpoint)
            .map_err(|e| WalletError::Contract(e.to_string()))?;
        self.contracts
            .insert(entry_id, EntryContract { dlc, params_digest });
        Ok(())
    }

    /// Rebuild the MuSig2 session for an entry.
    ///
    /// The MuSig rounds are separated by waiting for every other player, and
    /// the page may reload in between, so secret nonces are not stored: they
    /// are re-derived from a seed and must reproduce the public nonces sent in
    /// round one. The seed binds the entry key, the funding outpoint and the
    /// full contract parameters, so a different contract never reuses a nonce.
    fn signing_session(
        &self,
        entry_id: Uuid,
        contract: &EntryContract,
    ) -> Result<SigningSession<NonceSharingRound>, WalletError> {
        let key = self.entry_key(entry_id)?;
        let outpoint = contract.dlc.funding_outpoint();
        let tag = Sha256::digest(b"coordinator/musig-nonce/v1");
        let seed = Zeroizing::new(<[u8; 32]>::from(
            Sha256::new()
                .chain_update(tag)
                .chain_update(tag)
                .chain_update(&key.secret_bytes()[..])
                .chain_update(outpoint.txid.to_byte_array())
                .chain_update(outpoint.vout.to_le_bytes())
                .chain_update(contract.params_digest)
                .finalize(),
        ));
        let mut rng = ChaCha20Rng::from_seed(*seed);
        SigningSession::<NonceSharingRound>::new(contract.dlc.clone(), &mut rng, key.scalar())
            .map_err(|e| WalletError::Signing(e.to_string()))
    }

    /// Round one: this entry's public nonces. Repeatable; always the same
    /// nonces for the same accepted contract.
    pub fn generate_public_nonces(&self, entry_id: Uuid) -> Result<SigMap<PubNonce>, WalletError> {
        let contract = self
            .contracts
            .get(&entry_id)
            .ok_or(WalletError::NoContract(entry_id))?;
        Ok(self
            .signing_session(entry_id, contract)?
            .our_public_nonces()
            .clone())
    }

    /// Round two: partial signatures under the coordinator's aggregate nonces.
    ///
    /// The nonces are deterministic, so signing a *different* aggregate with
    /// them would give the coordinator a second equation in the entry key
    /// (three recover it). Re-signing the same aggregate is harmless: it
    /// reproduces the same signatures. This guard only lasts for the page
    /// session; a caller that wires this flow up must also persist the signed
    /// digest per entry.
    pub fn sign_aggregate_nonces(
        &mut self,
        aggregate_nonces: SigMap<AggNonce>,
        entry_id: Uuid,
    ) -> Result<SigMap<PartialSignature>, WalletError> {
        let digest = json_digest(&aggregate_nonces)?;
        let contract = self
            .contracts
            .get(&entry_id)
            .ok_or(WalletError::NoContract(entry_id))?;
        let nonce_context = (
            entry_id,
            contract.dlc.funding_outpoint(),
            contract.params_digest,
        );
        if self
            .signed_aggregates
            .get(&nonce_context)
            .is_some_and(|signed| *signed != digest)
        {
            return Err(WalletError::ConflictingAggregateNonces(entry_id));
        }

        let signed = self
            .signing_session(entry_id, contract)?
            .compute_partial_signatures(aggregate_nonces)
            .map_err(|e| WalletError::Signing(e.to_string()))?;
        self.signed_aggregates.insert(nonce_context, digest);
        Ok(signed.our_partial_signatures().clone())
    }

    /// Sign this entry's escrow input(s) in the contract funding transaction.
    ///
    /// The PSBT comes from the coordinator and is untrusted. Before signing:
    /// - its transaction must be the one whose output the accepted contract
    ///   spends (same txid, and the funding output at the contract's vout);
    /// - each signed input's witness script must hash to its P2WSH prevout and
    ///   contain this entry's key as a push;
    /// - the sighash type must be `ALL`, so the transaction cannot be changed
    ///   after signing.
    pub fn sign_funding_psbt(&self, mut psbt: Psbt, entry_id: Uuid) -> Result<Psbt, WalletError> {
        let contract = self
            .contracts
            .get(&entry_id)
            .ok_or(WalletError::NoContract(entry_id))?;
        check_funds_contract(
            &psbt,
            contract.dlc.funding_outpoint(),
            &contract.dlc.funding_output(),
        )?;

        let key = self.entry_key(entry_id)?;
        let our_pubkey = PublicKey::new(key.pubkey);
        let tx = psbt.unsigned_tx.clone();
        let mut sighashes = SighashCache::new(&tx);
        let mut signed = 0usize;

        for (index, input) in psbt.inputs.iter_mut().enumerate() {
            let Some(witness_script) = &input.witness_script else {
                continue;
            };
            if !script_pushes_key(witness_script, &our_pubkey) {
                continue;
            }
            let prevout = input.witness_utxo.as_ref().ok_or_else(|| {
                reject(format!("escrow input {index} is missing its witness UTXO"))
            })?;
            if prevout.script_pubkey != ScriptBuf::new_p2wsh(&witness_script.wscript_hash()) {
                return Err(reject(format!(
                    "escrow input {index} witness script does not match its prevout"
                )));
            }
            if let Some(requested) = input.sighash_type {
                if requested.ecdsa_hash_ty() != Ok(EcdsaSighashType::All) {
                    return Err(reject(format!(
                        "escrow input {index} requests sighash {requested}, only ALL is allowed"
                    )));
                }
            }

            let sighash = sighashes
                .p2wsh_signature_hash(index, witness_script, prevout.value, EcdsaSighashType::All)
                .map_err(|e| reject(format!("sighash for input {index}: {e}")))?;
            let signature =
                key.sign_ecdsa(&self.secp, &Message::from_digest(sighash.to_byte_array()));
            input.partial_sigs.insert(
                our_pubkey,
                ecdsa::Signature {
                    signature,
                    sighash_type: EcdsaSighashType::All,
                },
            );
            signed += 1;
        }

        if signed == 0 {
            return Err(reject(
                "no escrow input is locked to this entry's key".into(),
            ));
        }
        Ok(psbt)
    }

    /// The contract outcome selected by the oracle's attestation.
    pub fn current_outcome(
        attestation: MaybeScalar,
        event: &EventLockingConditions,
    ) -> Result<Outcome, WalletError> {
        let locking_point = attestation.base_point_mul();
        event
            .all_outcomes()
            .into_iter()
            .find(|outcome| match outcome {
                Outcome::Attestation(i) => event.locking_points.get(*i) == Some(&locking_point),
                Outcome::Expiry => false,
            })
            .ok_or(WalletError::NoMatchingOutcome)
    }
}

/// NOAA rank indices use lexicographic permutations followed by the refund-all
/// outcome. Rebuild them locally so a substituted payout table is never signed.
fn expected_payouts(
    players: usize,
    winners: usize,
) -> Result<std::collections::BTreeMap<Outcome, dlctix::PayoutWeights>, WalletError> {
    let reject = || WalletError::Contract("Invalid or oversized payout ranking policy".into());
    if !(2..=100).contains(&players) || winners == 0 || winners > 5 || winners >= players {
        return Err(reject());
    }
    let count = (0..winners)
        .try_fold(1usize, |count, index| count.checked_mul(players - index))
        .ok_or_else(reject)?;
    if count >= 20_000 {
        return Err(reject());
    }
    let weights: &[u64] = match winners {
        1 => &[100],
        2 => &[70, 30],
        3 => &[45, 35, 20],
        4 => &[42, 30, 18, 10],
        5 => &[40, 27, 16, 9, 8],
        _ => return Err(reject()),
    };
    let mut result = std::collections::BTreeMap::new();
    fn ranks(
        players: usize,
        weights: &[u64],
        current: &mut Vec<usize>,
        result: &mut std::collections::BTreeMap<Outcome, dlctix::PayoutWeights>,
    ) {
        if current.len() == weights.len() {
            result.insert(
                Outcome::Attestation(result.len()),
                current
                    .iter()
                    .enumerate()
                    .map(|(rank, player)| (*player, weights[rank]))
                    .collect(),
            );
            return;
        }
        for player in 0..players {
            if !current.contains(&player) {
                current.push(player);
                ranks(players, weights, current, result);
                current.pop();
            }
        }
    }
    ranks(players, weights, &mut Vec::new(), &mut result);
    let equal: dlctix::PayoutWeights = (0..players)
        .map(|i| {
            (
                i,
                100 / players as u64 + u64::from((i as u64) < 100 % players as u64),
            )
        })
        .collect();
    result.insert(Outcome::Attestation(result.len()), equal.clone());
    result.insert(Outcome::Expiry, equal);
    Ok(result)
}

fn signer(client: &NostrClientCore) -> Result<&CustomSigner, WalletError> {
    client
        .signer
        .as_ref()
        .ok_or(WalletError::NostrNotInitialized)
}

fn signer_error(error: impl std::fmt::Display) -> WalletError {
    WalletError::Signer(error.to_string())
}

fn json_digest<T: serde::Serialize>(value: &T) -> Result<[u8; 32], WalletError> {
    let json = serde_json::to_vec(value).map_err(|e| WalletError::Contract(e.to_string()))?;
    Ok(Sha256::digest(json).into())
}

fn reject(reason: String) -> WalletError {
    WalletError::FundingPsbtRejected(reason)
}

fn check_funds_contract(
    psbt: &Psbt,
    funding_outpoint: OutPoint,
    funding_output: &TxOut,
) -> Result<(), WalletError> {
    if psbt.unsigned_tx.compute_txid() != funding_outpoint.txid {
        return Err(reject(
            "transaction is not the accepted contract's funding transaction".into(),
        ));
    }
    let vout = usize::try_from(funding_outpoint.vout).unwrap_or(usize::MAX);
    if psbt.unsigned_tx.output.get(vout) != Some(funding_output) {
        return Err(reject(
            "transaction does not pay the contract funding output".into(),
        ));
    }
    Ok(())
}

fn script_pushes_key(script: &ScriptBuf, key: &PublicKey) -> bool {
    let key = key.to_bytes();
    script.instructions().any(|instruction| {
        matches!(instruction, Ok(Instruction::PushBytes(bytes)) if bytes.as_bytes() == key.as_slice())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dlctix::{
        bitcoin::{
            absolute::LockTime, psbt::PsbtSighashType, transaction::Version, Amount, FeeRate,
            OutPoint, Sequence, Transaction, TxIn, Witness,
        },
        hashlock,
        secp::{MaybePoint, Scalar},
        MarketMaker, PayoutWeights, Player,
    };
    use std::collections::BTreeMap;

    #[test]
    fn frozen_nip44_v2_backup_restores_original_entry_keys() {
        use std::{future::Future, task::Context, task::Poll, task::Waker};

        // Independent NIP-44 v2 vector: identity scalar 1, nonce bytes 0..31,
        // and plaintext coordinator-wallet-v1: followed by seed bytes 0..31 in hex.
        // Its generator was checked against the published NIP-44 encryption vectors.
        const BACKUP: &str = "AgABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4fbs/OmfRMUk7F+0uJ2f7fKhZzIQpO4B+r3cNO0C6SgEL2ltBxC/qEHHZ4nsOzZHscm6bC9zD4bRyH0tNZv8rKjkBrpdjATRlh//6eva50xVC4WNnCop8zKjiciNTn40sGxIjnzKRhhIrHaScdFwNpNvBR2FY5E6gg5Yf/pNGLgrY+SA==";
        let keys = ::nostr::Keys::parse(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap();
        let client = NostrClientCore {
            signer: Some(CustomSigner::Keys(keys)),
        };
        // A local key signer completes synchronously and needs no async runtime.
        let mut load = std::pin::pin!(DlcWalletCore::load(&client, BACKUP, Network::Signet));
        let wallet = match load.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(wallet) => wallet.unwrap(),
            Poll::Pending => panic!("local key signer unexpectedly requires I/O"),
        };
        let entry_id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let registration = wallet.entry_registration(entry_id).unwrap();
        assert_eq!(
            registration.ephemeral_pubkey,
            "02db32ae6adf4d575228bc8de8a99d3f856855bbe2d8d9ff84e9a1c81d9ea73a03"
        );
        assert_eq!(
            registration.payout_hash,
            "33ed8f79efa5a15b4c513b3ed5a22122a7d7eb2663687e3f555561fe13641977"
        );
        let release = wallet
            .payout_release(entry_id, &registration.ephemeral_pubkey)
            .unwrap();
        assert_eq!(
            release.ephemeral_private_key,
            "f53d104768bda153f481882115a011185c917998b2073bb45d6694eb0f9f1aed"
        );
        assert_eq!(
            release.payout_preimage,
            "f0856215fa2ee82c74c788deba04dfb38d068058617409973a5f007ee1327299"
        );
    }

    struct Fixture {
        wallet: DlcWalletCore,
        entry_id: Uuid,
        params: ContractParameters,
        market_maker_key: Scalar,
        other_player_key: Scalar,
    }

    fn fixture() -> Fixture {
        let wallet = DlcWalletCore::create(&NostrClientCore::default(), Network::Signet);
        let entry_id = Uuid::now_v7();
        let key = wallet.entry_key(entry_id).unwrap();
        let mut rng = rand::rng();

        let market_maker_key = Scalar::random(&mut rng);
        let other_player_key = Scalar::random(&mut rng);
        let other = Player {
            pubkey: other_player_key.base_point_mul(),
            ticket_hash: hashlock::sha256(&hashlock::preimage_random(&mut rng)),
            payout_hash: hashlock::sha256(&hashlock::preimage_random(&mut rng)),
        };
        let us = Player {
            pubkey: key.point(),
            ticket_hash: hashlock::sha256(&hashlock::preimage_random(&mut rng)),
            payout_hash: key.payout_hash(),
        };
        let oracle = Scalar::random(&mut rng).base_point_mul();
        let nonce = Scalar::random(&mut rng).base_point_mul();
        let locking_points: Vec<MaybePoint> = [b"us".as_slice(), b"them".as_slice()]
            .iter()
            .map(|msg| dlctix::attestation_locking_point(oracle, nonce, msg))
            .collect();

        let params = ContractParameters {
            market_maker: MarketMaker {
                pubkey: market_maker_key.base_point_mul(),
            },
            players: vec![us, other],
            event: EventLockingConditions {
                locking_points,
                expiry: None,
            },
            outcome_payouts: BTreeMap::from([
                (Outcome::Attestation(0), PayoutWeights::from([(0, 1)])),
                (Outcome::Attestation(1), PayoutWeights::from([(1, 1)])),
            ]),
            fee_rate: FeeRate::from_sat_per_vb_u32(1),
            funding_value: Amount::from_sat(100_000),
            relative_locktime_block_delta: 72,
        };
        Fixture {
            wallet,
            entry_id,
            params,
            market_maker_key,
            other_player_key,
        }
    }

    /// A funding PSBT spending one P2WSH escrow input locked to `escrow_key`.
    fn funding_psbt(f: &Fixture, escrow_key: PublicKey) -> Psbt {
        let witness_script = ScriptBuf::builder()
            .push_key(&escrow_key)
            .push_opcode(dlctix::bitcoin::opcodes::all::OP_CHECKSIG)
            .into_script();
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![f.params.funding_output().unwrap()],
        };
        let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
        psbt.inputs[0].witness_utxo = Some(TxOut {
            value: Amount::from_sat(120_000),
            script_pubkey: ScriptBuf::new_p2wsh(&witness_script.wscript_hash()),
        });
        psbt.inputs[0].witness_script = Some(witness_script);
        psbt
    }

    fn our_key(f: &Fixture) -> PublicKey {
        PublicKey::new(f.wallet.entry_key(f.entry_id).unwrap().pubkey)
    }

    /// Fixture with the contract accepted against `psbt`'s funding output.
    fn accepted(mut f: Fixture, psbt: &Psbt) -> Fixture {
        let outpoint = OutPoint::new(psbt.unsigned_tx.compute_txid(), 0);
        let params = f.params.clone();
        f.wallet.add_contract(f.entry_id, params, outpoint).unwrap();
        f
    }

    fn rejected(result: Result<Psbt, WalletError>) -> bool {
        matches!(result, Err(WalletError::FundingPsbtRejected(_)))
    }

    #[test]
    fn contract_without_our_key_is_refused() {
        let mut f = fixture();
        f.params.players.remove(0);
        let result = f
            .wallet
            .add_contract(f.entry_id, f.params.clone(), OutPoint::null());
        assert!(matches!(result, Err(WalletError::Contract(_))));
    }

    #[test]
    fn contract_with_foreign_payout_hash_is_refused() {
        let mut f = fixture();
        f.params.players[0].payout_hash = [7u8; 32];
        let result = f
            .wallet
            .add_contract(f.entry_id, f.params.clone(), OutPoint::null());
        assert!(matches!(result, Err(WalletError::Contract(_))));
    }

    #[test]
    fn partial_signatures_verify_after_wallet_reload() {
        let f = fixture();
        let psbt = funding_psbt(&f, our_key(&f));
        let mut f = accepted(f, &psbt);
        let id = f.entry_id;
        let dlc = f.wallet.contracts[&id].dlc.clone();
        let our_point = f.wallet.entry_key(id).unwrap().point();

        // Round one: every signer publishes nonces; the market maker aggregates.
        let our_nonces = f.wallet.generate_public_nonces(id).unwrap();
        let other = SigningSession::<NonceSharingRound>::new(
            dlc.clone(),
            &mut rand::rng(),
            f.other_player_key,
        )
        .unwrap();
        let market_maker =
            SigningSession::<NonceSharingRound>::new(dlc, &mut rand::rng(), f.market_maker_key)
                .unwrap()
                .aggregate_nonces_and_compute_partial_signatures(BTreeMap::from([
                    (our_point, our_nonces),
                    (
                        f.other_player_key.base_point_mul(),
                        other.our_public_nonces().clone(),
                    ),
                ]))
                .unwrap();

        // The page reloads: the wallet forgets everything but the contract.
        let outpoint = OutPoint::new(psbt.unsigned_tx.compute_txid(), 0);
        f.wallet.contracts.clear();
        f.wallet
            .add_contract(id, f.params.clone(), outpoint)
            .unwrap();

        // Round two: the re-derived nonces must match the ones sent in round one.
        let signatures = f
            .wallet
            .sign_aggregate_nonces(market_maker.aggregated_nonces().clone(), id)
            .unwrap();
        market_maker
            .verify_partial_signatures(our_point, &signatures)
            .expect("partial signatures from re-derived nonces verify");
    }

    #[test]
    fn nonces_change_with_contract_params() {
        let f = fixture();
        let psbt = funding_psbt(&f, our_key(&f));
        let mut f = accepted(f, &psbt);
        let id = f.entry_id;
        let before = f.wallet.generate_public_nonces(id).unwrap();

        let mut changed = f.params.clone();
        changed.fee_rate = FeeRate::from_sat_per_vb_u32(2);
        let outpoint = OutPoint::new(psbt.unsigned_tx.compute_txid(), 0);
        f.wallet.add_contract(id, changed, outpoint).unwrap();

        assert_ne!(f.wallet.generate_public_nonces(id).unwrap(), before);
    }

    #[test]
    fn refuses_to_sign_a_second_aggregate_nonce() {
        let f = fixture();
        let psbt = funding_psbt(&f, our_key(&f));
        let mut f = accepted(f, &psbt);
        let id = f.entry_id;
        let nonces = f.wallet.generate_public_nonces(id).unwrap();
        let first = nonces
            .clone()
            .map_values(|nonce| AggNonce::sum([nonce.clone()]));
        let second = nonces.map_values(|nonce| AggNonce::sum([nonce.clone(), nonce.clone()]));

        let signed = f.wallet.sign_aggregate_nonces(first.clone(), id).unwrap();
        assert_eq!(
            f.wallet.sign_aggregate_nonces(first, id).unwrap(),
            signed,
            "re-signing the same aggregate is idempotent"
        );
        assert!(matches!(
            f.wallet.sign_aggregate_nonces(second, id),
            Err(WalletError::ConflictingAggregateNonces(_))
        ));
    }

    #[test]
    fn replacing_a_contract_does_not_forget_signed_nonce_contexts() {
        let f = fixture();
        let psbt = funding_psbt(&f, our_key(&f));
        let mut f = accepted(f, &psbt);
        let id = f.entry_id;
        let outpoint = OutPoint::new(psbt.unsigned_tx.compute_txid(), 0);
        let nonces = f.wallet.generate_public_nonces(id).unwrap();
        let first = nonces
            .clone()
            .map_values(|nonce| AggNonce::sum([nonce.clone()]));
        let second = nonces.map_values(|nonce| AggNonce::sum([nonce.clone(), nonce.clone()]));
        let signed = f.wallet.sign_aggregate_nonces(first.clone(), id).unwrap();

        // Re-accepting the same contract must preserve the signing decision.
        f.wallet
            .add_contract(id, f.params.clone(), outpoint)
            .unwrap();
        assert!(matches!(
            f.wallet.sign_aggregate_nonces(second.clone(), id),
            Err(WalletError::ConflictingAggregateNonces(_))
        ));

        // Nor may replacing it and then returning to it reset that decision.
        let mut changed = f.params.clone();
        changed.fee_rate = FeeRate::from_sat_per_vb_u32(2);
        f.wallet.add_contract(id, changed, outpoint).unwrap();
        let changed_nonces = f
            .wallet
            .generate_public_nonces(id)
            .unwrap()
            .map_values(|nonce| AggNonce::sum([nonce.clone()]));
        f.wallet.sign_aggregate_nonces(changed_nonces, id).unwrap();
        f.wallet
            .add_contract(id, f.params.clone(), outpoint)
            .unwrap();
        assert_eq!(f.wallet.sign_aggregate_nonces(first, id).unwrap(), signed);
        assert!(matches!(
            f.wallet.sign_aggregate_nonces(second, id),
            Err(WalletError::ConflictingAggregateNonces(_))
        ));
    }

    #[test]
    fn signs_escrow_input_of_accepted_funding_tx() {
        let f = fixture();
        let psbt = funding_psbt(&f, our_key(&f));
        let f = accepted(f, &psbt);

        let signed = f.wallet.sign_funding_psbt(psbt, f.entry_id).unwrap();
        assert!(signed.inputs[0].partial_sigs.contains_key(&our_key(&f)));
    }

    #[test]
    fn refuses_tampered_funding_tx() {
        let f = fixture();
        let mut psbt = funding_psbt(&f, our_key(&f));
        let f = accepted(f, &psbt);
        psbt.unsigned_tx.output.push(TxOut {
            value: Amount::from_sat(10_000),
            script_pubkey: ScriptBuf::new(),
        });
        psbt.outputs.push(Default::default());

        assert!(rejected(f.wallet.sign_funding_psbt(psbt, f.entry_id)));
    }

    #[test]
    fn refuses_non_all_sighash() {
        let f = fixture();
        let mut psbt = funding_psbt(&f, our_key(&f));
        let f = accepted(f, &psbt);
        psbt.inputs[0].sighash_type = Some(PsbtSighashType::from(EcdsaSighashType::None));

        assert!(rejected(f.wallet.sign_funding_psbt(psbt, f.entry_id)));
    }

    #[test]
    fn refuses_witness_script_not_matching_prevout() {
        let f = fixture();
        let mut psbt = funding_psbt(&f, our_key(&f));
        let f = accepted(f, &psbt);
        psbt.inputs[0].witness_utxo.as_mut().unwrap().script_pubkey = ScriptBuf::new();

        assert!(rejected(f.wallet.sign_funding_psbt(psbt, f.entry_id)));
    }

    #[test]
    fn refuses_psbt_without_our_escrow_input() {
        let f = fixture();
        let stranger = PublicKey::new(f.wallet.entry_key(Uuid::now_v7()).unwrap().pubkey);
        let psbt = funding_psbt(&f, stranger);
        let f = accepted(f, &psbt);

        assert!(rejected(f.wallet.sign_funding_psbt(psbt, f.entry_id)));
    }

    #[test]
    fn payout_release_requires_matching_entry_pubkey() {
        let f = fixture();
        let ours = f.wallet.entry_registration(f.entry_id).unwrap();
        let other = f.wallet.entry_registration(Uuid::now_v7()).unwrap();

        assert!(f
            .wallet
            .payout_release(f.entry_id, &ours.ephemeral_pubkey)
            .is_ok());
        assert!(matches!(
            f.wallet.payout_release(f.entry_id, &other.ephemeral_pubkey),
            Err(WalletError::ForeignEntry(_))
        ));
        assert!(matches!(
            f.wallet.payout_release(f.entry_id, "not a pubkey"),
            Err(WalletError::ForeignEntry(_))
        ));
    }
    fn test_invoice(amount_msat: u64) -> Bolt11Invoice {
        use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
        InvoiceBuilder::new(Currency::Signet)
            .description("ordinary payout".into())
            .payment_hash(dlctix::bitcoin::hashes::sha256::Hash::hash(&[1; 32]))
            .payment_secret(PaymentSecret([2; 32]))
            .amount_milli_satoshis(amount_msat)
            .current_timestamp()
            .min_final_cltv_expiry_delta(18)
            .build_signed(|hash| {
                Secp256k1::new().sign_ecdsa_recoverable(
                    hash,
                    &dlctix::bitcoin::secp256k1::SecretKey::from_slice(&[3; 32]).unwrap(),
                )
            })
            .unwrap()
    }

    fn payout_assignment(f: &Fixture) -> (RegistrationAssignment, PayoutConsent) {
        let invoice = test_invoice(21_000);
        let consent = PayoutConsent {
            competition_id: Uuid::now_v7(),
            lightning_address: Some("alice+prize@wallet.com".into()),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            ticket_invoice: invoice.to_string(),
            ticket_amount_sats: 21,
            expected_funding_sats: f.params.funding_value.to_sat(),
            expected_player_count: 2,
            expected_winner_count: 1,
            expected_relative_locktime_delta: f.params.relative_locktime_block_delta,
            max_fee_rate_sat_vb: f.params.fee_rate.to_sat_per_vb_ceil(),
            oracle_announcement: {
                let mut event = f.params.event.clone();
                event.locking_points.push(MaybePoint::Valid(
                    Scalar::from_slice(&[7; 32]).unwrap().base_point_mul(),
                ));
                event
            },
        };
        let outcomes = expected_payouts(2, 1).unwrap();
        let terms = payout::ContractAuthorization {
            competition_id: consent.competition_id,
            entry_id: f.entry_id,
            network: Network::Signet,
            player_index: 0,
            player_count: 2,
            ticket_hash: invoice.payment_hash().to_byte_array(),
            payout_hash: f.wallet.entry_key(f.entry_id).unwrap().payout_hash(),
            market_maker: f.params.market_maker.clone(),
            event: consent.oracle_announcement.clone(),
            outcome_payouts: outcomes,
            funding_value: f.params.funding_value,
            relative_locktime_block_delta: f.params.relative_locktime_block_delta,
            max_fee_rate: f.params.fee_rate,
        };
        let policy = PayoutPolicy {
            automatic_lightning_address: consent.lightning_address.clone(),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            contract_terms: serde_json::to_string(&terms).unwrap(),
            ark_escrow: None,
        };
        let assignment = RegistrationAssignment {
            session_id: Uuid::now_v7().to_string(),
            user_id: Uuid::now_v7(),
            manifest_hash: vec![1; 32],
            enclave_id: 1,
            enclave_key_epoch: 1,
            enclave_public_key: "key".into(),
            gateway_url: "http://127.0.0.1:1".into(),
            trusted_pcrs: BTreeMap::new(),
            dangerous_trust_unattested_enclaves: true,
            payout_policy: Some(serde_json::to_string(&policy).unwrap()),
        };
        (assignment, consent)
    }

    #[test]
    fn payout_registration_checks_wallet_entry_ticket_and_explicit_choice_before_network() {
        let f = fixture();
        let (assignment, consent) = payout_assignment(&f);
        f.wallet
            .validate_payout_registration(f.entry_id, &assignment, &consent)
            .unwrap();
        for mutate in [
            |c: &mut PayoutConsent| c.lightning_address = Some("mallory@wallet.com".into()),
            |c: &mut PayoutConsent| c.release_entry_key_after_payment = false,
            |c: &mut PayoutConsent| c.allow_invoice_fallback = false,
            |c: &mut PayoutConsent| c.competition_id = Uuid::now_v7(),
            |c: &mut PayoutConsent| c.ticket_amount_sats = 22,
            |c: &mut PayoutConsent| c.expected_funding_sats += 1,
            |c: &mut PayoutConsent| c.expected_player_count = 3,
            |c: &mut PayoutConsent| c.expected_winner_count = 2,
            |c: &mut PayoutConsent| c.expected_relative_locktime_delta += 1,
            |c: &mut PayoutConsent| c.max_fee_rate_sat_vb = 0,
            |c: &mut PayoutConsent| c.oracle_announcement.locking_points.clear(),
        ] {
            let mut bad = consent.clone();
            mutate(&mut bad);
            let mut future =
                std::pin::pin!(f
                    .wallet
                    .keymeld_payout_registration(f.entry_id, &assignment, &bad));
            use std::{
                future::Future,
                task::{Context, Poll, Waker},
            };
            assert!(
                matches!(
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Err(_))
                ),
                "invalid consent must fail before any async network operation"
            );
        }
        for field in ["entry_id", "payout_hash", "ticket_hash", "network"] {
            let mut bad = assignment.clone();
            let mut policy: PayoutPolicy =
                serde_json::from_str(bad.payout_policy.as_deref().unwrap()).unwrap();
            let mut terms: serde_json::Value =
                serde_json::from_str(&policy.contract_terms).unwrap();
            terms[field] = match field {
                "entry_id" => serde_json::json!(Uuid::now_v7()),
                "network" => serde_json::json!("bitcoin"),
                _ => serde_json::to_value([9u8; 32]).unwrap(),
            };
            policy.contract_terms = terms.to_string();
            bad.payout_policy = Some(serde_json::to_string(&policy).unwrap());
            assert!(
                f.wallet
                    .validate_payout_registration(f.entry_id, &bad, &consent)
                    .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn payout_registration_checks_the_arkade_escrow() {
        use coordinator_ark_escrow::{EntryEscrow, EscrowTerms, RelativeTimelock};
        use coordinator_core::keymeld::ArkEscrowPolicy;
        use dlctix::bitcoin::absolute::LockTime;

        let f = fixture();
        let (assignment, mut consent) = payout_assignment(&f);
        let expiry = 1_900_000_000;
        consent.oracle_announcement.expiry = Some(expiry);
        // Each of the two players pays their 50,000 sat share of the pool plus a 100 sat fee.
        consent.ticket_amount_sats = 50_100;
        consent.ticket_invoice = test_invoice(50_100_000).to_string();
        let xonly =
            |point: Point| ark::XOnlyPublicKey::from_slice(&point.serialize_xonly()).unwrap();
        let player = xonly(f.wallet.entry_key(f.entry_id).unwrap().point());
        let market_maker = xonly(f.params.market_maker.pubkey);
        let stranger = xonly(f.other_player_key.base_point_mul());
        let server = xonly(Scalar::from_slice(&[5; 32]).unwrap().base_point_mul());
        let with_escrow = |player, coordinator, refund_at: u32, max_fee_sats| {
            let refund_locktime = LockTime::from_time(refund_at).unwrap();
            let exit_delay = RelativeTimelock::Seconds(2048);
            let escrow = EntryEscrow::new(EscrowTerms {
                player,
                coordinator,
                server,
                refund_locktime,
                exit_delay,
                unilateral_refund_delay: EscrowTerms::unilateral_refund_delay_for(
                    refund_locktime,
                    exit_delay,
                    refund_at - 86_400,
                )
                .unwrap(),
            })
            .unwrap();
            let mut assignment = assignment.clone();
            let mut policy: PayoutPolicy =
                serde_json::from_str(assignment.payout_policy.as_deref().unwrap()).unwrap();
            let mut terms: serde_json::Value =
                serde_json::from_str(&policy.contract_terms).unwrap();
            terms["event"] = serde_json::to_value(&consent.oracle_announcement).unwrap();
            policy.contract_terms = terms.to_string();
            policy.ark_escrow = Some(ArkEscrowPolicy {
                escrow_tap_tree: hex::encode(escrow.vtxo_script().encode_tap_tree()),
                max_fee_sats,
                max_refund_fee_sats: max_fee_sats.min(100),
                checkpoint_exit_script: hex::encode([0x51]),
            });
            assignment.payout_policy = Some(serde_json::to_string(&policy).unwrap());
            f.wallet
                .validate_payout_registration(f.entry_id, &assignment, &consent)
        };

        with_escrow(player, market_maker, expiry, 100).unwrap();
        for (label, result) in [
            (
                "foreign player key",
                with_escrow(stranger, market_maker, expiry, 100),
            ),
            (
                "foreign coordinator key",
                with_escrow(player, stranger, expiry, 100),
            ),
            (
                "refund after expiry",
                with_escrow(player, market_maker, expiry + 1, 100),
            ),
            (
                "fee beyond the markup",
                with_escrow(player, market_maker, expiry, 101),
            ),
        ] {
            assert!(result.is_err(), "{label}");
        }
    }

    fn invoice_consent(f: &mut Fixture) -> PayoutInvoiceConsent {
        let attestation = Scalar::from_slice(&[8; 32]).unwrap();
        f.params.event.locking_points[0] = MaybePoint::Valid(attestation.base_point_mul());
        for weights in f.params.outcome_payouts.values_mut() {
            for weight in weights.values_mut() {
                *weight = 100;
            }
        }
        let contract = payout::ContractCommitment {
            contract_parameters: f.params.clone(),
            funding_outpoint: OutPoint::null(),
        };
        let dlc = TicketedDLC::new(f.params.clone(), OutPoint::null()).unwrap();
        let key = f.wallet.entry_key(f.entry_id).unwrap();
        let ours =
            SigningSession::<NonceSharingRound>::new(dlc.clone(), &mut rand::rng(), key.scalar())
                .unwrap();
        let other = SigningSession::<NonceSharingRound>::new(
            dlc.clone(),
            &mut rand::rng(),
            f.other_player_key,
        )
        .unwrap();
        let dealer =
            SigningSession::<NonceSharingRound>::new(dlc, &mut rand::rng(), f.market_maker_key)
                .unwrap()
                .aggregate_nonces_and_compute_partial_signatures(BTreeMap::from([
                    (key.point(), ours.our_public_nonces().clone()),
                    (
                        f.other_player_key.base_point_mul(),
                        other.our_public_nonces().clone(),
                    ),
                ]))
                .unwrap();
        let ours = ours
            .compute_partial_signatures(dealer.aggregated_nonces().clone())
            .unwrap();
        let other = other
            .compute_partial_signatures(dealer.aggregated_nonces().clone())
            .unwrap();
        let signed = dealer
            .aggregate_all_signatures(BTreeMap::from([
                (key.point(), ours.our_partial_signatures().clone()),
                (
                    f.other_player_key.base_point_mul(),
                    other.our_partial_signatures().clone(),
                ),
            ]))
            .unwrap();
        let invoice = test_invoice(100_000_000).to_string();
        let competition_id = Uuid::now_v7();
        PayoutInvoiceConsent {
            entry_id: f.entry_id,
            competition_id,
            expected_pubkey: key.point().to_string(),
            context: payout_protocol::InvoiceAuthorizationContext {
                keygen_session_id: Uuid::now_v7().into(),
                user_id: Uuid::now_v7().into(),
                claim_id: Uuid::now_v7(),
                competition_id,
                entry_id: f.entry_id,
                contract_digest: payout::contract_digest(&contract).unwrap(),
                invoice_digest: payout::invoice_digest(&invoice),
                amount_msat: 100_000_000,
                expires_at: ::nostr::Timestamp::now().as_secs() + 300,
            },
            invoice,
            contract,
            signatures: signed.all_signatures().clone(),
            attestation: hex::encode(attestation.serialize()),
        }
    }

    #[test]
    fn invoice_authorization_binds_completed_contract_amount_and_exact_invoice() {
        let mut f = fixture();
        let consent = invoice_consent(&mut f);
        let signed = f.wallet.authorize_payout_invoice(consent.clone()).unwrap();
        signed
            .verify(
                &f.wallet.entry_key(f.entry_id).unwrap().point().serialize(),
                &consent.context,
                ::nostr::Timestamp::now().as_secs(),
            )
            .unwrap();
        for mutate in [
            |c: &mut PayoutInvoiceConsent| c.context.invoice_digest = "00".repeat(32),
            |c: &mut PayoutInvoiceConsent| c.context.contract_digest = "00".repeat(32),
            |c: &mut PayoutInvoiceConsent| c.context.amount_msat += 1000,
            |c: &mut PayoutInvoiceConsent| c.context.entry_id = Uuid::now_v7(),
            |c: &mut PayoutInvoiceConsent| c.context.competition_id = Uuid::now_v7(),
            |c: &mut PayoutInvoiceConsent| c.context.expires_at = 0,
            // Well past the ten minutes allowed, so a second's drift cannot make it legal.
            |c: &mut PayoutInvoiceConsent| {
                c.context.expires_at = ::nostr::Timestamp::now().as_secs() + 3_600
            },
            |c: &mut PayoutInvoiceConsent| c.signatures.outcome_tx_signatures.clear(),
            |c: &mut PayoutInvoiceConsent| {
                c.contract.contract_parameters.players[0].payout_hash = [4; 32]
            },
            |c: &mut PayoutInvoiceConsent| c.invoice = test_invoice(99_000_000).to_string(),
        ] {
            let mut bad = consent.clone();
            mutate(&mut bad);
            assert!(f.wallet.authorize_payout_invoice(bad).is_err());
        }
    }
}
