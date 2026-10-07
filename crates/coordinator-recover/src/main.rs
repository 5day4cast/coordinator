//! coordinator-recover: find and move a player's entries' money with only their nsec.
//!
//! See `docs/RECOVERY.md`. The nsec is read from `--nsec`, `COORDINATOR_RECOVER_NSEC` or a prompt,
//! and never printed; neither is the fee coin's key.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command as Process, ExitCode, Stdio};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use bitcoin::consensus::encode::serialize_hex;
use bitcoin::{
    Address, FeeRate, Network, NetworkKind, OutPoint, PrivateKey, ScriptBuf, Transaction,
};
use clap::{Parser, Subcommand};
use coordinator_recover::chain::TxStatus;
use coordinator_recover::fees::{sat_per_vb, AnchorBumper, BumpStatus, FeeCoin};
use coordinator_recover::inspect::utc;
use coordinator_recover::native::esplora::{Esplora, FeeEstimates};
use coordinator_recover::native::{self, ark};
use coordinator_recover::spec::{parse_pubkey, Kit};
use coordinator_recover::{defaults, parse_network, ClaimPlan, ClaimTx, Identity, Session};
use nostr::PublicKey;
use uuid::Uuid;
use zeroize::Zeroizing;

/// Esplora's estimate for this many blocks is the default fee rate.
const DEFAULT_TARGET_BLOCKS: u16 = 6;
/// Within this many blocks of a deadline, the default is the next-block estimate instead.
const URGENT_BLOCKS: u32 = 6;
/// Within this many seconds of a deadline in time, the default is the next-block estimate.
const URGENT_SECS: u64 = 2 * 3_600;

