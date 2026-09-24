//! The checks an Arkade server applies to a VTXO script before it accepts the VTXO in an intent.
//!
//! This mirrors `TapscriptsVtxoScript.Validate` in arkd (`pkg/ark-lib/script/vtxo_script.go`).
//! The coordinator runs it before handing out an escrow address.
//! Keymeld runs it before accepting a deposit.
//! An escrow the server would reject could only leave through the unilateral paths.

use bitcoin::XOnlyPublicKey;

use crate::{Error, RelativeTimelock, Tapscript, VtxoScript};

/// What an Arkade server accepts in a VTXO script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerRules {
    /// The server's signer key (`signerPubkey` in `/v1/info`).
    pub signer: XOnlyPublicKey,
    /// The shortest CSV delay allowed on an exit leaf (`unilateralExitDelay`).
    pub min_exit_delay: RelativeTimelock,
    /// Whether block-based timelocks are accepted.
    ///
    /// arkd allows them only when its VTXO tree expiry is in blocks.
    /// Timestamps and second-based delays are accepted either way.
    pub block_timelocks_allowed: bool,
}

impl ServerRules {
    /// Check `vtxo` the way the server will when it is spent in an intent.
    ///
    /// - Every leaf without a CSV delay is a forfeit leaf, and must include the server's key.
    /// - Every leaf with a CSV delay is an exit leaf, and must wait at least `min_exit_delay`.
    /// - Block-based timelocks must be allowed if either kind uses one.
    pub fn check(&self, vtxo: &VtxoScript) -> Result<(), Error> {
        let reject = |reason: String| Err(Error::ServerRules(reason));
        let mut smallest_exit: Option<RelativeTimelock> = None;
        for (index, script) in vtxo.scripts().iter().enumerate() {
            let tapscript = Tapscript::decode(script)?;
            let exit_delay = match &tapscript {
                Tapscript::Multisig { .. } | Tapscript::ConditionMultisig { .. } => None,
                Tapscript::CltvMultisig { locktime, .. } => {
                    if locktime.is_block_height() && !self.block_timelocks_allowed {
                        return reject(format!("leaf {index} has a block-height CLTV"));
                    }
                    None
                }
                Tapscript::CsvMultisig { timelock, .. }
                | Tapscript::ConditionCsvMultisig { timelock, .. } => Some(*timelock),
            };
            match exit_delay {
                None => {
                    if !tapscript.pubkeys().contains(&self.signer) {
                        return reject(format!("forfeit leaf {index} lacks the server key"));
                    }
                }
                Some(delay) => {
                    if matches!(delay, RelativeTimelock::Blocks(_)) && !self.block_timelocks_allowed
                    {
                        return reject(format!("exit leaf {index} has a block-based CSV"));
                    }
                    if smallest_exit
                        .is_none_or(|smallest| arkd_seconds(delay) < arkd_seconds(smallest))
                    {
                        smallest_exit = Some(delay);
                    }
                }
            }
        }
        match smallest_exit {
            Some(delay) if arkd_seconds(delay) < arkd_seconds(self.min_exit_delay) => {
                reject(format!(
                    "exit delay {delay:?} is shorter than the server's {:?}",
                    self.min_exit_delay
                ))
            }
            _ => Ok(()),
        }
    }
}

/// A relative timelock in arkd's comparison units.
///
/// arkd compares delays in seconds, and counts a block as one second (`SECONDS_PER_BLOCK = 1`).
fn arkd_seconds(timelock: RelativeTimelock) -> u32 {
    match timelock {
        RelativeTimelock::Blocks(blocks) => u32::from(blocks),
        RelativeTimelock::Seconds(seconds) => seconds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryEscrow, EscrowTerms};
    use bitcoin::absolute::LockTime;
    use bitcoin::key::Secp256k1;

    fn key(byte: u8) -> XOnlyPublicKey {
        let secp = Secp256k1::new();
        let secret = bitcoin::secp256k1::SecretKey::from_slice(&[byte; 32]).unwrap();
        secret.x_only_public_key(&secp).0
    }

    fn rules() -> ServerRules {
        ServerRules {
            signer: key(3),
            min_exit_delay: RelativeTimelock::Seconds(2048),
            block_timelocks_allowed: false,
        }
    }

    fn escrow(refund_locktime: LockTime, exit_delay: RelativeTimelock) -> EntryEscrow {
        let unilateral_refund_delay = match exit_delay {
            RelativeTimelock::Blocks(blocks) => RelativeTimelock::Blocks(blocks + 1008),
            RelativeTimelock::Seconds(seconds) => RelativeTimelock::Seconds(seconds + 512 * 1008),
        };
        EntryEscrow::new(EscrowTerms {
            player: key(1),
            coordinator: key(2),
            server: key(3),
            refund_locktime,
            exit_delay,
            unilateral_refund_delay,
        })
        .unwrap()
    }

    const KICKOFF_PLUS_A_DAY: u32 = 1_790_000_000;

    #[test]
    fn a_timestamp_escrow_with_the_server_delay_is_accepted() {
        let escrow = escrow(
            LockTime::from_consensus(KICKOFF_PLUS_A_DAY),
            RelativeTimelock::Seconds(2048),
        );
        rules().check(escrow.vtxo_script()).unwrap();
    }

    #[test]
    fn block_timelocks_need_a_block_based_server() {
        let height = escrow(
            LockTime::from_consensus(3_444_600),
            RelativeTimelock::Seconds(2048),
        );
        assert!(rules().check(height.vtxo_script()).is_err());

        let blocks = escrow(
            LockTime::from_consensus(KICKOFF_PLUS_A_DAY),
            RelativeTimelock::Blocks(2048),
        );
        assert!(rules().check(blocks.vtxo_script()).is_err());

        let block_server = ServerRules {
            block_timelocks_allowed: true,
            ..rules()
        };
        block_server.check(height.vtxo_script()).unwrap();
        block_server.check(blocks.vtxo_script()).unwrap();
    }

    #[test]
    fn a_short_exit_delay_is_rejected() {
        let escrow = escrow(
            LockTime::from_consensus(KICKOFF_PLUS_A_DAY),
            RelativeTimelock::Seconds(1536),
        );
        assert!(rules().check(escrow.vtxo_script()).is_err());
    }

    #[test]
    fn another_servers_escrow_is_rejected() {
        let escrow = escrow(
            LockTime::from_consensus(KICKOFF_PLUS_A_DAY),
            RelativeTimelock::Seconds(2048),
        );
        let other_server = ServerRules {
            signer: key(4),
            ..rules()
        };
        assert!(other_server.check(escrow.vtxo_script()).is_err());
    }
}
