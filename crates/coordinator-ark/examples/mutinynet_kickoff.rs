//! A pool kickoff on Arkade's Mutinynet server.
//! Escrow VTXOs are spent in one batch into a dlctix funding output, and the contract is signed before any forfeit.
//!
//! ```text
//! cargo run -p coordinator-ark --example mutinynet_kickoff -- escrows STATE.json [PLAYERS] [SATS]
//! # Pay each escrow address, for example with ark-client-sample's send-to-ark-addresses.
//! cargo run -p coordinator-ark --example mutinynet_kickoff -- status STATE.json
//! cargo run -p coordinator-ark --example mutinynet_kickoff -- kickoff STATE.json
//! cargo run -p coordinator-ark --example mutinynet_kickoff -- expire STATE.json
//! ```
//!
//! Every key is local and generated into `STATE.json`, so use test coins only.
//! The players' keys stand in for Keymeld's, and the coordinator's key is the market maker's.
//! `expire` broadcasts the contract's pre-signed expiry transaction once its height passes.
//! That proves the players' way back out of the funding output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Amount, FeeRate, OutPoint, Psbt};
use coordinator_ark::{
    fund_pool, ArkServer, BoxError, ContractSigner, DlcKickoff, EscrowInput, KeypairSigner,
    KickoffConfig, LocalContractSigner, PoolFunding,
};
use coordinator_ark_escrow::EntryEscrow;
use dlctix::secp::{Point, Scalar};
use dlctix::{
    attestation_locking_point, hashlock, ContractParameters, ContractSignatures,
    EventLockingConditions, MarketMaker, Outcome, Player, SignedContract, TicketedDLC,
};
use serde::{Deserialize, Serialize};

const SERVER: &str = "https://mutinynet.arkade.sh";
const ESPLORA: &str = "https://mutinynet.com/api";
/// Mutinynet mines a block about every 30 seconds, so this is about three minutes.
const EXPIRY_BLOCKS: u32 = 6;
const REFUND_AFTER_SECS: u32 = 2 * 24 * 60 * 60;

#[derive(Serialize, Deserialize)]
struct State {
    /// Hex secret keys. Test coins only.
    players: Vec<String>,
    coordinator: String,
    oracle: String,
    oracle_nonce: String,
    escrow_sats: u64,
    created_at: u32,
    refund_at: u32,
    #[serde(default)]
    kickoff: Option<KickoffRecord>,
}

#[derive(Serialize, Deserialize)]
struct KickoffRecord {
    batch_id: String,
    commitment_txid: String,
    funding: String,
    expiry_height: u32,
    seconds_in_batch: f64,
}

impl State {
    fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    fn player_keys(&self) -> anyhow::Result<Vec<Keypair>> {
        self.players.iter().map(|hex| keypair(hex)).collect()
    }

    fn escrows(&self, server: &ArkServer) -> anyhow::Result<Vec<EntryEscrow>> {
        let coordinator = keypair(&self.coordinator)?.x_only_public_key().0;
        self.player_keys()?
            .iter()
            .map(|player| {
                let terms = server.escrow_terms(
                    player.x_only_public_key().0,
                    coordinator,
                    self.refund_at,
                    self.created_at,
                )?;
                Ok(server.entry_escrow(terms)?)
            })
            .collect()
    }
}

fn keypair(hex: &str) -> anyhow::Result<Keypair> {
    let secret = SecretKey::from_slice(&hex::decode(hex)?)?;
    Ok(Keypair::from_secret_key(&Secp256k1::new(), &secret))
}

fn scalar(keypair: &Keypair) -> Scalar {
    Scalar::from_slice(&keypair.secret_bytes()).expect("a secp256k1 secret key is a valid scalar")
}

fn fresh_secret() -> String {
    hex::encode(SecretKey::new(&mut bitcoin::secp256k1::rand::thread_rng()).secret_bytes())
}

fn now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as u32
}

