//! The VTXO a refund pays into, on its way to the player's Lightning Address.
//!
//! A competition that never kicks off refunds each escrow after `T`. The player runs no Ark
//! wallet, so the refund goes to a swap service, which pays the player's Lightning Address and
//! takes the VTXO in exchange. The swap is atomic: the service can spend this VTXO only with the
//! preimage of the invoice it paid, and the player takes it back if the service never pays.
//!
//! Each spend condition has a collaborative leaf, which the Arkade server co-signs.
//! It also has a unilateral leaf without the server, usable only once the VTXO is unrolled on chain.
//!
//! | Leaf                | Signers          | Condition                                  |
//! |---------------------|------------------|--------------------------------------------|
//! | `Claim`             | swapper, server  | the invoice preimage                       |
//! | `Reclaim`           | player, server   | locktime `D`                               |
//! | `UnilateralClaim`   | swapper          | the invoice preimage, and the exit delay   |
//! | `UnilateralReclaim` | player           | the unilateral reclaim delay               |
//!
//! The player learns the preimage when the invoice is paid, since the player is its payee, but
//! the claim leaves need the service's key as well, so knowing it takes nothing from the player.
//!
//! `D` is the deadline for the service to pay. Like the entry escrow, the player's solo leaf
//! cannot wait for `D` directly, because arkd refuses timelock opcodes inside a condition; it
//! waits a relative delay that ends after `D` plus the exit delay instead. Keymeld signs as the
//! player on both player leaves, from the same entry key that signed the refund.

use bitcoin::absolute::LockTime;
use bitcoin::opcodes::all::{OP_EQUALVERIFY, OP_SHA256};
use bitcoin::script::Builder;
use bitcoin::{ScriptBuf, XOnlyPublicKey};

use crate::{Error, EscrowTerms, RelativeTimelock, Tapscript, VtxoScript};

/// Everything that fixes a refund swap's script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapTerms {
    /// The entry key, held by Keymeld, which also signs the refund that pays this VTXO.
    pub player: XOnlyPublicKey,
    /// The swap service's key.
    pub swapper: XOnlyPublicKey,
    /// The Arkade server's signer key (`signerPubkey` in `/v1/info`).
    pub server: XOnlyPublicKey,
    /// SHA-256 of the invoice preimage, which is the invoice's payment hash.
    pub payment_hash: [u8; 32],
    /// `D`, from which the player can take the swap back.
    pub deadline: LockTime,
    /// The CSV delay on the unilateral claim leaf, at least the server's `unilateralExitDelay`.
    pub exit_delay: RelativeTimelock,
    /// The CSV delay on the player's solo leaf, longer than `exit_delay` and in the same unit.
    pub unilateral_reclaim_delay: RelativeTimelock,
}

impl SwapTerms {
    /// The shortest unilateral reclaim delay for a swap created at `created_at`, in UNIX seconds.
    ///
    /// As for an entry escrow, a player alone cannot spend before `D` plus `exit_delay`.
    pub fn unilateral_reclaim_delay_for(
        deadline: LockTime,
        exit_delay: RelativeTimelock,
        created_at: u32,
    ) -> Result<RelativeTimelock, Error> {
        EscrowTerms::unilateral_refund_delay_for(deadline, exit_delay, created_at)
    }
}

/// One spend condition of a refund swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapPath {
    Claim,
    Reclaim,
    UnilateralClaim,
    UnilateralReclaim,
}

impl SwapPath {
    fn index(self) -> usize {
        match self {
            SwapPath::Claim => 0,
            SwapPath::Reclaim => 1,
            SwapPath::UnilateralClaim => 2,
            SwapPath::UnilateralReclaim => 3,
        }
    }
}

/// The condition both claim leaves carry: the spender supplies the invoice preimage.
fn preimage_condition(payment_hash: [u8; 32]) -> ScriptBuf {
    Builder::new()
        .push_opcode(OP_SHA256)
        .push_slice(payment_hash)
        .push_opcode(OP_EQUALVERIFY)
        .into_script()
}

/// A refund swap's VTXO script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundSwap {
    terms: SwapTerms,
    tapscripts: [Tapscript; 4],
    vtxo: VtxoScript,
}

