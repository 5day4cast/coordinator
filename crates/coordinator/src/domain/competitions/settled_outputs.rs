//! What the chain holds at the address of the coordinator's own key.
//!
//! Every transaction that closes a settled contract pays there (`simple_sweep_tx`): the close
//! of a pot whose winners were all paid over Lightning, the close of one paid winner's split
//! output, and the reclaim of a split output its winner never claimed. The output key is the
//! coordinator's key itself, with no taproot tweak, so a key-path signature with that key
//! spends it. LND's wallet does not hold the key: it neither lists these outputs nor can
//! spend them, and nothing in the coordinator spends them either. This only reads them, for
//! the wallet page and the metrics.

use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use bitcoin::{Address, ScriptBuf};
use time::OffsetDateTime;

use super::{p2tr_script_pubkey, Coordinator};
use crate::infra::{
    bitcoin::ScriptFunds,
    refresh_cache::{Cached, RefreshCache},
};

/// How long a read is reused. Listing hundreds of outputs takes the chain index a while, and
/// they change only when a contract settles.
const READ_EVERY: Duration = Duration::from_secs(300);

/// The last read of each script, shared by the wallet page and the metrics. `None` is the
/// answer of a chain backend that cannot list a script's outputs.
static READS: LazyLock<Arc<RefreshCache<ScriptBuf, Option<ScriptFunds>>>> =
    LazyLock::new(|| Arc::new(RefreshCache::new()));

impl Coordinator {
    /// The script the closing transactions of settled contracts pay.
    pub fn settled_outputs_script(&self) -> ScriptBuf {
        p2tr_script_pubkey(self.public_key)
    }

    /// That script's address on the coordinator's network.
    pub fn settled_outputs_address(&self) -> Option<Address> {
        Address::from_script(&self.settled_outputs_script(), self.bitcoin.get_network()).ok()
    }

    /// The unspent outputs at that script as of the last read. One older than five minutes is
    /// read again in the background, and each read sets the metrics. With nothing read yet,
    /// this waits up to `wait` for the first one.
    pub async fn settled_outputs(&self, wait: Duration) -> Cached<Option<ScriptFunds>> {
        let script = self.settled_outputs_script();
        let bitcoin = self.bitcoin.clone();
        let listed = script.clone();
        READS
            .get(script, READ_EVERY, wait, move || async move {
                let funds = bitcoin.script_funds(listed).await?;
                if let Some(funds) = funds {
                    crate::metrics::record_settled_outputs(
                        funds.outputs,
                        funds.sats,
                        OffsetDateTime::now_utc(),
                    );
                }
                Ok(funds)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspent_outputs_are_counted_with_the_unconfirmed_ones_apart() {
        let funds = ScriptFunds::of([(34_690, true), (14_691, true), (4_690, false)]);
        assert_eq!(
            funds,
            ScriptFunds {
                outputs: 3,
                sats: 54_071,
                unconfirmed_outputs: 1,
                unconfirmed_sats: 4_690,
            }
        );
        assert_eq!(ScriptFunds::of([]), ScriptFunds::default());
    }

    /// The address is the key itself as a taproot output key: its witness program is the
    /// key's x coordinate, untweaked, which is what `simple_sweep_tx` pays.
    #[test]
    fn the_address_is_the_coordinators_key_untweaked() {
        let key = dlctix::secp::Scalar::from_slice(&[7; 32])
            .unwrap()
            .base_point_mul();
        let script = p2tr_script_pubkey(key);
        assert!(script.is_p2tr());
        assert_eq!(script.as_bytes()[2..], key.serialize_xonly());
    }
}
