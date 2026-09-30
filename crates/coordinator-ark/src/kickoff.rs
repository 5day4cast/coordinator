//! Kickoff: one Arkade batch spends a pool's escrows into its DLC funding output.
//!
//! 1. Register an intent. Its proof signs every escrow's funding leaf.
//!    It names the funding output, and the coordinator's fee if any, as on-chain outputs.
//! 2. When a batch selects the intent, confirm it and collect the batch's connector tree.
//! 3. At batch finalization, the commitment transaction is known.
//!    Check that it pays those outputs, and that the connectors spend from it.
//! 4. Run [`KickoffHooks::before_forfeits`], which signs the pool's refund transaction.
//! 5. Sign one forfeit per escrow, and submit them.
//! 6. Return once the server finalizes the batch.
//!
//! Nothing is forfeited before step 5.
//! If a check or the hook fails, every escrow stays spendable.
//! But arkd bans the scripts of VTXOs whose forfeits never arrive, for its configured ban duration.
//!
//! arkd keeps an intent queued until a batch confirms it or its owner deletes it, and refuses
//! every other spend of its escrows meanwhile, refunds included. So a kickoff that fails before
//! step 5 deletes its intent, and each kickoff first deletes any intent an earlier attempt left,
//! as one interrupted by a restart would. A delete proof signs the escrows' funding leaves like
//! the intent, but pays nothing.
//!
//! The intent lists no cosigner key. A batch that also creates other users' VTXOs builds a VTXO
//! tree, with an empty leaf for this intent that the server cosigns alone, so the pool is never
//! asked to cosign. Steps 3 to 5 are the only work inside the server's session window.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ark_core::batch::create_and_sign_forfeit_txs;
use ark_core::intent::{self, make_intent, Intent, IntentMessage};
use ark_core::server::{BatchTreeEventType, Info, StreamEvent};
use ark_core::{TxGraph, TxGraphChunk};
use async_trait::async_trait;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::DisplayHex;
use bitcoin::taproot::LeafVersion;
use bitcoin::{Amount, OutPoint, Psbt, TapLeafHash, Transaction, TxOut, Txid, XOnlyPublicKey};
use coordinator_ark_escrow::{EntryEscrow, EscrowPath, ServerRules};
use futures::StreamExt;
use tokio::time::{timeout_at, Instant};

use crate::signer::{collect_signatures, insert_signature};
use crate::{
    script_spend_sighash, ArkTransport, BoxError, Error, EscrowSigner, EventStream, SigningPurpose,
    SigningRequest,
};

/// In ark-core's forfeit transactions, input 0 spends the connector and input 1 spends the VTXO.
const FORFEIT_VTXO_INDEX: usize = 1;

/// One entry's escrow VTXO.
#[derive(Debug, Clone)]
pub struct EscrowInput {
    pub escrow: EntryEscrow,
    pub outpoint: OutPoint,
    pub amount: Amount,
}

/// A pool ready to kick off: its escrows, and the DLC funding output they pay.
#[derive(Debug, Clone)]
pub struct PoolFunding {
    inputs: Vec<EscrowInput>,
    total: Amount,
    funding_output: TxOut,
    coordinator_fee: Option<TxOut>,
    coordinator: XOnlyPublicKey,
}