async fn tip_height(http: &reqwest::Client) -> anyhow::Result<u32> {
    let body = http
        .get(format!("{ESPLORA}/blocks/tip/height"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(body.trim().parse()?)
}

/// The pool's contract: each player wins one outcome, and expiry splits the pot evenly.
fn contract(
    state: &State,
    funding_value: Amount,
    expiry_height: u32,
) -> anyhow::Result<ContractParameters> {
    let oracle = scalar(&keypair(&state.oracle)?).base_point_mul();
    let nonce = scalar(&keypair(&state.oracle_nonce)?).base_point_mul();
    let players: Vec<Player> = state
        .player_keys()?
        .iter()
        .map(|key| {
            let secret = key.secret_bytes();
            Player {
                pubkey: scalar(key).base_point_mul(),
                ticket_hash: hashlock::sha256(&hashlock::sha256(
                    &[b"ticket".as_slice(), &secret].concat(),
                )),
                payout_hash: hashlock::sha256(&hashlock::sha256(
                    &[b"payout".as_slice(), &secret].concat(),
                )),
            }
        })
        .collect();
    let mut outcome_payouts: BTreeMap<Outcome, BTreeMap<usize, u64>> = (0..players.len())
        .map(|index| (Outcome::Attestation(index), BTreeMap::from([(index, 1)])))
        .collect();
    outcome_payouts.insert(
        Outcome::Expiry,
        (0..players.len()).map(|index| (index, 1)).collect(),
    );
    let coordinator: Point = scalar(&keypair(&state.coordinator)?).base_point_mul();
    Ok(ContractParameters {
        market_maker: MarketMaker {
            pubkey: coordinator,
        },
        event: EventLockingConditions {
            locking_points: (0..players.len())
                .map(|index| {
                    attestation_locking_point(oracle, nonce, format!("player {index} wins"))
                })
                .collect(),
            expiry: Some(expiry_height),
        },
        players,
        outcome_payouts,
        fee_rate: FeeRate::from_sat_per_vb_u32(2),
        funding_value,
        relative_locktime_block_delta: 2,
    })
}

/// Signs with every local key, and writes the contract next to the state before any forfeit.
struct SaveBeforeForfeits {
    signer: LocalContractSigner,
    path: PathBuf,
}

#[async_trait]
impl ContractSigner for SaveBeforeForfeits {
    async fn sign_contract(
        &self,
        dlc: &TicketedDLC,
        commitment_tx: &Psbt,
    ) -> Result<ContractSignatures, BoxError> {
        let started = Instant::now();
        let signatures = self.signer.sign_contract(dlc, commitment_tx).await?;
        println!(
            "signed {} outcome and {} split transactions in {:.2}s",
            signatures.outcome_tx_signatures.len(),
            signatures.split_tx_signatures.len(),
            started.elapsed().as_secs_f64()
        );
        Ok(signatures)
    }

    async fn keep(&self, contract: &SignedContract, _: &Psbt) -> Result<(), BoxError> {
        std::fs::write(&self.path, serde_json::to_string(contract)?)?;
        println!("saved the signed contract to {}", self.path.display());
        Ok(())
    }
}

fn contract_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("contract.json")
}

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("{} {}: {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log::set_logger(&StderrLogger).expect("one logger");
    log::set_max_level(log::LevelFilter::Info);

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, path) = match args.as_slice() {
        [command, path, ..] => (command.as_str(), PathBuf::from(path)),
        _ => anyhow::bail!("usage: mutinynet_kickoff escrows|status|kickoff|expire STATE.json"),
    };
    let server = ArkServer::connect(SERVER).await?;
    let http = reqwest::Client::new();

    match command {
        "escrows" => {
            let state = if path.exists() {
                State::load(&path)?
            } else {
                let players: usize = args.get(2).map(|n| n.parse()).transpose()?.unwrap_or(3);
                let escrow_sats: u64 = args
                    .get(3)
                    .map(|n| n.parse())
                    .transpose()?
                    .unwrap_or(20_000);
                let created_at = now();
                let state = State {
                    players: (0..players).map(|_| fresh_secret()).collect(),
                    coordinator: fresh_secret(),
                    oracle: fresh_secret(),
                    oracle_nonce: fresh_secret(),
                    escrow_sats,
                    created_at,
                    refund_at: created_at + REFUND_AFTER_SECS,
                    kickoff: None,
                };
                state.save(&path)?;
                state
            };
            let escrows = state.escrows(&server)?;
            let payments: Vec<String> = escrows
                .iter()
                .map(|escrow| {
                    Ok(format!(
                        "{}:{}",
                        escrow.address(server.hrp())?.encode(),
                        state.escrow_sats
                    ))
                })
                .collect::<anyhow::Result<_>>()?;
            println!("{}", payments.join(","));
        }
        "status" => {
            let state = State::load(&path)?;
            let escrows = state.escrows(&server)?;
            for (escrow, vtxos) in escrows.iter().zip(per_escrow(&server, &escrows).await?) {
                println!("{}", escrow.address(server.hrp())?.encode());
                for vtxo in vtxos {
                    println!(
                        "  {} {} spent={} swept={} preconfirmed={} expires_at={}",
                        vtxo.outpoint,
                        vtxo.amount,
                        vtxo.is_spent,
                        vtxo.is_swept,
                        vtxo.is_preconfirmed,
                        vtxo.expires_at
                    );
                }
            }
        }
        "kickoff" => {
            let mut state = State::load(&path)?;
            anyhow::ensure!(state.kickoff.is_none(), "this state already kicked off");
            let escrows = state.escrows(&server)?;
            let vtxos = per_escrow(&server, &escrows).await?;
            let mut inputs = Vec::new();
            for (escrow, vtxos) in escrows.into_iter().zip(vtxos) {
                let vtxo = vtxos
                    .into_iter()
                    .find(|vtxo| !vtxo.is_spent && !vtxo.is_swept)
                    .ok_or_else(|| {
                        anyhow::anyhow!("an escrow has no unspent VTXO; pay it first")
                    })?;
                inputs.push(EscrowInput {
                    escrow,
                    outpoint: vtxo.outpoint,
                    amount: vtxo.amount,
                });
            }
            let total: Amount = inputs.iter().map(|input| input.amount).sum();
            let expiry_height = tip_height(&http).await? + EXPIRY_BLOCKS;
            let hooks = DlcKickoff::new(
                contract(&state, total, expiry_height)?,
                SaveBeforeForfeits {
                    signer: LocalContractSigner::new(
                        scalar(&keypair(&state.coordinator)?),
                        state.player_keys()?.iter().map(scalar),
                    ),
                    path: contract_path(&path),
                },
            )?;
            let pool = PoolFunding::new(
                inputs,
                hooks.funding_output().clone(),
                server.rules(),
                server.info().dust,
            )?;
            println!(
                "kicking off {} escrows worth {total} into {} (expiry at height {expiry_height})",
                pool.inputs().len(),
                hooks.funding_output().script_pubkey
            );

            let started = Instant::now();
            let kickoff = fund_pool(
                server.client(),
                server.info(),
                &pool,
                &KeypairSigner::new(state.player_keys()?),
                &KeypairSigner::new([keypair(&state.coordinator)?]),
                &hooks,
                &KickoffConfig::for_server(server.info()),
            )
            .await?;
            let seconds_in_batch = started.elapsed().as_secs_f64();
            println!(
                "batch {} finalized commitment {} after {seconds_in_batch:.1}s",
                kickoff.batch_id, kickoff.commitment_txid
            );
            println!("funding output {}", kickoff.funding);
            println!("https://mutinynet.com/tx/{}", kickoff.commitment_txid);
            state.kickoff = Some(KickoffRecord {
                batch_id: kickoff.batch_id,
                commitment_txid: kickoff.commitment_txid.to_string(),
                funding: kickoff.funding.to_string(),
                expiry_height,
                seconds_in_batch,
            });
            state.save(&path)?;
        }
        "expire" => {
            let state = State::load(&path)?;
            let record = state
                .kickoff
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("run kickoff first"))?;
            let contract: SignedContract =
                serde_json::from_str(&std::fs::read_to_string(contract_path(&path))?)?;
            let funding: OutPoint = record.funding.parse()?;
            anyhow::ensure!(contract.dlc().funding_outpoint() == funding);
            let tip = tip_height(&http).await?;
            anyhow::ensure!(
                tip >= record.expiry_height,
                "the tip is {tip}; the expiry transaction is valid from height {}",
                record.expiry_height
            );
            let expiry = contract
                .expiry_tx()
                .ok_or_else(|| anyhow::anyhow!("the contract has no expiry transaction"))?;
            let response = http
                .post(format!("{ESPLORA}/tx"))
                .body(bitcoin::consensus::encode::serialize_hex(&expiry))
                .send()
                .await?;
            let status = response.status();
            let body = response.text().await?;
            anyhow::ensure!(status.is_success(), "broadcast failed: {status} {body}");
            println!("broadcast expiry transaction {body}");
            println!("https://mutinynet.com/tx/{body}");
        }
        other => anyhow::bail!("unknown command {other}"),
    }
    Ok(())
}

/// The VTXOs at each escrow's address, in escrow order.
async fn per_escrow(
    server: &ArkServer,
    escrows: &[EntryEscrow],
) -> anyhow::Result<Vec<Vec<ark_core::server::VirtualTxOutPoint>>> {
    let vtxos = server.escrow_vtxos(escrows).await?;
    Ok(escrows
        .iter()
        .map(|escrow| {
            let script = escrow.script_pubkey();
            vtxos
                .iter()
                .filter(|vtxo| vtxo.script == script)
                .cloned()
                .collect()
        })
        .collect())
}
