//! Signing escrow spends.
//!
//! Every spend of an escrow's funding leaf needs the player's key and the coordinator's key.
//! (The server adds its own signature.)
//! Keymeld holds each entry's player key and signs only what its policy allows.
//! So each request carries the whole transaction, and the purpose it serves, for the signer to check.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bitcoin::hashes::Hash;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::{schnorr, Message};
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::{
    OutPoint, Psbt, TapLeafHash, TapSighash, TapSighashType, Transaction, Txid, XOnlyPublicKey,
};

use crate::{BoxError, Error};

/// What a requested signature is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningPurpose {
    /// The intent proof registering a pool's escrows for a batch.
    ///
    /// It proves ownership and names the pool's funding output. It moves nothing by itself.
    IntentProof,
    /// A forfeit, giving the escrow to the server once `commitment_txid` confirms.
    ///
    /// The forfeit also spends a connector from that commitment transaction, so it is void without it.
    /// The commitment transaction pays the pool's funding output.
    Forfeit {
        /// The batch's commitment transaction, which pays the pool's funding output.
        commitment_tx: Arc<Transaction>,
        /// The connector transactions from the one this forfeit spends up to the one spending
        /// the commitment transaction, so a signer can check the forfeit is void without it.
        connectors: Arc<Vec<Transaction>>,
    },
}

impl SigningPurpose {
    /// The commitment transaction a forfeit depends on.
    pub fn commitment_txid(&self) -> Option<Txid> {
        match self {
            SigningPurpose::IntentProof => None,
            SigningPurpose::Forfeit { commitment_tx, .. } => Some(commitment_tx.compute_txid()),
        }
    }
}

/// One signature to make: a script-path spend of an escrow's funding leaf.
#[derive(Debug, Clone)]
pub struct SigningRequest {
    pub purpose: SigningPurpose,
    /// The escrow VTXO being spent.
    pub escrow: OutPoint,
    /// The key to sign with: the escrow's player key or the coordinator key.
    pub key: XOnlyPublicKey,
    /// The transaction, with every input's witness UTXO and this input's leaf script filled in.
    pub psbt: Arc<Psbt>,
    pub input_index: usize,
    pub leaf_hash: TapLeafHash,
    /// The BIP341 script-path sighash with `SIGHASH_DEFAULT`.
    ///
    /// A signer that checks policy should recompute this with [`script_spend_sighash`].
    pub sighash: TapSighash,
}

/// Something that can sign escrow spends.
#[async_trait]
pub trait EscrowSigner: Send + Sync {
    /// Sign every request, returning one BIP340 signature per request, in order.
    async fn sign(&self, requests: &[SigningRequest]) -> Result<Vec<schnorr::Signature>, BoxError>;
}

#[async_trait]
impl<T: EscrowSigner + ?Sized> EscrowSigner for Arc<T> {
    async fn sign(&self, requests: &[SigningRequest]) -> Result<Vec<schnorr::Signature>, BoxError> {
        (**self).sign(requests).await
    }
}

/// Signs with keys held in memory: the coordinator's key, or test keys.
pub struct KeypairSigner {
    secp: Secp256k1<bitcoin::secp256k1::All>,
    keypairs: HashMap<XOnlyPublicKey, Keypair>,
}

impl KeypairSigner {
    pub fn new(keypairs: impl IntoIterator<Item = Keypair>) -> Self {
        Self {
            secp: Secp256k1::new(),
            keypairs: keypairs
                .into_iter()
                .map(|keypair| (keypair.x_only_public_key().0, keypair))
                .collect(),
        }
    }
}

#[async_trait]
impl EscrowSigner for KeypairSigner {
    async fn sign(&self, requests: &[SigningRequest]) -> Result<Vec<schnorr::Signature>, BoxError> {
        requests
            .iter()
            .map(|request| {
                let keypair = self
                    .keypairs
                    .get(&request.key)
                    .ok_or_else(|| format!("no key for {}", request.key))?;
                let message = Message::from_digest(request.sighash.to_byte_array());
                Ok(self.secp.sign_schnorr(&message, keypair))
            })
            .collect()
    }
}

/// The sighash for spending `input_index` of `psbt` through the leaf `leaf_hash`, with `SIGHASH_DEFAULT`.
///
/// Every input needs its witness UTXO, since taproot sighashes commit to all prevouts.
pub fn script_spend_sighash(
    psbt: &Psbt,
    input_index: usize,
    leaf_hash: TapLeafHash,
) -> Result<TapSighash, Error> {
    let prevouts = psbt
        .inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            input
                .witness_utxo
                .clone()
                .ok_or_else(|| Error::Protocol(format!("input {index} has no witness UTXO")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    SighashCache::new(&psbt.unsigned_tx)
        .taproot_script_spend_signature_hash(
            input_index,
            &Prevouts::All(&prevouts),
            leaf_hash,
            TapSighashType::Default,
        )
        .map_err(|error| Error::Protocol(format!("sighash for input {input_index}: {error}")))
}

/// Ask the players' signer and the coordinator's signer for every request, and check each signature.
pub(crate) async fn collect_signatures(
    requests: Vec<SigningRequest>,
    coordinator_key: XOnlyPublicKey,
    players: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
) -> Result<Vec<(SigningRequest, schnorr::Signature)>, Error> {
    let (for_coordinator, for_players): (Vec<_>, Vec<_>) = requests
        .into_iter()
        .partition(|request| request.key == coordinator_key);
    let (player_signatures, coordinator_signatures) = futures::try_join!(
        sign_all(players, &for_players),
        sign_all(coordinator, &for_coordinator),
    )?;

    let secp = Secp256k1::verification_only();
    let signed = for_players
        .into_iter()
        .zip(player_signatures)
        .chain(for_coordinator.into_iter().zip(coordinator_signatures))
        .map(|(request, signature)| {
            let message = Message::from_digest(request.sighash.to_byte_array());
            secp.verify_schnorr(&signature, &message, &request.key)
                .map_err(|_| Error::BadSignature {
                    escrow: request.escrow,
                    key: request.key,
                })?;
            Ok((request, signature))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(signed)
}

async fn sign_all(
    signer: &dyn EscrowSigner,
    requests: &[SigningRequest],
) -> Result<Vec<schnorr::Signature>, Error> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let signatures = signer.sign(requests).await.map_err(Error::Signer)?;
    if signatures.len() != requests.len() {
        return Err(Error::Signer(
            format!(
                "asked for {} signatures, got {}",
                requests.len(),
                signatures.len()
            )
            .into(),
        ));
    }
    Ok(signatures)
}

/// Put a checked signature into its PSBT input.
pub(crate) fn insert_signature(
    psbt: &mut Psbt,
    request: &SigningRequest,
    signature: schnorr::Signature,
) {
    psbt.inputs[request.input_index].tap_script_sigs.insert(
        (request.key, request.leaf_hash),
        bitcoin::taproot::Signature {
            signature,
            sighash_type: TapSighashType::Default,
        },
    );
}