impl PoolFunding {
    /// Spend `inputs` into `funding_output`.
    ///
    /// Every escrow must be one that `rules`' server accepts, and all must share one coordinator key.
    /// The funding output can be worth at most the escrows' total, and any difference goes to the server as fees.
    pub fn new(
        inputs: Vec<EscrowInput>,
        funding_output: TxOut,
        rules: &ServerRules,
        dust: Amount,
    ) -> Result<Self, Error> {
        let invalid = |reason: String| Err(Error::InvalidPool(reason));
        let Some(first) = inputs.first() else {
            return invalid("a pool needs at least one escrow".into());
        };
        let coordinator = first.escrow.terms().coordinator;
        let mut outpoints = HashSet::new();
        let mut total = Amount::ZERO;
        for input in &inputs {
            let outpoint = input.outpoint;
            if !outpoints.insert(outpoint) {
                return invalid(format!("escrow {outpoint} appears twice"));
            }
            let terms = input.escrow.terms();
            if terms.coordinator != coordinator {
                return invalid(format!("escrow {outpoint} has another coordinator key"));
            }
            if terms.server != rules.signer {
                return invalid(format!("escrow {outpoint} names another server"));
            }
            rules.check(input.escrow.vtxo_script())?;
            // arkd takes sub-dust VTXOs without a forfeit, which would leave nothing tying them to this batch.
            if input.amount < dust {
                return invalid(format!("escrow {outpoint} holds less than dust"));
            }
            total = total
                .checked_add(input.amount)
                .ok_or_else(|| Error::InvalidPool("the escrows overflow".into()))?;
        }
        if funding_output.value < dust {
            return invalid("the funding output is below dust".into());
        }
        if funding_output.value > total {
            return invalid(format!(
                "the funding output needs {}, but the escrows hold {total}",
                funding_output.value
            ));
        }
        Ok(Self {
            inputs,
            total,
            funding_output,
            coordinator_fee: None,
            coordinator,
        })
    }

    /// Also pay the coordinator's fee, on chain, from the same escrows.
    ///
    /// Each escrow holds a ticket price: an entry fee for the pool, plus the coordinator's fee.
    /// The funding output and the fee together can be worth at most the escrows' total.
    pub fn with_coordinator_fee(self, fee_output: TxOut, dust: Amount) -> Result<Self, Error> {
        let invalid = |reason: String| Err(Error::InvalidPool(reason));
        if fee_output.value < dust {
            return invalid("the coordinator fee is below dust".into());
        }
        if fee_output.script_pubkey == self.funding_output.script_pubkey {
            return invalid("the coordinator fee must not pay the funding script".into());
        }
        let outputs = self.funding_output.value.checked_add(fee_output.value);
        if outputs.is_none_or(|outputs| outputs > self.total) {
            return invalid(format!(
                "the funding output and fee need more than the escrows' {}",
                self.total
            ));
        }
        Ok(Self {
            coordinator_fee: Some(fee_output),
            ..self
        })
    }

    pub fn inputs(&self) -> &[EscrowInput] {
        &self.inputs
    }

    pub fn funding_output(&self) -> &TxOut {
        &self.funding_output
    }

    pub fn coordinator_fee(&self) -> Option<&TxOut> {
        self.coordinator_fee.as_ref()
    }

    /// The intent's on-chain outputs: the funding output, then the coordinator's fee if any.
    fn outputs(&self) -> Vec<TxOut> {
        [Some(&self.funding_output), self.coordinator_fee.as_ref()]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }

    pub fn coordinator(&self) -> XOnlyPublicKey {
        self.coordinator
    }
}

/// Work that must happen between learning the commitment transaction and forfeiting the escrows.
#[async_trait]
pub trait KickoffHooks: Send + Sync {
    /// The commitment transaction pays the pool's funding output at `funding`, and nothing is forfeited yet.
    ///
    /// Sign the pool's refund transaction here, spending `funding`, so players can always leave the funding output.
    /// Returning an error abandons the batch, and every escrow stays spendable.
    /// This runs inside the server's session window, so it must be quick.
    async fn before_forfeits(
        &self,
        funding: OutPoint,
        commitment_tx: &Psbt,
    ) -> Result<(), BoxError>;
}

#[derive(Debug, Clone)]
pub struct KickoffConfig {
    /// How long the intent stays valid for a batch to select it.
    pub intent_lifetime: Duration,
    /// How long to wait, in total, for a batch to select and finalize the intent.
    pub timeout: Duration,
}

impl KickoffConfig {
    /// A two-minute intent, and time for one more session plus a margin after it expires.
    pub fn for_server(info: &Info) -> Self {
        let intent_lifetime = Duration::from_secs(120);
        Self {
            intent_lifetime,
            timeout: intent_lifetime + Duration::from_secs(info.session_duration + 30),
        }
    }
}

/// How long a delete proof stays valid. It is used at once.
const DELETE_PROOF_LIFETIME: Duration = Duration::from_secs(120);