#[derive(Parser)]
#[command(
    name = "coordinator-recover",
    version,
    about = "Recover a player's entries from their nsec alone, without the coordinator"
)]
struct Cli {
    /// The player's nsec (or hex secret key). If not given, it is read from
    /// COORDINATOR_RECOVER_NSEC or asked for.
    #[arg(
        long,
        env = "COORDINATOR_RECOVER_NSEC",
        hide_env_values = true,
        global = true
    )]
    nsec: Option<String>,
    /// The recovery file downloaded from the account page.
    #[arg(long, global = true)]
    kit: Option<PathBuf>,
    /// Relays to read records from, comma separated. They replace this build's default relays;
    /// the recovery file's relays are read as well.
    #[arg(long, value_delimiter = ',', global = true)]
    relays: Vec<String>,
    /// The coordinator's recovery pubkey (hex or npub). By default the recovery file's, or the
    /// one this build has for the network.
    #[arg(long, global = true)]
    coordinator_pubkey: Option<String>,
    /// bitcoin, signet (Mutinynet), testnet4 or regtest. By default the recovery file's, else
    /// the network the player's records name.
    #[arg(long, global = true)]
    network: Option<String>,
    /// An Esplora API. Defaults to mempool.space on mainnet and Mutinynet's on signet.
    #[arg(long, global = true)]
    esplora: Option<String>,
    /// The Arkade server, for escrows. Defaults to the one the entry record names.
    #[arg(long, global = true)]
    arkd: Option<String>,
    /// The oracle's API, to fetch an attestation the relays do not have.
    #[arg(long, global = true)]
    oracle: Option<String>,
    /// A confirmed coin of yours (txid:vout) that pays for a CPFP child when a transaction with
    /// an anchor pays less than the fee rate. One coin pays for one child; repeat the flag (or
    /// separate coins with commas) for more. All must pay the --fee-key's address.
    #[arg(long, value_delimiter = ',', global = true)]
    fee_utxo: Vec<String>,
    /// The WIF private key of the --fee-utxo coins, which must pay this key's P2WPKH or
    /// single-key P2TR address. If not given, it is read from COORDINATOR_RECOVER_FEE_KEY.
    #[arg(
        long,
        env = "COORDINATOR_RECOVER_FEE_KEY",
        hide_env_values = true,
        global = true
    )]
    fee_key: Option<String>,
    /// Where a CPFP child's change goes: a bitcoin address of yours. By default back to the
    /// coin's own address, so the same key pays for the next child.
    #[arg(long, global = true)]
    fee_change: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Where each entry's money is, what can be done now, and when the next step opens.
    Inspect,
    /// Broadcast the outcome or expiry transaction, the split, and then the claim to your address,
    /// skipping what is already on chain. Each step waits until nodes take it; run it again as
    /// each confirms.
    Claim {
        /// Only this entry; by default every entry with a contract.
        #[arg(long)]
        entry: Option<Uuid>,
        /// Where the final claim pays: a bitcoin address of yours.
        #[arg(long)]
        to: Option<String>,
        /// The final claim's fee rate in sat/vB, and the rate a CPFP child lifts a transaction
        /// with an anchor to. By default Esplora's six-block estimate, or its next-block estimate
        /// when the step's deadline is near.
        #[arg(long)]
        fee_rate: Option<f64>,
        /// Your ticket preimage (hex), if the records do not hold it.
        #[arg(long)]
        ticket_preimage: Option<String>,
        /// Print the raw transactions, CPFP children included, instead of broadcasting them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Take an escrow back through Arkade (refund leaf), to an Ark address of yours.
    RefundEscrow {
        #[arg(long)]
        entry: Uuid,
        /// An Ark address of yours.
        #[arg(long)]
        to: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Take an escrow on chain without Arkade's cooperation, then sweep it to your address.
    Unroll {
        #[arg(long)]
        entry: Uuid,
        /// Where the sweep pays: a bitcoin address of yours.
        #[arg(long)]
        to: Option<String>,
        /// The sweep's fee rate in sat/vB, and the rate CPFP children lift the virtual
        /// transactions to; by default Esplora's six-block estimate.
        #[arg(long)]
        fee_rate: Option<f64>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let kit = match &cli.kit {
        Some(path) => {
            let json = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            Some(Kit::parse(&json).map_err(|e| e.to_string())?)
        }
        None => None,
    };
    let explicit_network = cli
        .network
        .as_deref()
        .map(parse_network)
        .transpose()
        .map_err(|e| e.to_string())?;
    let kit_network = match &kit {
        Some(kit) if !kit.network.is_empty() => {
            Some(parse_network(&kit.network).map_err(|e| e.to_string())?)
        }
        _ => None,
    };
    let network_hint = explicit_network.or(kit_network);

    let nsec = match cli.nsec.clone() {
        Some(nsec) => Zeroizing::new(nsec),
        None => prompt_nsec()?,
    };
    let identity = Identity::from_nsec(&nsec).map_err(|e| e.to_string())?;
    drop(nsec);
    let mut session = Session::new(identity, network_hint.unwrap_or(Network::Bitcoin));
    if let Some(pubkey) = &cli.coordinator_pubkey {
        session.set_coordinator(
            parse_pubkey(pubkey, "--coordinator-pubkey").map_err(|e| e.to_string())?,
        );
    }
    if let Some(kit) = kit {
        session.add_kit(kit).map_err(|e| e.to_string())?;
    }

    // Whose records to look for: the key given or in the file, else this build's.
    let built_in = defaults::built_in();
    let candidates: Vec<(PublicKey, Option<Network>)> = match session.coordinator() {
        Ok(coordinator) => vec![(coordinator, network_hint)],
        Err(_) => built_in
            .coordinators
            .iter()
            .filter(|(network, _)| network_hint.is_none_or(|hint| hint == *network))
            .map(|(network, key)| (*key, Some(*network)))
            .collect(),
    };
    if candidates.is_empty() {
        return Err(match network_hint {
            Some(network) => format!(
                "this build has no coordinator recovery pubkey for {network}: pass \
                 --coordinator-pubkey <hex> or --kit <recovery file>"
            ),
            None => "this build has no coordinator recovery pubkey: pass --coordinator-pubkey \
                     <hex> with --network, or --kit <recovery file>"
                .into(),
        });
    }

    let mut relays: Vec<String> = if cli.relays.is_empty() {
        built_in.relays.clone()
    } else {
        cli.relays.clone()
    };
    relays.extend(session.kit_relays());
    relays.sort();
    relays.dedup();

    eprintln!("Reading records from {} relays", relays.len());
    for warning in native::find_player(&mut session, &relays, &candidates).await? {
        eprintln!("Note: {warning}");
    }
    let network = session.network();
    if network_hint.is_none() {
        eprintln!("Network: {network}, as your records say (pass --network to choose another)");
    }
    for warning in native::load(&mut session, &relays).await? {
        eprintln!("Note: {warning}");
    }
    for warning in native::find_attestations(&mut session, &relays, cli.oracle.as_deref()).await {
        eprintln!("Note: {warning}");
    }
    for warning in &session.warnings {
        eprintln!("Warning: {warning}");
    }

    let esplora_url = match &cli.esplora {
        Some(url) => url.clone(),
        None => native::esplora::default_url(network)
            .ok_or("no public Esplora on this network: pass --esplora")?
            .to_owned(),
    };
    let esplora = Esplora::new(&esplora_url);
    let fee_args = FeeArgs {
        utxos: cli.fee_utxo.clone(),
        key: cli.fee_key.clone().map(Zeroizing::new),
        change: cli.fee_change.clone(),
    };

    match cli.command {
        Command::Inspect => inspect(&mut session, &esplora, cli.arkd.as_deref()).await,
        Command::Claim {
            entry,
            to,
            fee_rate,
            ticket_preimage,
            dry_run,
        } => {
            let destination = to.map(|to| destination(&to, network)).transpose()?;
            let fees = match fee_rate {
                Some(rate) => FeePolicy::Given(given_fee_rate(rate)?),
                None => FeePolicy::Estimated(esplora.fee_estimates().await?),
            };
            let mut coins = fee_coins(&fee_args, &esplora, network).await?;
            let entries: Vec<Uuid> = match entry {
                Some(entry) => vec![entry],
                None => session
                    .entries()
                    .map(|e| e.entry_id)
                    .filter(|id| session.contract(*id).is_some())
                    .collect(),
            };
            if entries.is_empty() {
                println!("No entry has a contract to claim.");
            }
            // One entry's failure does not stop the others.
            let mut failed = Vec::new();
            for entry_id in &entries {
                let claimed = claim(
                    &mut session,
                    &esplora,
                    *entry_id,
                    ClaimArgs {
                        destination: destination.clone(),
                        fees: &fees,
                        ticket_preimage: ticket_preimage.as_deref(),
                        dry_run,
                    },
                    &mut coins,
                )
                .await;
                if let Err(e) = claimed {
                    println!("  Error: {e}");
                    failed.push(*entry_id);
                }
            }
            match failed.len() {
                0 => Ok(()),
                n => Err(format!(
                    "{n} of {} entries could not be claimed this run: {}",
                    entries.len(),
                    failed
                        .iter()
                        .map(Uuid::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
        Command::RefundEscrow { entry, to, dry_run } => {
            let escrow = session
                .escrow(entry)
                .map_err(|e| e.to_string())?
                .ok_or("this entry has no escrow")?;
            let key = session
                .entry_key(session.entry(entry).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let url = arkd_url(cli.arkd.as_deref(), &session, entry)?;
            for line in ark::refund(&url, &escrow, &key, &to, dry_run).await? {
                println!("{line}");
            }
            Ok(())
        }
        Command::Unroll {
            entry,
            to,
            fee_rate,
            dry_run,
        } => {
            let escrow = session
                .escrow(entry)
                .map_err(|e| e.to_string())?
                .ok_or("this entry has no escrow")?;
            let key = session
                .entry_key(session.entry(entry).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let destination = to.map(|to| destination(&to, network)).transpose()?;
            let fee_rate = match fee_rate {
                Some(rate) => given_fee_rate(rate)?,
                None => esplora.fee_rate(DEFAULT_TARGET_BLOCKS).await?,
            };
            let coins = fee_coins(&fee_args, &esplora, network).await?;
            let url = arkd_url(cli.arkd.as_deref(), &session, entry).ok();
            let lines = ark::unroll(
                url.as_deref(),
                &escrow,
                &key,
                &esplora,
                destination,
                fee_rate,
                coins.first().map(|coin| coin as &dyn AnchorBumper),
                dry_run,
            )
            .await?;
            for line in lines {
                println!("{line}");
            }
            Ok(())
        }
    }
}

async fn inspect(
    session: &mut Session,
    esplora: &Esplora,
    arkd: Option<&str>,
) -> Result<(), String> {
    // Ask Arkade about escrows that have not become contracts.
    let escrows: Vec<(Uuid, String)> = session
        .entries()
        .filter(|entry| session.contract(entry.entry_id).is_none())
        .filter_map(|entry| {
            let url = arkd
                .map(str::to_owned)
                .or_else(|| entry.escrow.as_ref().map(|e| e.arkd_url.clone()))
                .filter(|url| !url.is_empty())?;
            Some((entry.entry_id, url))
        })
        .collect();
    for (entry_id, url) in escrows {
        if let Ok(Some(escrow)) = session.escrow(entry_id) {
            let state = ark::escrow_state(&url, &escrow).await;
            session.set_escrow_state(entry_id, state);
        }
    }
    let now = now();
    let reports = native::settle_chain(session, esplora, |session| session.inspect(now)).await?;
    if reports.is_empty() {
        println!("No entries were found for this nsec.");
    }
    for report in reports {
        println!("{report}");
    }
    Ok(())
}

/// The fee rate a claim pays its final transaction and lifts anchored transactions to.
enum FeePolicy {
    /// `--fee-rate`.
    Given(FeeRate),
    /// Esplora's estimate for [`DEFAULT_TARGET_BLOCKS`], or the next block's near a deadline.
    Estimated(FeeEstimates),
}

/// What [`claim`] needs beyond the session and the chain.
struct ClaimArgs<'a> {
    destination: Option<ScriptBuf>,
    fees: &'a FeePolicy,
    ticket_preimage: Option<&'a str>,
    dry_run: bool,
}

async fn claim(
    session: &mut Session,
    esplora: &Esplora,
    entry_id: Uuid,
    args: ClaimArgs<'_>,
    coins: &mut Vec<FeeCoin>,
) -> Result<(), String> {
    println!("Entry {entry_id}");
    let mut fee_rate = match args.fees {
        FeePolicy::Given(rate) => *rate,
        FeePolicy::Estimated(estimates) => estimates.within(DEFAULT_TARGET_BLOCKS),
    };
    let mut plan = plan_claim(session, esplora, entry_id, &args, fee_rate).await?;
    let urgent = match (args.fees, &plan) {
        (FeePolicy::Estimated(estimates), Ok(planned)) => {
            let urgent = estimates.within(1);
            (urgent > fee_rate && deadline_near(planned, session)).then_some(urgent)
        }
        _ => None,
    };
    if let Some(urgent) = urgent {
        fee_rate = urgent;
        plan = plan_claim(session, esplora, entry_id, &args, fee_rate).await?;
        println!(
            "  The deadline is near: paying the next-block estimate, {} sat/vB",
            fee_rate.to_sat_per_vb_ceil()
        );
    }
    let plan = plan.map_err(|e| e.to_string())?;
    for done in &plan.done {
        println!("  Already on chain: {done}");
    }
    if let Some(deadline) = &plan.deadline {
        let by = match (deadline.height, deadline.time) {
            (Some(height), _) => session.chain.describe_height(height),
            (None, Some(time)) => utc(time),
            (None, None) => String::new(),
        };
        println!("  Deadline: confirm before {by}: {}", deadline.reason);
    }
    for step in &plan.unconfirmed {
        println!(
            "  {} transaction {} is in the mempool: {}",
            step.label,
            step.tx.compute_txid(),
            step.bump.describe()
        );
        if let Some(child) = cpfp_child(step, fee_rate, coins)? {
            if args.dry_run {
                println!("  {}", serialize_hex(&child));
                continue;
            }
            esplora.broadcast_package(&step.tx, &child).await?;
            println!("  Broadcast CPFP child {}", child.compute_txid());
        }
    }
    for step in &plan.txs {
        let txid = step.tx.compute_txid();
        println!(
            "  {} transaction {txid}: {}",
            step.label,
            step.bump.describe()
        );
        let child = cpfp_child(step, fee_rate, coins)?;
        if args.dry_run {
            println!("  {}", serialize_hex(&step.tx));
            if let Some(child) = &child {
                println!("  {}", serialize_hex(child));
            }
            continue;
        }
        if let Some(child) = child {
            esplora.broadcast_package(&step.tx, &child).await?;
            println!("  Broadcast with CPFP child {}", child.compute_txid());
            continue;
        }
        if esplora.tx_status(txid).await?.is_some() {
            println!("  Already known to the chain");
            continue;
        }
        esplora.broadcast(&step.tx).await.map_err(|e| {
            if matches!(step.bump, BumpStatus::Anchor { .. }) && coins.is_empty() {
                format!("{e}; pass --fee-utxo to send it with a CPFP child as a package")
            } else {
                e
            }
        })?;
        println!("  Broadcast");
    }
    if let Some(waiting) = &plan.waiting {
        println!("  Then: {waiting}");
    }
    Ok(())
}

/// The claim for `entry_id` at `fee_rate`, once the chain lookups it needs are answered.
async fn plan_claim(
    session: &mut Session,
    esplora: &Esplora,
    entry_id: Uuid,
    args: &ClaimArgs<'_>,
    fee_rate: FeeRate,
) -> Result<coordinator_recover::Result<ClaimPlan>, String> {
    let destination = args.destination.clone();
    let preimage = args.ticket_preimage;
    native::settle_chain(session, esplora, move |session| {
        session.claim(entry_id, destination.clone(), fee_rate, preimage)
    })
    .await
}

/// Whether `plan`'s deadline is close enough to pay for the next block.
fn deadline_near(plan: &ClaimPlan, session: &Session) -> bool {
    let Some(deadline) = &plan.deadline else {
        return false;
    };
    let by_height = deadline
        .height
        .and_then(|height| session.chain.blocks_until(height))
        .is_some_and(|blocks| blocks <= URGENT_BLOCKS);
    let by_time = deadline
        .time
        .is_some_and(|time| now() + URGENT_SECS >= time);
    by_height || by_time
}

/// A CPFP child for `step` if it has an anchor and pays less than `fee_rate` on its own, paid
/// from the next fee coin, which it uses up. Without a coin, says how to give one.
fn cpfp_child(
    step: &ClaimTx,
    fee_rate: FeeRate,
    coins: &mut Vec<FeeCoin>,
) -> Result<Option<Transaction>, String> {
    let Some(parent_fee) = step.bump.below(&step.tx, fee_rate) else {
        return Ok(None);
    };
    let rate = fee_rate.to_sat_per_vb_ceil();
    let own = sat_per_vb(&step.tx, parent_fee);
    let Some(coin) = coins.first() else {
        println!(
            "  It pays about {own} sat/vB on its own, less than {rate}: pass --fee-utxo (and its \
             key) to pay for it with a child"
        );
        return Ok(None);
    };
    let child = coin.bump(&step.tx, parent_fee, fee_rate)?;
    println!(
        "  CPFP child {} spends its anchor and {} so that both pay {rate} sat/vB (it pays about \
         {own} on its own)",
        child.compute_txid(),
        coin.outpoint()
    );
    coins.remove(0);
    Ok(Some(child))
}

/// The fee coins' flags, kept apart from the subcommand.
struct FeeArgs {
    utxos: Vec<String>,
    key: Option<Zeroizing<String>>,
    change: Option<String>,
}

/// The fee coins given with --fee-utxo: each output is read from the chain, and must be
/// confirmed, unspent and pay the key's address.
async fn fee_coins(
    args: &FeeArgs,
    esplora: &Esplora,
    network: Network,
) -> Result<Vec<FeeCoin>, String> {
    if args.utxos.is_empty() {
        if args.change.is_some() {
            return Err("--fee-change goes with --fee-utxo".into());
        }
        return Ok(Vec::new());
    }
    let key = args
        .key
        .as_ref()
        .ok_or("--fee-utxo needs --fee-key or COORDINATOR_RECOVER_FEE_KEY")?;
    let key = PrivateKey::from_wif(key.trim())
        .map_err(|_| "--fee-key is not a WIF private key".to_owned())?;
    if key.network != NetworkKind::from(network) {
        return Err(format!(
            "--fee-key is a key for another network than {network}"
        ));
    }
    let change = args
        .change
        .as_deref()
        .map(|address| destination(address, network))
        .transpose()?;
    let mut coins = Vec::new();
    for utxo in &args.utxos {
        let outpoint =
            OutPoint::from_str(utxo.trim()).map_err(|e| format!("--fee-utxo {utxo}: {e}"))?;
        let funding = esplora
            .transaction(outpoint.txid)
            .await?
            .ok_or_else(|| format!("--fee-utxo: {} is not on chain", outpoint.txid))?;
        let prevout = funding
            .output
            .get(outpoint.vout as usize)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "--fee-utxo: {} has no output {}",
                    outpoint.txid, outpoint.vout
                )
            })?;
        if !matches!(
            esplora.tx_status(outpoint.txid).await?,
            Some(TxStatus {
                confirmed_height: Some(_)
            })
        ) {
            return Err(format!(
                "--fee-utxo {outpoint} is not confirmed yet: a child pays only from a confirmed coin"
            ));
        }
        if let Some(by) = esplora
            .outspend(outpoint.txid, outpoint.vout)
            .await?
            .spent_by
        {
            return Err(format!("--fee-utxo {outpoint} is already spent by {by}"));
        }
        coins.push(FeeCoin::new(outpoint, prevout, key, change.clone())?);
    }
    Ok(coins)
}

fn arkd_url(given: Option<&str>, session: &Session, entry: Uuid) -> Result<String, String> {
    given
        .map(str::to_owned)
        .or_else(|| {
            session
                .entry(entry)
                .ok()
                .and_then(|entry| entry.escrow.as_ref())
                .map(|escrow| escrow.arkd_url.clone())
        })
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "the record names no Arkade server: pass --arkd".to_owned())
}

fn destination(address: &str, network: Network) -> Result<ScriptBuf, String> {
    Address::from_str(address)
        .map_err(|e| format!("{address}: {e}"))?
        .require_network(network)
        .map_err(|e| format!("{address}: {e}"))
        .map(|address| address.script_pubkey())
}

fn given_fee_rate(rate: f64) -> Result<FeeRate, String> {
    if rate.is_finite() && rate >= 1.0 {
        Ok(FeeRate::from_sat_per_kwu((rate * 250.0).ceil() as u64))
    } else {
        Err(format!("--fee-rate {rate}: at least 1 sat/vB"))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Ask for the nsec on the terminal without echoing it.
fn prompt_nsec() -> Result<Zeroizing<String>, String> {
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    eprint!("nsec: ");
    let _ = std::io::stderr().flush();
    let echo = |on: bool| {
        if terminal {
            let _ = Process::new("stty")
                .arg(if on { "echo" } else { "-echo" })
                .stdin(Stdio::inherit())
                .status();
        }
    };
    echo(false);
    let mut line = Zeroizing::new(String::new());
    let read = stdin.lock().read_line(&mut line);
    echo(true);
    eprintln!();
    read.map_err(|e| format!("cannot read the nsec: {e}"))?;
    Ok(Zeroizing::new(line.trim().to_owned()))
}
