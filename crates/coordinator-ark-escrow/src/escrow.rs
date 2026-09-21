//! The escrow VTXO holding one competition entry's buy-in until kickoff.
//!
//! Each spend condition has a collaborative leaf, which the Arkade server co-signs.
//! It also has a unilateral leaf without the server, usable only after the exit delay.
//!
//! | Leaf                | Signers                      | Condition                    |
//! |---------------------|------------------------------|------------------------------|
//! | `Funding`           | player, coordinator, server  | none (the kickoff batch)     |
//! | `Refund`            | player, server               | locktime `T`                 |
//! | `UnilateralFunding` | player, coordinator          | exit delay                   |
//! | `UnilateralRefund`  | player                       | locktime `T` and exit delay  |
//!
//! Keymeld signs as the player, from the entry key deposited at registration.
//! Each entry has its own player key, so every escrow address is unique without a nonce leaf.

use bitcoin::absolute::LockTime;
use bitcoin::taproot::ControlBlock;
use bitcoin::{ScriptBuf, XOnlyPublicKey};

use crate::tapscript::cltv_condition;
use crate::{ArkAddress, Error, RelativeTimelock, Tapscript, VtxoScript};

/// Everything that fixes an entry's escrow script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscrowTerms {
    /// The entry key, held by Keymeld from the deposit.
    pub player: XOnlyPublicKey,
    /// The coordinator's escrow key.
    pub coordinator: XOnlyPublicKey,
    /// The Arkade server's signer key (`signerPubkey` in `/v1/info`).
    pub server: XOnlyPublicKey,
    /// `T`, from which the player can take the escrow back.
    pub refund_locktime: LockTime,
    /// The CSV delay on the unilateral leaves, at least the server's `unilateralExitDelay`.
    pub exit_delay: RelativeTimelock,
}

/// A spend path through the escrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EscrowPath {
    Funding,
    Refund,
    UnilateralFunding,
    UnilateralRefund,
}

impl EscrowPath {
    /// Every path, in leaf order.
    pub const ALL: [EscrowPath; 4] = [
        EscrowPath::Funding,
        EscrowPath::Refund,
        EscrowPath::UnilateralFunding,
        EscrowPath::UnilateralRefund,
    ];

    fn index(self) -> usize {
        match self {
            EscrowPath::Funding => 0,
            EscrowPath::Refund => 1,
            EscrowPath::UnilateralFunding => 2,
            EscrowPath::UnilateralRefund => 3,
        }
    }
}

/// An entry's escrow VTXO script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryEscrow {
    terms: EscrowTerms,
    tapscripts: [Tapscript; 4],
    vtxo: VtxoScript,
}

impl EntryEscrow {
    pub fn new(terms: EscrowTerms) -> Result<Self, Error> {
        let EscrowTerms {
            player,
            coordinator,
            server,
            refund_locktime,
            exit_delay,
        } = terms;
        if player == coordinator || player == server || coordinator == server {
            return Err(Error::Escrow(
                "player, coordinator, and server keys must differ".into(),
            ));
        }
        if refund_locktime.to_consensus_u32() == 0 {
            return Err(Error::Escrow("the refund locktime must be set".into()));
        }
        if exit_delay.to_sequence()?.to_consensus_u32() == 0 {
            return Err(Error::Escrow("the exit delay must be positive".into()));
        }

        let tapscripts = [
            Tapscript::Multisig {
                pubkeys: vec![player, coordinator, server],
            },
            Tapscript::CltvMultisig {
                locktime: refund_locktime,
                pubkeys: vec![player, server],
            },
            Tapscript::CsvMultisig {
                timelock: exit_delay,
                pubkeys: vec![player, coordinator],
            },
            Tapscript::ConditionCsvMultisig {
                condition: cltv_condition(refund_locktime),
                timelock: exit_delay,
                pubkeys: vec![player],
            },
        ];
        let vtxo = VtxoScript::from_tapscripts(&tapscripts)?;
        Ok(Self {
            terms,
            tapscripts,
            vtxo,
        })
    }

