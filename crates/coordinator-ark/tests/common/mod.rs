#![allow(dead_code)]

//! The kickoff test fixture: three players, a coordinator, and a server, run by `testing::MockArkd`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ark_core::server::Info;
use async_trait::async_trait;
use bitcoin::hashes::Hash;
use bitcoin::key::Keypair;
use bitcoin::{Amount, OutPoint, Psbt, TxOut, Txid};
pub use coordinator_ark::testing::*;
use coordinator_ark::{
    escrow_terms, BoxError, EscrowInput, KeypairSigner, KickoffConfig, KickoffHooks, PoolFunding,
};
use coordinator_ark_escrow::{EntryEscrow, RelativeTimelock, ServerRules};

pub const ESCROW_SATS: u64 = 10_000;
pub const REFUND_AT: u32 = 1_790_000_000;
pub const CREATED_AT: u32 = 1_789_900_000;

/// Three players, a coordinator, and a server, with escrows the server accepts.
pub struct Fixture {
    pub info: Info,
    pub rules: ServerRules,
    pub players: Vec<Keypair>,
    pub coordinator: Keypair,
    pub pool: PoolFunding,
}

impl Fixture {
    pub fn new() -> Self {
        let server = keypair(9);
        let info = mock_info(&server);
        let rules = coordinator_ark::server_rules(&info).unwrap();
        assert_eq!(rules.min_exit_delay, RelativeTimelock::Seconds(2048));
        let coordinator = keypair(8);
        let players = vec![keypair(1), keypair(2), keypair(3)];
        let inputs = players
            .iter()
            .enumerate()
            .map(|(index, player)| {
                let terms = escrow_terms(
                    &rules,
                    xonly(player),
                    xonly(&coordinator),
                    REFUND_AT,
                    CREATED_AT,
                )
                .unwrap();
                EscrowInput {
                    escrow: EntryEscrow::new(terms).unwrap(),
                    outpoint: OutPoint::new(Txid::from_byte_array([index as u8 + 1; 32]), 0),
                    amount: Amount::from_sat(ESCROW_SATS),
                }
            })
            .collect();
        let funding_output = TxOut {
            value: Amount::from_sat(3 * ESCROW_SATS),
            script_pubkey: p2tr(50),
        };
        let pool = PoolFunding::new(inputs, funding_output, &rules, info.dust).unwrap();
        Self {
            info,
            rules,
            players,
            coordinator,
            pool,
        }
    }

    pub fn player_signer(&self) -> KeypairSigner {
        KeypairSigner::new(self.players.iter().copied())
    }

    pub fn coordinator_signer(&self) -> KeypairSigner {
        KeypairSigner::new([self.coordinator])
    }

    pub fn config() -> KickoffConfig {
        KickoffConfig {
            intent_lifetime: Duration::from_secs(120),
            timeout: Duration::from_secs(5),
        }
    }
}

/// Records the hook call, and whether any forfeit had been submitted by then.
pub struct Hooks {
    pub fail: bool,
    pub arkd: Arc<MockArkd>,
    pub calls: Mutex<Vec<(OutPoint, usize)>>,
}

impl Hooks {
    pub fn new(arkd: &Arc<MockArkd>, fail: bool) -> Self {
        Self {
            fail,
            arkd: arkd.clone(),
            calls: Mutex::default(),
        }
    }
}

#[async_trait]
impl KickoffHooks for Hooks {
    async fn before_forfeits(
        &self,
        funding: OutPoint,
        commitment_tx: &Psbt,
    ) -> Result<(), BoxError> {
        assert_eq!(funding.txid, commitment_tx.unsigned_tx.compute_txid());
        let forfeits = self.arkd.forfeits().len();
        self.calls.lock().unwrap().push((funding, forfeits));
        if self.fail {
            Err("keymeld could not sign the refund transaction".into())
        } else {
            Ok(())
        }
    }
}
