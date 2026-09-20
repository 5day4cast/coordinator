use super::{
    keymeld_trust::trusted_assignment,
    keys::{EntryKey, WalletSeed},
    EncryptedWalletBackup, EntryRegistration, PayoutRelease, WalletError,
};
use crate::nostr::{CustomSigner, NostrClientCore};
use coordinator_core::PayoutPolicy;
use coordinator_core::{
    keymeld::{prepare_registration, PreparedRegistration},
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
use nostr_sdk::NostrSigner;
use rand_chacha::{rand_core::SeedableRng, ChaCha20Rng};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;
use zeroize::Zeroizing;

pub struct DlcWalletCore {
    seed: WalletSeed,
    network: Network,
    nostr_client: NostrClientCore,
    secp: Secp256k1<All>,
    contracts: HashMap<Uuid, EntryContract>,
}

/// A contract the user has checked and agreed to sign for one entry.
struct EntryContract {
    dlc: TicketedDLC,
    /// Digest of the accepted `ContractParameters`; part of the nonce seed.
    params_digest: [u8; 32],
    /// Digest of the aggregate nonces already signed in this page session.
    signed_aggregate: Option<[u8; 32]>,
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
    ///
    /// `payout_policy` is sealed in the same envelope: only the enclave can
    /// then release this entry's payout preimage, and only for a payment to
    /// that address.
    pub async fn keymeld_registration(
        &self,
        entry_id: Uuid,
        assignment: &RegistrationAssignment,
        payout_policy: Option<PayoutPolicy>,
    ) -> Result<PreparedRegistration, WalletError> {
        let assignment = trusted_assignment(assignment, self.network)?;
        let key = self.entry_key(entry_id)?;
        prepare_registration(&key.secret_bytes(), &assignment, payout_policy)
            .await
            .map_err(|e| WalletError::Keymeld(e.to_string()))
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
        self.contracts.insert(
            entry_id,
            EntryContract {
                dlc,
                params_digest,
                signed_aggregate: None,
            },
        );
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
        if contract
            .signed_aggregate
            .is_some_and(|signed| signed != digest)
        {
            return Err(WalletError::ConflictingAggregateNonces(entry_id));
        }

        let signed = self
            .signing_session(entry_id, contract)?
            .compute_partial_signatures(aggregate_nonces)
            .map_err(|e| WalletError::Signing(e.to_string()))?;
        if let Some(contract) = self.contracts.get_mut(&entry_id) {
            contract.signed_aggregate = Some(digest);
        }
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
}