impl RefundSwap {
    pub fn new(terms: SwapTerms) -> Result<Self, Error> {
        let SwapTerms {
            player,
            swapper,
            server,
            payment_hash,
            deadline,
            exit_delay,
            unilateral_reclaim_delay,
        } = terms;
        if player == swapper || player == server || swapper == server {
            return Err(Error::Swap(
                "player, swapper, and server keys must differ".into(),
            ));
        }
        if payment_hash == [0u8; 32] {
            return Err(Error::Swap("the payment hash must be set".into()));
        }
        if deadline.to_consensus_u32() == 0 {
            return Err(Error::Swap("the swap deadline must be set".into()));
        }
        if exit_delay.to_sequence()?.to_consensus_u32() == 0 {
            return Err(Error::Swap("the exit delay must be positive".into()));
        }
        let longer = match (exit_delay, unilateral_reclaim_delay) {
            (RelativeTimelock::Blocks(exit), RelativeTimelock::Blocks(reclaim)) => reclaim > exit,
            (RelativeTimelock::Seconds(exit), RelativeTimelock::Seconds(reclaim)) => reclaim > exit,
            _ => {
                return Err(Error::Swap(
                    "the exit and unilateral reclaim delays must use the same unit".into(),
                ))
            }
        };
        if !longer {
            return Err(Error::Swap(
                "the unilateral reclaim delay must be longer than the exit delay".into(),
            ));
        }

        let condition = preimage_condition(payment_hash);
        let tapscripts = [
            Tapscript::ConditionMultisig {
                condition: condition.clone(),
                pubkeys: vec![swapper, server],
            },
            Tapscript::CltvMultisig {
                locktime: deadline,
                pubkeys: vec![player, server],
            },
            Tapscript::ConditionCsvMultisig {
                condition,
                timelock: exit_delay,
                pubkeys: vec![swapper],
            },
            Tapscript::CsvMultisig {
                timelock: unilateral_reclaim_delay,
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
    /// Fails unless the leaves are exactly a refund swap, in order.
    pub fn from_vtxo_script(vtxo: &VtxoScript) -> Result<Self, Error> {
        let mismatch = || Error::Swap("the leaves are not a refund swap".into());
        let [claim, reclaim, unilateral_claim, unilateral_reclaim] = vtxo.scripts() else {
            return Err(mismatch());
        };
        let Tapscript::ConditionMultisig {
            condition,
            pubkeys: claim_keys,
        } = Tapscript::decode(claim)?
        else {
            return Err(mismatch());
        };
        let [swapper, server] = claim_keys[..] else {
            return Err(mismatch());
        };
        let payment_hash = payment_hash_of(&condition).ok_or_else(mismatch)?;
        let Tapscript::CltvMultisig {
            locktime: deadline,
            pubkeys: reclaim_keys,
        } = Tapscript::decode(reclaim)?
        else {
            return Err(mismatch());
        };
        let [player, _] = reclaim_keys[..] else {
            return Err(mismatch());
        };
        let Tapscript::ConditionCsvMultisig {
            timelock: exit_delay,
            ..
        } = Tapscript::decode(unilateral_claim)?
        else {
            return Err(mismatch());
        };
        let Tapscript::CsvMultisig {
            timelock: unilateral_reclaim_delay,
            ..
        } = Tapscript::decode(unilateral_reclaim)?
        else {
            return Err(mismatch());
        };
        let swap = Self::new(SwapTerms {
            player,
            swapper,
            server,
            payment_hash,
            deadline,
            exit_delay,
            unilateral_reclaim_delay,
        })?;
        if swap.vtxo.scripts() == vtxo.scripts() {
            Ok(swap)
        } else {
            Err(mismatch())
        }
    }

    pub fn terms(&self) -> &SwapTerms {
        &self.terms
    }

    pub fn script(&self, path: SwapPath) -> &ScriptBuf {
        &self.vtxo.scripts()[path.index()]
    }

    pub fn vtxo_script(&self) -> &VtxoScript {
        &self.vtxo
    }

    pub fn address(&self, hrp: &str) -> Result<crate::ArkAddress, Error> {
        self.vtxo.address(hrp, self.terms.server)
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        self.vtxo.script_pubkey()
    }
}

/// The payment hash a claim condition commits to, if it is one.
fn payment_hash_of(condition: &ScriptBuf) -> Option<[u8; 32]> {
    let mut instructions = condition.instructions();
    match (
        instructions.next()?.ok()?.opcode(),
        instructions.next()?.ok()?.push_bytes(),
        instructions.next()?.ok()?.opcode(),
        instructions.next(),
    ) {
        (Some(OP_SHA256), Some(hash), Some(OP_EQUALVERIFY), None) => {
            hash.as_bytes().try_into().ok()
        }
        _ => None,
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

    fn terms() -> SwapTerms {
        let deadline = LockTime::from_time(1_800_000_000).unwrap();
        let exit_delay = RelativeTimelock::Seconds(512 * 10);
        SwapTerms {
            player: key(1),
            swapper: key(2),
            server: key(3),
            payment_hash: [7u8; 32],
            deadline,
            exit_delay,
            unilateral_reclaim_delay: SwapTerms::unilateral_reclaim_delay_for(
                deadline,
                exit_delay,
                1_799_000_000,
            )
            .unwrap(),
        }
    }

    #[test]
    fn every_leaf_has_a_valid_control_block() {
        let swap = RefundSwap::new(terms()).unwrap();
        let vtxo = swap.vtxo_script();
        for leaf in vtxo.scripts() {
            assert!(vtxo.control_block(leaf).unwrap().verify_taproot_commitment(
                &Secp256k1::verification_only(),
                vtxo.output_key().to_x_only_public_key(),
                leaf,
            ));
        }
    }

    #[test]
    fn the_terms_survive_a_round_trip_through_the_leaves() {
        let swap = RefundSwap::new(terms()).unwrap();
        let recovered = RefundSwap::from_vtxo_script(swap.vtxo_script()).unwrap();
        assert_eq!(recovered, swap);
        assert_eq!(recovered.terms().payment_hash, [7u8; 32]);
    }

    #[test]
    fn only_the_claim_leaves_carry_the_preimage_condition() {
        let swap = RefundSwap::new(terms()).unwrap();
        let condition = preimage_condition([7u8; 32]);
        let leaf = |path: SwapPath| Tapscript::decode(swap.script(path)).unwrap();
        assert!(matches!(
            leaf(SwapPath::Claim),
            Tapscript::ConditionMultisig { condition: c, .. } if c == condition
        ));
        assert!(matches!(
            leaf(SwapPath::UnilateralClaim),
            Tapscript::ConditionCsvMultisig { condition: c, .. } if c == condition
        ));
        assert!(matches!(
            leaf(SwapPath::Reclaim),
            Tapscript::CltvMultisig { .. }
        ));
        assert!(matches!(
            leaf(SwapPath::UnilateralReclaim),
            Tapscript::CsvMultisig { .. }
        ));
    }

    #[test]
    fn the_player_alone_waits_past_the_deadline() {
        let swap = RefundSwap::new(terms()).unwrap();
        let (RelativeTimelock::Seconds(exit), RelativeTimelock::Seconds(reclaim)) = (
            swap.terms().exit_delay,
            swap.terms().unilateral_reclaim_delay,
        ) else {
            panic!("these terms use seconds");
        };
        assert!(reclaim > exit);
        // Counted from creation, the player's solo leaf opens after the deadline and the exit delay.
        assert!(u64::from(reclaim) + 1_799_000_000 >= 1_800_000_000 + u64::from(exit));
    }

    #[test]
    fn the_leaves_must_be_a_refund_swap() {
        let swap = RefundSwap::new(terms()).unwrap();
        let entry = crate::EntryEscrow::new(EscrowTerms {
            player: key(1),
            coordinator: key(2),
            server: key(3),
            refund_locktime: swap.terms().deadline,
            exit_delay: swap.terms().exit_delay,
            unilateral_refund_delay: swap.terms().unilateral_reclaim_delay,
        })
        .unwrap();
        assert!(RefundSwap::from_vtxo_script(entry.vtxo_script()).is_err());
    }

    #[test]
    fn distinct_keys_and_a_set_payment_hash_are_required() {
        let same_key = SwapTerms {
            swapper: key(1),
            ..terms()
        };
        assert!(RefundSwap::new(same_key).is_err());
        let unset_hash = SwapTerms {
            payment_hash: [0u8; 32],
            ..terms()
        };
        assert!(RefundSwap::new(unset_hash).is_err());
    }
}