/// A funded pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kickoff {
    pub intent_id: String,
    pub batch_id: String,
    pub commitment_txid: Txid,
    /// The pool's DLC funding output, in the commitment transaction.
    pub funding: OutPoint,
    /// The coordinator's fee output, if the pool pays one.
    pub coordinator_fee: Option<OutPoint>,
}

enum State {
    Registered,
    Joined {
        batch_id: String,
        connectors: Vec<TxGraphChunk>,
        /// When the batch started, to log how much of its session is left at finalization.
        started: Instant,
    },
    Forfeited {
        batch_id: String,
        commitment_txid: Txid,
        funding: OutPoint,
        coordinator_fee: Option<OutPoint>,
    },
}

impl State {
    fn batch_id(&self) -> Option<&str> {
        match self {
            State::Registered => None,
            State::Joined { batch_id, .. } | State::Forfeited { batch_id, .. } => Some(batch_id),
        }
    }

    fn waiting_for(&self) -> &'static str {
        match self {
            State::Registered => "waiting for a batch to select the kickoff intent",
            State::Joined { .. } => "waiting for the batch's commitment transaction",
            State::Forfeited { .. } => "waiting for the batch to finalize after the forfeits",
        }
    }
}

/// A step of the kickoff inside the server's session window.
#[derive(Debug, Clone, Copy)]
enum Step {
    Contract,
    Forfeits,
    Submit,
}

/// How long the batch took to reach finalization, and each step after it, for the log.
struct Steps {
    finalization: Duration,
    taken: Vec<(Step, Duration, bool)>,
}

impl Steps {
    fn new(finalization: Duration) -> Self {
        Self {
            finalization,
            taken: Vec::new(),
        }
    }

    async fn time<T>(
        &mut self,
        step: Step,
        run: impl std::future::Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        let started = Instant::now();
        let result = run.await;
        self.taken.push((step, started.elapsed(), result.is_ok()));
        result
    }
}

impl std::fmt::Display for Steps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "batch finalization after {:.1} s",
            self.finalization.as_secs_f64()
        )?;
        for (step, taken, done) in &self.taken {
            let what = match (step, done) {
                (Step::Contract, true) => "contract signed in",
                (Step::Contract, false) => "contract signing failed after",
                (Step::Forfeits, true) => "forfeits signed in",
                (Step::Forfeits, false) => "forfeit signing failed after",
                (Step::Submit, true) => "submitted in",
                (Step::Submit, false) => "submit failed after",
            };
            write!(f, ", {what} {:.1} s", taken.as_secs_f64())?;
        }
        Ok(())
    }
}

/// Fund `pool` in the next batch that selects it. See the module documentation for the steps.
pub async fn fund_pool<T: ArkTransport + ?Sized>(
    transport: &T,
    info: &Info,
    pool: &PoolFunding,
    players: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
    hooks: &dyn KickoffHooks,
    config: &KickoffConfig,
) -> Result<Kickoff, Error> {
    // An earlier attempt, interrupted before its batch, may have left its intent queued. arkd
    // would refuse this one for spending the same escrows. If it cannot be deleted, registering
    // says why.
    match delete_pool_intent(transport, pool, players, coordinator).await {
        Ok(true) => log::warn!("deleted a kickoff intent an earlier attempt left queued"),
        Ok(false) => {}
        Err(error) => log::warn!("cannot delete any kickoff intent left queued: {error}"),
    }
    let deadline = Instant::now() + config.timeout;
    // arkd asks every listed cosigner to sign a batch's VTXO tree, even for an intent with no
    // VTXO outputs, and needs cosigners only for off-chain receivers. So list none.
    let vtxo_inputs = intent_inputs(&pool.inputs)?;

    let outputs = pool.outputs();
    let now = unix_now()?;
    let message = IntentMessage::Register {
        onchain_output_indexes: (0..outputs.len()).collect(),
        valid_at: now,
        expire_at: now + config.intent_lifetime.as_secs(),
        own_cosigner_pks: Vec::new(),
    };
    let mut intent = make_intent(
        |_, _| Ok(Vec::new()),
        |_, _| Err(ark_core::Error::ad_hoc("a pool has no on-chain inputs")),
        vtxo_inputs.clone(),
        outputs.into_iter().map(intent::Output::Onchain).collect(),
        message,
    )?;
    sign_intent_proof(
        &mut intent,
        &pool.inputs,
        pool.coordinator,
        SigningPurpose::IntentProof,
        players,
        coordinator,
    )
    .await?;

    let topics = pool
        .inputs
        .iter()
        .map(|input| input.outpoint.to_string())
        .collect();
    // Subscribe before registering, so the batch that selects the intent cannot be missed.
    let mut events = transport.event_stream(topics).await?;
    let intent_id = transport.register_intent(intent).await?;
    log::info!(
        "registered kickoff intent {intent_id} for {} escrows",
        pool.inputs.len()
    );

    let mut state = State::Registered;
    let result = run_batch(
        transport,
        info,
        pool,
        players,
        coordinator,
        hooks,
        &mut events,
        deadline,
        intent_id.clone(),
        &vtxo_inputs,
        &mut state,
    )
    .await;
    // Until the forfeits are in, the escrows are the players' own, and the intent would hold
    // them in arkd's queue. Once a batch selected it, it is gone and this deletes nothing.
    if let Err(error) = &result {
        if !matches!(state, State::Forfeited { .. }) {
            if let Err(delete) = delete_pool_intent(transport, pool, players, coordinator).await {
                log::warn!(
                    "kickoff intent {intent_id} may still hold the escrows, since deleting it \
                     failed ({delete}) after the kickoff failed: {error}"
                );
            }
        }
    }
    result
}

