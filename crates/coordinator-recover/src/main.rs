//! coordinator-recover: find and move a player's entries' money with only their nsec.
//!
//! See `docs/RECOVERY.md`. The nsec is read from `--nsec`, `COORDINATOR_RECOVER_NSEC` or a prompt,
//! and never printed.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command as Process, ExitCode, Stdio};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use bitcoin::consensus::encode::serialize_hex;
use bitcoin::{Address, FeeRate, Network, ScriptBuf};
use clap::{Parser, Subcommand};
use coordinator_recover::native::{self, ark, esplora::Esplora, relays::DEFAULT_RELAYS};
use coordinator_recover::spec::{parse_pubkey, Kit};
use coordinator_recover::{parse_network, Identity, Session};
use uuid::Uuid;
use zeroize::Zeroizing;

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
    /// Relays to read records from, comma separated, besides the recovery file's.
    #[arg(long, value_delimiter = ',', global = true)]
    relays: Vec<String>,
    /// The coordinator's recovery pubkey (hex). The recovery file names it.
    #[arg(long, global = true)]
    coordinator_pubkey: Option<String>,
    /// bitcoin, signet (Mutinynet), testnet4 or regtest. Defaults to the recovery file's, else bitcoin.
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Where each entry's money is, what can be done now, and when the next step opens.
    Inspect,
    /// Broadcast the outcome or expiry transaction, the split, and then the claim to your address,
    /// skipping what is already on chain.
    Claim {
        /// Only this entry; by default every entry with a contract.
        #[arg(long)]
        entry: Option<Uuid>,
        /// Where the final claim pays: a bitcoin address of yours.
        #[arg(long)]
        to: Option<String>,
        /// The final claim's fee rate in sat/vB; by default Esplora's six-block estimate.
        #[arg(long)]
        fee_rate: Option<f64>,
        /// Your ticket preimage (hex), if the records do not hold it.
        #[arg(long)]
        ticket_preimage: Option<String>,
        /// Print the raw transactions instead of broadcasting them.
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
        /// The sweep's fee rate in sat/vB; by default Esplora's six-block estimate.
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
    let network = match (&cli.network, &kit) {
        (Some(name), _) => parse_network(name),
        (None, Some(kit)) if !kit.network.is_empty() => parse_network(&kit.network),
        _ => Ok(Network::Bitcoin),
    }
    .map_err(|e| e.to_string())?;

    let nsec = match cli.nsec.clone() {
        Some(nsec) => Zeroizing::new(nsec),
        None => prompt_nsec()?,
    };
    let identity = Identity::from_nsec(&nsec).map_err(|e| e.to_string())?;
    drop(nsec);
    let mut session = Session::new(identity, network);
    if let Some(pubkey) = &cli.coordinator_pubkey {
        session.set_coordinator(
            parse_pubkey(pubkey, "--coordinator-pubkey").map_err(|e| e.to_string())?,
        );
    }
    if let Some(kit) = kit {
        session.add_kit(kit).map_err(|e| e.to_string())?;
    }
    if session.coordinator().is_err() {
        if let Some(pubkey) = native::default_coordinator_pubkey(network) {
            session.set_coordinator(
                parse_pubkey(pubkey, "coordinator pubkey").map_err(|e| e.to_string())?,
            );
        }
    }
    session.coordinator().map_err(|e| e.to_string())?;

    let mut relays: Vec<String> = cli.relays.clone();
    relays.extend(session.kit_relays());
    if relays.is_empty() {
        relays = DEFAULT_RELAYS
            .iter()
            .map(|relay| relay.to_string())
            .collect();
    }
    relays.sort();
    relays.dedup();

    let esplora_url = match &cli.esplora {
        Some(url) => url.clone(),
        None => native::esplora::default_url(network)
            .ok_or("no public Esplora on this network: pass --esplora")?
            .to_owned(),
    };
    let esplora = Esplora::new(&esplora_url);

    eprintln!("Reading records from {} relays", relays.len());
    for warning in native::load(&mut session, &relays).await? {
        eprintln!("Note: {warning}");
    }
    for warning in native::find_attestations(&mut session, &relays, cli.oracle.as_deref()).await {
        eprintln!("Note: {warning}");
    }
    for warning in &session.warnings {
        eprintln!("Warning: {warning}");
    }

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
            let fee_rate = fee_rate_or_estimate(fee_rate, &esplora).await?;
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
            for entry_id in entries {
                claim(
                    &mut session,
                    &esplora,
                    entry_id,
                    destination.clone(),
                    fee_rate,
                    ticket_preimage.as_deref(),
                    dry_run,
                )
                .await?;
            }
            Ok(())
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
            let fee_rate = fee_rate_or_estimate(fee_rate, &esplora).await?;
            let url = arkd_url(cli.arkd.as_deref(), &session, entry).ok();
            let lines = ark::unroll(
                url.as_deref(),
                &escrow,
                &key,
                &esplora,
                destination,
                fee_rate,
                None,
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

async fn claim(
    session: &mut Session,
    esplora: &Esplora,
    entry_id: Uuid,
    destination: Option<ScriptBuf>,
    fee_rate: FeeRate,
    ticket_preimage: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    println!("Entry {entry_id}");
    let plan = native::settle_chain(session, esplora, |session| {
        session.claim(entry_id, destination.clone(), fee_rate, ticket_preimage)
    })
    .await?
    .map_err(|e| e.to_string())?;
    for done in &plan.done {
        println!("  Already on chain: {done}");
    }
    for step in &plan.txs {
        let txid = step.tx.compute_txid();
        println!(
            "  {} transaction {txid}: {}",
            step.label,
            step.bump.describe()
        );
        if dry_run {
            println!("  {}", serialize_hex(&step.tx));
            continue;
        }
        if esplora.tx_status(txid).await?.is_some() {
            println!("  Already known to the chain");
            continue;
        }
        esplora.broadcast(&step.tx).await?;
        println!("  Broadcast");
    }
    if let Some(waiting) = &plan.waiting {
        println!("  Then: {waiting}");
    }
    Ok(())
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

async fn fee_rate_or_estimate(given: Option<f64>, esplora: &Esplora) -> Result<FeeRate, String> {
    match given {
        Some(rate) if rate.is_finite() && rate >= 1.0 => {
            Ok(FeeRate::from_sat_per_kwu((rate * 250.0).ceil() as u64))
        }
        Some(rate) => Err(format!("--fee-rate {rate}: at least 1 sat/vB")),
        None => esplora.fee_rate(6).await,
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
