//! The daemon boards coins and signs Ark spends with its existing Ark key provider.
//! It does not expose the SDK's separate descriptor wallet, unilateral exits, or CPFP.
//! Reject those SDK wallet operations explicitly rather than constructing an unused
//! BDK wallet with a different derivation and a third-party indexer.

use ark_client::{
    wallet::{Balance, OnchainWallet},
    Error,
};
use ark_core::UtxoCoinSelection;
use bitcoin::{Address, Amount, FeeRate, Psbt};

pub(crate) struct BoardingOnlyWallet;

fn unsupported() -> Error {
    Error::wallet("ark-swapd supports cooperative boarding through its Ark key provider; independent on-chain wallet operations are unavailable")
}

impl OnchainWallet for BoardingOnlyWallet {
    fn get_onchain_address(&self) -> Result<Address, Error> {
        Err(unsupported())
    }

    async fn sync(&self) -> Result<(), Error> {
        Err(unsupported())
    }

    fn balance(&self) -> Result<Balance, Error> {
        Err(unsupported())
    }

    fn prepare_send_to_address(
        &self,
        _address: Address,
        _amount: Amount,
        _fee_rate: FeeRate,
    ) -> Result<Psbt, Error> {
        Err(unsupported())
    }

    fn sign(&self, _psbt: &mut Psbt) -> Result<bool, Error> {
        Err(unsupported())
    }

    fn select_coins(&self, _target_amount: Amount) -> Result<UtxoCoinSelection, Error> {
        Err(unsupported())
    }
}