/// Follow the batch from the intent's registration to its finalization, tracking `state`.
#[allow(clippy::too_many_arguments)]
async fn run_batch<T: ArkTransport + ?Sized>(
    transport: &T,
    info: &Info,
    pool: &PoolFunding,
    players: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
    hooks: &dyn KickoffHooks,
    events: &mut EventStream<'_>,
    deadline: Instant,
    intent_id: String,
    vtxo_inputs: &[intent::Input],
    state: &mut State,
) -> Result<Kickoff, Error> {
    let intent_hash = sha256::Hash::hash(intent_id.as_bytes())
        .to_byte_array()
        .to_lower_hex_string();
    loop {
        let event = match timeout_at(deadline, events.next()).await {
            Err(_) => return Err(Error::Timeout(state.waiting_for())),
            Ok(None) => return Err(Error::Protocol("the event stream ended".into())),
            Ok(Some(event)) => event?,
        };
        match event {
            StreamEvent::BatchStarted(event)
                if matches!(state, State::Registered)
                    && event.intent_id_hashes.contains(&intent_hash) =>
            {
                let started = Instant::now();
                transport.confirm_registration(intent_id.clone()).await?;
                log::info!("batch {} selected kickoff intent {intent_id}", event.id);
                *state = State::Joined {
                    batch_id: event.id,
                    connectors: Vec::new(),
                    started,
                };
            }
            StreamEvent::TreeTx(event) => {
                if let State::Joined {
                    batch_id,
                    connectors,
                    ..
                } = state
                {
                    if event.id == *batch_id
                        && matches!(event.batch_tree_event_type, BatchTreeEventType::Connector)
                    {
                        connectors.push(event.tx_graph_chunk);
                    }
                }
            }
            StreamEvent::BatchFinalization(event) => {
                let State::Joined {
                    batch_id,
                    connectors,
                    started,
                } = state
                else {
                    continue;
                };
                if event.id != *batch_id {
                    continue;
                }
                let mut steps = Steps::new(started.elapsed());
                let commitment_txid = event.commitment_tx.unsigned_tx.compute_txid();
                let chunks = std::mem::take(connectors);
                let forfeited = async {
                    let funding =
                        paid_output(&event.commitment_tx, &pool.funding_output, "funding")?;
                    let coordinator_fee = pool
                        .coordinator_fee
                        .as_ref()
                        .map(|fee| paid_output(&event.commitment_tx, fee, "coordinator fee"))
                        .transpose()?;
                    let connector_txs: HashMap<Txid, Transaction> = chunks
                        .iter()
                        .map(|chunk| {
                            (
                                chunk.tx.unsigned_tx.compute_txid(),
                                chunk.tx.unsigned_tx.clone(),
                            )
                        })
                        .collect();
                    let connectors = connector_graph(chunks, commitment_txid)?;
                    steps
                        .time(Step::Contract, async {
                            hooks
                                .before_forfeits(funding, &event.commitment_tx)
                                .await
                                .map_err(Error::Hook)
                        })
                        .await?;
                    let forfeits = steps
                        .time(
                            Step::Forfeits,
                            sign_forfeits(
                                vtxo_inputs,
                                &connectors,
                                &connector_txs,
                                info,
                                pool,
                                Arc::new(event.commitment_tx.unsigned_tx.clone()),
                                players,
                                coordinator,
                            ),
                        )
                        .await?;
                    steps
                        .time(Step::Submit, transport.submit_forfeits(forfeits))
                        .await?;
                    Ok::<_, Error>((funding, coordinator_fee))
                }
                .await;
                let (funding, coordinator_fee) = match forfeited {
                    Ok(paid) => {
                        log::info!(
                            "forfeited {} escrows into commitment {commitment_txid}: {steps}",
                            pool.inputs.len()
                        );
                        paid
                    }
                    Err(error) => {
                        log::info!(
                            "did not forfeit {} escrows into commitment {commitment_txid}: {steps}",
                            pool.inputs.len()
                        );
                        return Err(error);
                    }
                };
                *state = State::Forfeited {
                    batch_id: event.id,
                    commitment_txid,
                    funding,
                    coordinator_fee,
                };
            }
            StreamEvent::BatchFinalized(event) => {
                if let State::Forfeited {
                    batch_id,
                    commitment_txid,
                    funding,
                    coordinator_fee,
                } = &*state
                {
                    if event.id != *batch_id {
                        continue;
                    }
                    if event.commitment_txid != *commitment_txid {
                        return Err(Error::Protocol(format!(
                            "batch {batch_id} finalized {}, not {commitment_txid}",
                            event.commitment_txid
                        )));
                    }
                    return Ok(Kickoff {
                        intent_id,
                        batch_id: event.id,
                        commitment_txid: *commitment_txid,
                        funding: *funding,
                        coordinator_fee: *coordinator_fee,
                    });
                }
            }
            StreamEvent::BatchFailed(event) if state.batch_id() == Some(event.id.as_str()) => {
                return Err(Error::BatchFailed {
                    id: event.id,
                    reason: event.reason,
                });
            }
            _ => {}
        }
    }
}

