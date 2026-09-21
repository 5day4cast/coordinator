//! A pool's ticketed DLC, funded by the kickoff batch.
//!
//! Every DLC transaction spends the contract's funding outpoint.
//! That outpoint is an output of the batch's commitment transaction, so it exists only once the batch reaches finalization.
//! The contract is therefore signed inside the batch, in [`KickoffHooks::before_forfeits`]:
//!
//! 1. The intent pays [`DlcKickoff::funding_output`], which depends only on the contract parameters.
//! 2. When the commitment transaction is known, build the [`TicketedDLC`] on the funding outpoint.
//! 3. The [`ContractSigner`] (Keymeld in production) signs every transaction, including the expiry transaction.
//! 4. Verify every signature as the market maker, and hand the signed contract to [`ContractSigner::keep`].
//! 5. Only then does the kickoff forfeit the escrows.
//!
//! Players' funds therefore never reach a funding output without a signed expiry transaction to take them back out.

use std::sync::Mutex;

use async_trait::async_trait;
use bitcoin::{OutPoint, Psbt, TxOut};
use dlctix::musig2::PubNonce;
use dlctix::secp::{Point, Scalar};
use dlctix::{
    ContractParameters, ContractSignatures, NonceSharingRound, Outcome, SigMap, SignedContract,
    SigningSession, TicketedDLC,
};

use crate::{BoxError, Error, KickoffHooks};

/// Signs every transaction of a pool's contract once its funding outpoint is known.
#[async_trait]
pub trait ContractSigner: Send + Sync {
    /// Sign every outcome, split, and expiry transaction of `dlc`.
    ///
    /// `commitment_tx` is the batch's commitment transaction, which pays `dlc`'s funding outpoint.
    /// In production this is Keymeld's `sign_ark_dlc_batch`.
    /// It runs inside the Arkade batch, so it must finish within the server's session window.
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        commitment_tx: &Psbt,
    ) -> Result<ContractSignatures, BoxError>;

    /// Store the verified contract. The escrows are forfeited as soon as this returns.
    ///
    /// `commitment_tx` is the batch's unsigned commitment transaction, which pays the funding output.
    async fn keep(&self, contract: &SignedContract, commitment_tx: &Psbt) -> Result<(), BoxError> {
        let _ = (contract, commitment_tx);
        Ok(())
    }
}

#[async_trait]
impl<T: ContractSigner + ?Sized> ContractSigner for std::sync::Arc<T> {
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        commitment_tx: &Psbt,
    ) -> Result<ContractSignatures, BoxError> {
        (**self).sign_contract(dlc, commitment_tx).await
    }

    async fn keep(&self, contract: &SignedContract, commitment_tx: &Psbt) -> Result<(), BoxError> {
        (**self).keep(contract, commitment_tx).await
    }
}

/// Kickoff hooks that sign a pool's DLC against the batch's funding output before any forfeit.
pub struct DlcKickoff<S> {
    params: ContractParameters,
    funding_output: TxOut,
    signer: S,
    signed: Mutex<Option<SignedContract>>,
}

impl<S: ContractSigner> DlcKickoff<S> {
    /// Check `params` and prepare to sign them in the batch.
    ///
    /// The contract must have an expiry outcome, so players can always leave the funding output.
    pub fn new(params: ContractParameters, signer: S) -> Result<Self, Error> {
        params.validate().map_err(contract_error)?;
        if params.event.expiry.is_none() || !params.outcome_payouts.contains_key(&Outcome::Expiry) {
            return Err(Error::InvalidPool(
                "a pool contract needs an expiry outcome, so players can leave the funding output"
                    .into(),
            ));
        }
        let funding_output = params.funding_output().map_err(contract_error)?;
        Ok(Self {
            params,
            funding_output,
            signer,
            signed: Mutex::new(None),
        })
    }

    pub fn params(&self) -> &ContractParameters {
        &self.params
    }

    pub fn signer(&self) -> &S {
        &self.signer
    }

    /// The output the kickoff intent must pay: the contract's funding script and value.
    pub fn funding_output(&self) -> &TxOut {
        &self.funding_output
    }

    /// The signed contract, once [`KickoffHooks::before_forfeits`] has run.
    pub fn signed_contract(&self) -> Option<SignedContract> {
        self.signed
            .lock()
            .expect("the signed contract lock is never poisoned")
            .clone()
    }
}

#[async_trait]
impl<S: ContractSigner> KickoffHooks for DlcKickoff<S> {
    async fn before_forfeits(
        &self,
        funding: OutPoint,
        commitment_tx: &Psbt,
    ) -> Result<(), BoxError> {
        let paid = commitment_tx.unsigned_tx.output.get(funding.vout as usize);
        if paid != Some(&self.funding_output) {
            return Err("the funding outpoint does not hold the contract's funding output".into());
        }
        let dlc = TicketedDLC::new(self.params.clone(), funding)?;
        let signatures = self.signer.sign_contract(&dlc, commitment_tx).await?;
        if signatures.expiry_tx_signature.is_none() {
            return Err("the contract signatures leave out the expiry transaction".into());
        }
        // Only the market maker's verification covers every signature in the contract.
        let market_maker = self.params.market_maker.pubkey;
        let contract = dlc.into_signed_contract(market_maker, signatures)?;
        self.signer.keep(&contract, commitment_tx).await?;
        *self
            .signed
            .lock()
            .expect("the signed contract lock is never poisoned") = Some(contract);
        Ok(())
    }
}

/// Signs a contract with every party's secret key held locally.
///
/// For tests and test networks: it stands in for Keymeld, which holds the players' keys in production.
pub struct LocalContractSigner {
    market_maker: Scalar,
    players: Vec<Scalar>,
}

impl LocalContractSigner {
    pub fn new(market_maker: Scalar, players: impl IntoIterator<Item = Scalar>) -> Self {
        Self {
            market_maker,
            players: players.into_iter().collect(),
        }
    }
}

#[async_trait]
impl ContractSigner for LocalContractSigner {
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        _commitment_tx: &Psbt,
    ) -> Result<ContractSignatures, BoxError> {
        let mut rng = rand::rng();
        let mut session = |seckey: Scalar| {
            SigningSession::<NonceSharingRound>::new(dlc.clone(), &mut rng, seckey)
        };

        let market_maker = session(self.market_maker)?;
        let players = self
            .players
            .iter()
            .map(|&seckey| Ok((seckey.base_point_mul(), session(seckey)?)))
            .collect::<Result<Vec<(Point, _)>, dlctix::Error>>()?;

        let nonces: std::collections::BTreeMap<Point, SigMap<PubNonce>> = players
            .iter()
            .map(|(pubkey, session)| (*pubkey, session.our_public_nonces().clone()))
            .chain([(
                self.market_maker.base_point_mul(),
                market_maker.our_public_nonces().clone(),
            )])
            .collect();
        let market_maker = market_maker.aggregate_nonces_and_compute_partial_signatures(nonces)?;

        let mut partial_signatures = std::collections::BTreeMap::new();
        for (pubkey, session) in players {
            let session =
                session.compute_partial_signatures(market_maker.aggregated_nonces().clone())?;
            market_maker.verify_partial_signatures(pubkey, session.our_partial_signatures())?;
            partial_signatures.insert(pubkey, session.our_partial_signatures().clone());
        }
        let contract = market_maker.aggregate_all_signatures(partial_signatures)?;
        Ok(contract.all_signatures().clone())
    }
}

fn contract_error(error: dlctix::Error) -> Error {
    Error::InvalidPool(format!("the pool contract is invalid: {error}"))
}
