//! Kicking off a competition's pool in an Arkade batch, with Keymeld signing for every player.
//!
//! The batch spends the pool's escrow VTXOs into its dlctix funding output.
//! Keymeld holds each player's entry key and signs as them:
//!
//! 1. The intent proof, for each player's escrow.
//! 2. The whole contract, once the batch's commitment transaction fixes the funding outpoint.
//! 3. Each player's forfeit, only after the contract is signed.
//!
//! The Coordinator verifier inside Keymeld checks every step against the players' consent.
//! The pool's contract must be bound before the batch, with a null funding outpoint.
//! See `coordinator_ark::DlcKickoff` and `docs/QUEUED_COMPETITIONS.md`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::secp256k1::schnorr;
use bitcoin::{Psbt, XOnlyPublicKey};
use coordinator_ark::{BoxError, ContractSigner, EscrowSigner, SigningPurpose, SigningRequest};
use coordinator_escrow::ark::{psbt_hex, ArkEscrowSpend, ArkFunding};
use dlctix::{ContractSignatures, SignedContract, TicketedDLC};
use keymeld_sdk::prelude::UserId;

use crate::infra::keymeld::{DlcKeygenSession, Keymeld};

/// The Arkade server and swap service that fund Arkade competitions.
pub struct Arkade {
    pub server: coordinator_ark::ArkServer,
    pub swaps: Arc<dyn crate::infra::ark_swap::EscrowSwaps>,
    /// How long after the observation window starts an unfunded entry can be refunded.
    pub refund_after_start_secs: u64,
    /// The most a refund's swap may keep for paying the player's Lightning Address.
    pub max_refund_fee_sats: u64,
}

/// Keymeld as a pool's contract signer and every player's escrow signer.
pub struct KeymeldArkPool {
    keymeld: Arc<dyn Keymeld>,
    session: DlcKeygenSession,
    /// The players' Keymeld identities, in contract order.
    players: Vec<UserId>,
    /// Each player's entry key, which is also their escrow's player key.
    entry_keys: HashMap<XOnlyPublicKey, UserId>,
    /// Set once the contract is signed and verified, before any forfeit.
    contract: Mutex<Option<SignedContract>>,
    /// The batch's commitment transaction, which pays the contract's funding output.
    commitment: Mutex<Option<bitcoin::Transaction>>,
}

impl KeymeldArkPool {
    /// `players` pairs each player's entry key with their Keymeld identity, in contract order.
    pub fn new(
        keymeld: Arc<dyn Keymeld>,
        session: DlcKeygenSession,
        players: Vec<(XOnlyPublicKey, UserId)>,
    ) -> Self {
        Self {
            keymeld,
            session,
            entry_keys: players.iter().cloned().collect(),
            players: players.into_iter().map(|(_, user)| user).collect(),
            contract: Mutex::new(None),
            commitment: Mutex::new(None),
        }
    }

    /// The commitment transaction the signed contract spends from.
    pub fn commitment_tx(&self) -> Option<bitcoin::Transaction> {
        self.commitment
            .lock()
            .expect("the commitment lock is never poisoned")
            .clone()
    }

    /// The signed contract, once the kickoff hook has run.
    pub fn signed_contract(&self) -> Option<SignedContract> {
        self.contract
            .lock()
            .expect("the contract lock is never poisoned")
            .clone()
    }

    fn spend(&self, request: &SigningRequest) -> Result<ArkEscrowSpend, BoxError> {
        Ok(match &request.purpose {
            SigningPurpose::IntentProof => ArkEscrowSpend::IntentProof {
                proof_psbt: psbt_hex(&request.psbt),
            },
            SigningPurpose::Forfeit {
                commitment_tx,
                connectors,
            } => {
                let contract = self
                    .signed_contract()
                    .ok_or("a forfeit needs the pool's signed contract first")?;
                ArkEscrowSpend::Forfeit {
                    forfeit_psbt: psbt_hex(&request.psbt),
                    funding: ArkFunding::new(commitment_tx, contract.dlc().funding_outpoint().vout),
                    connector_txs: connectors.iter().map(serialize_hex).collect(),
                    contract_signatures: serde_json::to_string(contract.all_signatures())?,
                }
            }
        })
    }
}

#[async_trait]
impl ContractSigner for KeymeldArkPool {
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        commitment_tx: &Psbt,
    ) -> Result<ContractSignatures, BoxError> {
        let funding = ArkFunding::new(&commitment_tx.unsigned_tx, dlc.funding_outpoint().vout);
        let signatures = self
            .keymeld
            .sign_ark_dlc_batch(
                &self.session,
                &dlc.signing_data()?,
                dlc.params(),
                self.players.clone(),
                funding,
            )
            .await?;
        Ok(ContractSignatures {
            expiry_tx_signature: signatures.expiry_signature,
            outcome_tx_signatures: signatures.outcome_signatures,
            split_tx_signatures: signatures.split_signatures,
        })
    }

    async fn keep(&self, contract: &SignedContract, commitment_tx: &Psbt) -> Result<(), BoxError> {
        *self
            .commitment
            .lock()
            .expect("the commitment lock is never poisoned") =
            Some(commitment_tx.unsigned_tx.clone());
        *self
            .contract
            .lock()
            .expect("the contract lock is never poisoned") = Some(contract.clone());
        Ok(())
    }
}

#[async_trait]
impl EscrowSigner for KeymeldArkPool {
    /// One Keymeld round trip for all players; each spend signs every input of one player's escrow.
    async fn sign(&self, requests: &[SigningRequest]) -> Result<Vec<schnorr::Signature>, BoxError> {
        let mut groups: BTreeMap<(Vec<u8>, bitcoin::Txid), Vec<usize>> = BTreeMap::new();
        for (index, request) in requests.iter().enumerate() {
            groups
                .entry((
                    request.key.serialize().to_vec(),
                    request.psbt.unsigned_tx.compute_txid(),
                ))
                .or_default()
                .push(index);
        }
        let groups: Vec<Vec<usize>> = groups.into_values().collect();
        let spends = groups
            .iter()
            .map(|indexes| {
                let first = &requests[indexes[0]];
                let user = self
                    .entry_keys
                    .get(&first.key)
                    .ok_or_else(|| format!("no player holds entry key {}", first.key))?;
                Ok((user.clone(), self.spend(first)?))
            })
            .collect::<Result<Vec<_>, BoxError>>()?;
        let signed = self.keymeld.sign_ark_escrows(&self.session, spends).await?;
        let mut signatures = vec![None; requests.len()];
        for (indexes, signed) in groups.into_iter().zip(signed) {
            for index in indexes {
                let input = requests[index].input_index;
                let (_, signature) = signed
                    .iter()
                    .find(|(signed_input, _)| *signed_input == input)
                    .ok_or_else(|| format!("Keymeld did not sign input {input}"))?;
                signatures[index] = Some(schnorr::Signature::from_slice(signature)?);
            }
        }
        Ok(signatures
            .into_iter()
            .map(|signature| signature.expect("every request is in a group"))
            .collect())
    }
}