/// Delete any queued intent that spends one of the pool's escrows.
///
/// Returns whether arkd deleted one: `false` means none was queued.
pub async fn delete_pool_intent<T: ArkTransport + ?Sized>(
    transport: &T,
    pool: &PoolFunding,
    players: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
) -> Result<bool, Error> {
    delete_intent(
        transport,
        &pool.inputs,
        pool.coordinator,
        players,
        coordinator,
    )
    .await
}

/// Delete any queued intent that spends `input`'s escrow, with a proof over that escrow alone.
///
/// arkd deletes a whole intent for a proof over any one of its inputs, so this frees every other
/// escrow the intent held as well. Returns whether arkd deleted one.
pub async fn delete_escrow_intent<T: ArkTransport + ?Sized>(
    transport: &T,
    input: &EscrowInput,
    players: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
) -> Result<bool, Error> {
    let coordinator_key = input.escrow.terms().coordinator;
    let inputs = std::slice::from_ref(input);
    delete_intent(transport, inputs, coordinator_key, players, coordinator).await
}

/// Sign a delete proof over `inputs`, and hand it to arkd.
async fn delete_intent<T: ArkTransport + ?Sized>(
    transport: &T,
    inputs: &[EscrowInput],
    coordinator_key: XOnlyPublicKey,
    players: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
) -> Result<bool, Error> {
    let message = IntentMessage::Delete {
        expire_at: unix_now()? + DELETE_PROOF_LIFETIME.as_secs(),
    };
    let encoded = message.encode()?;
    // No outputs: ark-core gives the proof a single empty OP_RETURN, so it pays nothing.
    let mut proof = make_intent(
        |_, _| Ok(Vec::new()),
        |_, _| Err(ark_core::Error::ad_hoc("an escrow has no on-chain inputs")),
        intent_inputs(inputs)?,
        Vec::new(),
        message,
    )?;
    sign_intent_proof(
        &mut proof,
        inputs,
        coordinator_key,
        SigningPurpose::DeleteIntent { message: encoded },
        players,
        coordinator,
    )
    .await?;
    match transport.delete_intent(proof).await {
        Ok(()) => Ok(true),
        Err(Error::NoMatchingIntent(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Each escrow as an intent input, spent through its funding leaf.
fn intent_inputs(inputs: &[EscrowInput]) -> Result<Vec<intent::Input>, Error> {
    inputs
        .iter()
        .map(|input| {
            let escrow = &input.escrow;
            let leaf = (
                escrow.script(EscrowPath::Funding).clone(),
                escrow.control_block(EscrowPath::Funding),
            );
            // The funding leaf has no timelock. Like ark-client, the sequence carries the exit delay.
            let sequence = escrow.terms().exit_delay.to_sequence()?;
            Ok(intent::Input::new(
                input.outpoint,
                sequence,
                None,
                TxOut {
                    value: input.amount,
                    script_pubkey: escrow.script_pubkey(),
                },
                escrow.vtxo_script().scripts().to_vec(),
                leaf,
                false,
                false,
                Vec::new(),
            ))
        })
        .collect()
}

/// The player and coordinator signatures for spending `input`'s funding leaf at `input_index` of `psbt`.
fn funding_leaf_requests(
    purpose: SigningPurpose,
    psbt: &Arc<Psbt>,
    input_index: usize,
    input: &EscrowInput,
    coordinator: XOnlyPublicKey,
) -> Result<[SigningRequest; 2], Error> {
    let leaf_hash = TapLeafHash::from_script(
        input.escrow.script(EscrowPath::Funding),
        LeafVersion::TapScript,
    );
    let sighash = script_spend_sighash(psbt, input_index, leaf_hash)?;
    Ok(
        [input.escrow.terms().player, coordinator].map(|key| SigningRequest {
            purpose: purpose.clone(),
            escrow: input.outpoint,
            key,
            psbt: psbt.clone(),
            input_index,
            leaf_hash,
            sighash,
        }),
    )
}

/// Sign every input of an intent proof over `inputs`, for `purpose`.
///
/// As in BIP322, input 0 spends a message-only output locked like the first escrow, and inputs 1 to n spend the escrows.
async fn sign_intent_proof(
    intent: &mut Intent,
    inputs: &[EscrowInput],
    coordinator_key: XOnlyPublicKey,
    purpose: SigningPurpose,
    players: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
) -> Result<(), Error> {
    let psbt = Arc::new(intent.proof.clone());
    if psbt.inputs.len() != inputs.len() + 1 {
        return Err(Error::Protocol(
            "the intent proof has the wrong inputs".into(),
        ));
    }
    let mut requests = Vec::with_capacity(2 * psbt.inputs.len());
    for input_index in 0..psbt.inputs.len() {
        let escrow = &inputs[input_index.saturating_sub(1)];
        requests.extend(funding_leaf_requests(
            purpose.clone(),
            &psbt,
            input_index,
            escrow,
            coordinator_key,
        )?);
    }
    for (request, signature) in
        collect_signatures(requests, coordinator_key, players, coordinator).await?
    {
        insert_signature(&mut intent.proof, &request, signature);
    }
    Ok(())
}

/// The commitment transaction's output paying exactly `expected`, the pool's `what` output.
fn paid_output(commitment_tx: &Psbt, expected: &TxOut, what: &str) -> Result<OutPoint, Error> {
    let outputs = &commitment_tx.unsigned_tx.output;
    let mut matching = outputs
        .iter()
        .enumerate()
        .filter(|(_, output)| *output == expected);
    match (matching.next(), matching.next()) {
        (Some((vout, _)), None) => Ok(OutPoint::new(
            commitment_tx.unsigned_tx.compute_txid(),
            vout as u32,
        )),
        (None, _) => Err(Error::Unfunded(format!(
            "the commitment transaction does not pay the {what} output"
        ))),
        (Some(_), Some(_)) => Err(Error::Unfunded(format!(
            "the commitment transaction pays the {what} output twice"
        ))),
    }
}

/// The batch's connector tree, checked to spend from the commitment transaction.
///
/// Each forfeit also spends a connector, so it is void unless this commitment transaction confirms.
fn connector_graph(chunks: Vec<TxGraphChunk>, commitment_txid: Txid) -> Result<TxGraph, Error> {
    if chunks.is_empty() {
        return Err(Error::Protocol("the batch sent no connectors".into()));
    }
    let graph = TxGraph::new(chunks)?;
    let root_inputs = &graph.root().unsigned_tx.input;
    if root_inputs.len() != 1 || root_inputs[0].previous_output.txid != commitment_txid {
        return Err(Error::Protocol(
            "the connector tree does not spend from the commitment transaction".into(),
        ));
    }
    Ok(graph)
}

/// Build one forfeit per escrow against the batch's connectors, and sign each.
#[allow(clippy::too_many_arguments)]
async fn sign_forfeits(
    vtxo_inputs: &[intent::Input],
    connectors: &TxGraph,
    connector_txs: &HashMap<Txid, Transaction>,
    info: &Info,
    pool: &PoolFunding,
    commitment_tx: Arc<Transaction>,
    players: &dyn EscrowSigner,
    coordinator: &dyn EscrowSigner,
) -> Result<Vec<Psbt>, Error> {
    let commitment_txid = commitment_tx.compute_txid();
    let mut forfeits = create_and_sign_forfeit_txs(
        |_, _| Ok(Vec::new()),
        vtxo_inputs,
        &connectors.leaves(),
        &info.forfeit_address,
        info.dust,
    )?;
    if forfeits.len() != pool.inputs.len() {
        return Err(Error::Protocol(format!(
            "built {} forfeits for {} escrows",
            forfeits.len(),
            pool.inputs.len()
        )));
    }

    let mut requests = Vec::with_capacity(2 * forfeits.len());
    for forfeit in &forfeits {
        let outpoint = forfeit.unsigned_tx.input[FORFEIT_VTXO_INDEX].previous_output;
        let input = pool
            .inputs
            .iter()
            .find(|input| input.outpoint == outpoint)
            .ok_or_else(|| Error::Protocol(format!("a forfeit spends unknown VTXO {outpoint}")))?;
        let chain = connector_chain(
            forfeit.unsigned_tx.input[0].previous_output.txid,
            connector_txs,
            commitment_txid,
        )?;
        requests.extend(funding_leaf_requests(
            SigningPurpose::Forfeit {
                commitment_tx: commitment_tx.clone(),
                connectors: Arc::new(chain),
            },
            &Arc::new(forfeit.clone()),
            FORFEIT_VTXO_INDEX,
            input,
            pool.coordinator,
        )?);
    }
    for (request, signature) in
        collect_signatures(requests, pool.coordinator, players, coordinator).await?
    {
        let forfeit = forfeits
            .iter_mut()
            .find(|forfeit| {
                forfeit.unsigned_tx.input[FORFEIT_VTXO_INDEX].previous_output == request.escrow
            })
            .expect("every request came from a forfeit");
        insert_signature(forfeit, &request, signature);
    }
    Ok(forfeits)
}

/// The connector transactions from `leaf` up to the one spending the commitment transaction.
fn connector_chain(
    leaf: Txid,
    connector_txs: &HashMap<Txid, Transaction>,
    commitment_txid: Txid,
) -> Result<Vec<Transaction>, Error> {
    let mut chain = Vec::new();
    let mut txid = leaf;
    // A connector tree is never deeper than its transactions.
    for _ in 0..=connector_txs.len() {
        let tx = connector_txs
            .get(&txid)
            .ok_or_else(|| Error::Protocol(format!("connector {txid} is not in the batch")))?;
        chain.push(tx.clone());
        let parent = tx
            .input
            .first()
            .ok_or_else(|| Error::Protocol("a connector has no input".into()))?
            .previous_output
            .txid;
        if parent == commitment_txid {
            return Ok(chain);
        }
        txid = parent;
    }
    Err(Error::Protocol(
        "a connector does not descend from the commitment transaction".into(),
    ))
}

fn unix_now() -> Result<u64, Error> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.as_secs())
        .map_err(|error| Error::Protocol(format!("the clock is before 1970: {error}")))
}