    /// Recover the terms from a VTXO's leaves.
    ///
    /// Fails unless the leaves are exactly an entry escrow, in order.
    pub fn from_vtxo_script(vtxo: &VtxoScript) -> Result<Self, Error> {
        let mismatch = || Error::Escrow("the leaves are not an entry escrow".into());
        let [funding, refund, unilateral_funding, _] = vtxo.scripts() else {
            return Err(mismatch());
        };
        let Tapscript::Multisig { pubkeys } = Tapscript::decode(funding)? else {
            return Err(mismatch());
        };
        let [player, coordinator, server] = pubkeys[..] else {
            return Err(mismatch());
        };
        let Tapscript::CltvMultisig { locktime, .. } = Tapscript::decode(refund)? else {
            return Err(mismatch());
        };
        let Tapscript::CsvMultisig { timelock, .. } = Tapscript::decode(unilateral_funding)? else {
            return Err(mismatch());
        };
        let escrow = Self::new(EscrowTerms {
            player,
            coordinator,
            server,
            refund_locktime: locktime,
            exit_delay: timelock,
        })?;
        if escrow.vtxo.scripts() == vtxo.scripts() {
            Ok(escrow)
        } else {
            Err(mismatch())
        }
    }

    pub fn terms(&self) -> &EscrowTerms {
        &self.terms
    }

    pub fn tapscript(&self, path: EscrowPath) -> &Tapscript {
        &self.tapscripts[path.index()]
    }

    pub fn script(&self, path: EscrowPath) -> &ScriptBuf {
        &self.vtxo.scripts()[path.index()]
    }

    pub fn control_block(&self, path: EscrowPath) -> ControlBlock {
        self.vtxo
            .control_block(self.script(path))
            .expect("every escrow leaf is in the tree")
    }

    pub fn vtxo_script(&self) -> &VtxoScript {
        &self.vtxo
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        self.vtxo.script_pubkey()
    }

    pub fn address(&self, hrp: &str) -> Result<ArkAddress, Error> {
        self.vtxo.address(hrp, self.terms.server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::key::Secp256k1;

    fn key(byte: u8) -> XOnlyPublicKey {
        let secp = Secp256k1::new();
        let secret = bitcoin::secp256k1::SecretKey::from_slice(&[byte; 32]).unwrap();
        secret.x_only_public_key(&secp).0
    }

    fn terms() -> EscrowTerms {
        EscrowTerms {
            player: key(1),
            coordinator: key(2),
            server: key(3),
            refund_locktime: LockTime::from_consensus(3_444_600),
            exit_delay: RelativeTimelock::Seconds(2048),
        }
    }

    #[test]
    fn leaves_decode_to_their_closures() {
        let escrow = EntryEscrow::new(terms()).unwrap();
        for path in EscrowPath::ALL {
            assert_eq!(
                &Tapscript::decode(escrow.script(path)).unwrap(),
                escrow.tapscript(path)
            );
        }
    }

    #[test]
    fn every_path_has_a_valid_control_block() {
        let escrow = EntryEscrow::new(terms()).unwrap();
        let secp = Secp256k1::verification_only();
        for path in EscrowPath::ALL {
            assert!(escrow.control_block(path).verify_taproot_commitment(
                &secp,
                escrow.vtxo_script().tweaked_key(),
                escrow.script(path),
            ));
        }
    }

    #[test]
    fn terms_round_trip_through_the_tap_tree() {
        let escrow = EntryEscrow::new(terms()).unwrap();
        let vtxo = VtxoScript::decode_tap_tree(&escrow.vtxo_script().encode_tap_tree()).unwrap();
        assert_eq!(EntryEscrow::from_vtxo_script(&vtxo).unwrap(), escrow);
    }

    #[test]
    fn a_different_escrow_is_not_accepted() {
        let escrow = EntryEscrow::new(terms()).unwrap();
        let mut scripts = escrow.vtxo_script().scripts().to_vec();
        scripts.swap(1, 2);
        let swapped = VtxoScript::new(scripts).unwrap();
        assert!(EntryEscrow::from_vtxo_script(&swapped).is_err());

        let other = EntryEscrow::new(EscrowTerms {
            refund_locktime: LockTime::from_consensus(3_444_601),
            ..terms()
        })
        .unwrap();
        let mut spliced = escrow.vtxo_script().scripts().to_vec();
        spliced[3] = other.script(EscrowPath::UnilateralRefund).clone();
        let spliced = VtxoScript::new(spliced).unwrap();
        assert!(EntryEscrow::from_vtxo_script(&spliced).is_err());
    }

    #[test]
    fn keys_must_differ_and_locks_must_be_set() {
        assert!(EntryEscrow::new(EscrowTerms {
            coordinator: key(1),
            ..terms()
        })
        .is_err());
        assert!(EntryEscrow::new(EscrowTerms {
            refund_locktime: LockTime::ZERO,
            ..terms()
        })
        .is_err());
        assert!(EntryEscrow::new(EscrowTerms {
            exit_delay: RelativeTimelock::Blocks(0),
            ..terms()
        })
        .is_err());
    }
}
