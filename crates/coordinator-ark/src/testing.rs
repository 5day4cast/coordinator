//! A scripted arkd, for testing a kickoff without a server.
//!
//! The mock checks every signature the way arkd would, from the PSBTs alone.
//! It plays one batch: selection, a connector tree, the commitment transaction, and finalization.
//! Events from an unrelated batch are mixed in, and the kickoff must ignore them.
//! The batch can also create other users' VTXOs, and start signing their VTXO tree.
//!
//! Like arkd, it keeps a registered intent queued until a batch confirms it or a delete proof
//! removes it, and meanwhile refuses offchain spends of its VTXOs with
//! [`Error::VtxoAlreadyRegistered`]. It refuses an offchain spend of a VTXO that expired or was
//! swept with [`Error::VtxoRecoverable`].

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Mutex;

use ark_core::intent::Intent;
use ark_core::server::{
    BatchFailed, BatchFinalizationEvent, BatchFinalizedEvent, BatchStartedEvent,
    BatchTreeEventType, Info, StreamEvent, TreeSigningStartedEvent, TreeTxEvent, VirtualTxOutPoint,
};
use ark_core::TxGraphChunk;
use async_trait::async_trait;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::DisplayHex;
use bitcoin::key::{Keypair, Secp256k1, TweakedPublicKey};
use bitcoin::secp256k1::{Message, PublicKey, SecretKey};
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{
    Address, Amount, Network, OutPoint, Psbt, ScriptBuf, Sequence, TapSighashType, Transaction,
    TxIn, TxOut, Txid, XOnlyPublicKey,
};
use futures::channel::mpsc;
use futures::StreamExt;

use crate::{ArkTransport, Error, EventStream, PoolFunding};

pub const BATCH: &str = "batch-7";
pub const INTENT_ID: &str = "intent-42";

/// A test key from a repeated byte.
pub fn keypair(byte: u8) -> Keypair {
    Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_slice(&[byte; 32]).expect("a valid test key"),
    )
}

pub fn xonly(keypair: &Keypair) -> XOnlyPublicKey {
    keypair.x_only_public_key().0
}

/// A P2TR script for test key `byte`.
pub fn p2tr(byte: u8) -> ScriptBuf {
    ScriptBuf::new_p2tr_tweaked(TweakedPublicKey::dangerous_assume_tweaked(xonly(&keypair(
        byte,
    ))))
}

/// Server parameters like Mutinynet's, with `server` as the signer.
pub fn mock_info(server: &Keypair) -> Info {
    Info {
        version: "mock".into(),
        signer_pk: server.public_key(),
        forfeit_pk: server.public_key(),
        forfeit_address: Address::from_str("tb1qz5zgustrxzztljhfr5pm8s4m0a4v0pzqzct90v")
            .expect("a valid address")
            .require_network(Network::Signet)
            .expect("a signet address"),
        // Shaped like Mutinynet's: after a delay, the server alone can spend a stalled
        // checkpoint. Offchain spends pass through an output made of this and the spent leaf.
        checkpoint_tapscript: bitcoin::script::Builder::new()
            .push_sequence(Sequence::from_512_second_intervals(8))
            .push_opcode(bitcoin::opcodes::all::OP_CSV)
            .push_opcode(bitcoin::opcodes::all::OP_DROP)
            .push_slice(server.x_only_public_key().0.serialize())
            .push_opcode(bitcoin::opcodes::all::OP_CHECKSIG)
            .into_script(),
        network: Network::Signet,
        session_duration: 60,
        unilateral_exit_delay: Sequence::from_512_second_intervals(4),
        boarding_exit_delay: Sequence::from_512_second_intervals(1181),
        utxo_min_amount: None,
        utxo_max_amount: None,
        vtxo_min_amount: None,
        vtxo_max_amount: None,
        dust: Amount::from_sat(330),
        fees: None,
        scheduled_session: None,
        deprecated_signers: Vec::new(),
        service_status: HashMap::new(),
        digest: String::new(),
        max_tx_weight: 40_000,
        max_op_return_outputs: 3,
    }
}

/// How the mock server misbehaves, if at all.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Commitment {
    PaysThePool,
    PaysSomeoneElse,
}

/// A scripted arkd for one batch.
pub struct MockArkd {
    pub commitment: Commitment,
    /// The escrows' funding leaf keys, by outpoint, for checking signatures.
    pub signers: HashMap<OutPoint, [XOnlyPublicKey; 2]>,
    pub amounts: HashMap<OutPoint, Amount>,
    /// Distinguishes this batch's commitment transaction from another mock's.
    pub batch_nonce: u8,
    pub forfeit_script: ScriptBuf,
    pub dust: Amount,
    /// Whether a batch selects the registered intent. Without, it stays queued.
    pub selects: bool,
    /// The cosigners of a VTXO tree the batch builds for other users' outputs. Without, the batch
    /// has no VTXO tree.
    pub vtxo_tree: Option<Vec<PublicKey>>,
    pub sender: mpsc::UnboundedSender<Result<StreamEvent, Error>>,
    pub receiver: Mutex<Option<mpsc::UnboundedReceiver<Result<StreamEvent, Error>>>>,
    pub state: Mutex<MockState>,
}

/// An offchain spend the mock took, so a test can see what it was given.
#[derive(Clone)]
pub struct OffchainSpend {
    pub ark_tx: Psbt,
    pub checkpoints: Vec<Psbt>,
    pub finalized: Vec<Psbt>,
}

/// An intent waiting in the batch queue, with the funding leaf keys of the VTXOs it spends.
#[derive(Clone)]
pub struct QueuedIntent {
    pub id: String,
    pub signers: HashMap<OutPoint, [XOnlyPublicKey; 2]>,
}

#[derive(Default)]
pub struct MockState {
    /// Intents registered but neither confirmed by a batch nor deleted.
    pub queued: Vec<QueuedIntent>,
    /// The IDs of the intents delete proofs removed, in order.
    pub deleted: Vec<String>,
    /// How many delete proofs were presented, whether or not they matched an intent.
    pub delete_proofs: usize,
    /// The offchain spends submitted, in order, and what was finalized for each.
    pub offchain: Vec<OffchainSpend>,
    /// VTXOs this server lists, by the address whose script they pay. A finalized offchain
    /// spend marks its inputs spent here.
    pub vtxos: Vec<VirtualTxOutPoint>,
    /// VTXOs the server treats as expired whatever it lists, as when its indexer lags.
    pub expired: HashSet<OutPoint>,
    /// The cosigner keys the registered intent listed.
    pub cosigners: Vec<PublicKey>,
    /// The intent's on-chain outputs.
    pub outputs: Option<Vec<TxOut>>,
    pub commitment_txid: Option<Txid>,
    pub connector_txid: Option<Txid>,
    pub forfeits: Vec<Psbt>,
}

impl MockArkd {
    /// A server that runs `pool`'s batch, with `info`'s forfeit address and dust.
    pub fn new(pool: &PoolFunding, info: &Info, commitment: Commitment) -> Self {
        let (sender, receiver) = mpsc::unbounded();
        let signers = pool
            .inputs()
            .iter()
            .map(|input| {
                let terms = input.escrow.terms();
                (input.outpoint, [terms.player, terms.coordinator])
            })
            .collect();
        let amounts = pool
            .inputs()
            .iter()
            .map(|input| (input.outpoint, input.amount))
            .collect();
        Self {
            commitment,
            signers,
            amounts,
            batch_nonce: 0,
            forfeit_script: info.forfeit_address.script_pubkey(),
            dust: info.dust,
            selects: true,
            vtxo_tree: None,
            sender,
            receiver: Mutex::new(Some(receiver)),
            state: Mutex::default(),
        }
    }

    /// A server that runs no batch: it takes offchain spends and lists VTXOs, as an escrow's
    /// funding check and refund need.
    pub fn offchain(info: &Info) -> Self {
        let (sender, receiver) = mpsc::unbounded();
        Self {
            commitment: Commitment::PaysThePool,
            signers: HashMap::new(),
            amounts: HashMap::new(),
            batch_nonce: 0,
            forfeit_script: info.forfeit_address.script_pubkey(),
            dust: info.dust,
            selects: true,
            vtxo_tree: None,
            sender,
            receiver: Mutex::new(Some(receiver)),
            state: Mutex::default(),
        }
    }

    /// List a VTXO at the encoded Ark `address`, worth `amount`, created at `created_at` (UNIX
    /// seconds).
    pub fn add_vtxo(
        &self,
        address: &str,
        outpoint: OutPoint,
        amount: Amount,
        created_at: i64,
        is_spent: bool,
    ) {
        let script = ark_core::ArkAddress::decode(address)
            .expect("an Ark address")
            .to_p2tr_script_pubkey();
        self.state.lock().unwrap().vtxos.push(VirtualTxOutPoint {
            outpoint,
            created_at,
            expires_at: created_at + 30 * 24 * 60 * 60,
            amount,
            script,
            is_preconfirmed: true,
            is_swept: false,
            is_unrolled: false,
            is_spent,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
            depth: 0,
        });
    }

    /// Queue an intent spending `signers`' VTXOs, as a kickoff that never finished leaves one.
    pub fn queue_intent(&self, id: &str, signers: HashMap<OutPoint, [XOnlyPublicKey; 2]>) {
        self.state.lock().unwrap().queued.push(QueuedIntent {
            id: id.into(),
            signers,
        });
    }

    /// A server whose batches never select the intent, so it stays queued.
    pub fn never_selecting(mut self) -> Self {
        self.selects = false;
        self
    }

    /// A server whose batch also creates other users' VTXOs, and asks `cosigners` to sign
    /// their VTXO tree.
    pub fn sharing_a_vtxo_tree(mut self, cosigners: Vec<PublicKey>) -> Self {
        self.vtxo_tree = Some(cosigners);
        self
    }

    /// The IDs of the intents still queued.
    pub fn queued(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .queued
            .iter()
            .map(|intent| intent.id.clone())
            .collect()
    }

    /// A later batch, whose commitment transaction differs from the first mock's.
    pub fn with_batch_nonce(mut self, nonce: u8) -> Self {
        self.batch_nonce = nonce;
        self
    }

    pub fn send(&self, event: StreamEvent) {
        self.sender.unbounded_send(Ok(event)).unwrap();
    }

    pub fn forfeits(&self) -> Vec<Psbt> {
        self.state.lock().unwrap().forfeits.clone()
    }

    /// Check that `keys` signed input `index` of `psbt` through its only leaf, as arkd does.
    pub fn check_leaf_signatures(psbt: &Psbt, index: usize, keys: [XOnlyPublicKey; 2]) {
        let input = &psbt.inputs[index];
        let (_, (script, version)) = input.tap_scripts.iter().next().expect("a leaf script");
        let leaf_hash = bitcoin::TapLeafHash::from_script(script, *version);
        let prevouts: Vec<TxOut> = psbt
            .inputs
            .iter()
            .map(|input| input.witness_utxo.clone().unwrap())
            .collect();
        let sighash = SighashCache::new(&psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(
                index,
                &Prevouts::All(&prevouts),
                leaf_hash,
                TapSighashType::Default,
            )
            .unwrap();
        let message = Message::from_digest(sighash.to_byte_array());
        let secp = Secp256k1::verification_only();
        for key in keys {
            let signature = input
                .tap_script_sigs
                .get(&(key, leaf_hash))
                .unwrap_or_else(|| panic!("input {index} lacks a signature from {key}"));
            secp.verify_schnorr(&signature.signature, &message, &key)
                .unwrap_or_else(|_| panic!("input {index} has a bad signature from {key}"));
        }
    }
}

#[async_trait]
impl ArkTransport for MockArkd {
    async fn register_intent(&self, intent: Intent) -> Result<String, Error> {
        let proof = &intent.proof;
        let outputs = proof.unsigned_tx.output.clone();
        let indexes: Vec<String> = (0..outputs.len()).map(|index| index.to_string()).collect();
        assert!(intent.serialize_message().unwrap().contains(&format!(
            r#""onchain_output_indexes":[{}]"#,
            indexes.join(",")
        )));
        // Input 0 is the BIP322 message input, locked like the first escrow.
        let first = proof.unsigned_tx.input[1].previous_output;
        MockArkd::check_leaf_signatures(proof, 0, self.signers[&first]);
        for index in 1..proof.inputs.len() {
            let outpoint = proof.unsigned_tx.input[index].previous_output;
            MockArkd::check_leaf_signatures(proof, index, self.signers[&outpoint]);
        }
        {
            let mut state = self.state.lock().unwrap();
            let spent = proof.unsigned_tx.input[1..]
                .iter()
                .map(|input| input.previous_output)
                .collect::<Vec<_>>();
            // arkd pushes an intent only if no queued intent spends the same VTXOs.
            if state.queued.iter().any(|queued| {
                spent
                    .iter()
                    .any(|outpoint| queued.signers.contains_key(outpoint))
            }) {
                return Err(Error::Protocol(
                    "duplicated input, already registered by another intent".into(),
                ));
            }
            state.cosigners = listed_cosigners(&intent.serialize_message()?);
            state.outputs = Some(outputs);
            state.queued.push(QueuedIntent {
                id: INTENT_ID.into(),
                signers: spent
                    .iter()
                    .map(|outpoint| (*outpoint, self.signers[outpoint]))
                    .collect(),
            });
        }
        if !self.selects {
            return Ok(INTENT_ID.into());
        }

        // Another batch's traffic, which the kickoff must ignore.
        self.send(StreamEvent::BatchStarted(BatchStartedEvent {
            id: "batch-6".into(),
            intent_id_hashes: vec!["00".repeat(32)],
            batch_expiry: Sequence::from_512_second_intervals(100),
        }));
        self.send(StreamEvent::BatchFailed(BatchFailed {
            id: "batch-6".into(),
            reason: "not ours".into(),
        }));
        let hash = sha256::Hash::hash(INTENT_ID.as_bytes()).to_byte_array();
        self.send(StreamEvent::BatchStarted(BatchStartedEvent {
            id: BATCH.into(),
            intent_id_hashes: vec!["11".repeat(32), hash.to_lower_hex_string()],
            batch_expiry: Sequence::from_512_second_intervals(100),
        }));
        Ok(INTENT_ID.into())
    }

    /// Deletes every queued intent spending an input of the proof, as arkd does.
    ///
    /// The proof must pay nothing and prove a `delete` message, and each input the mock knows
    /// the keys of must carry both funding leaf signatures.
    async fn delete_intent(&self, proof: Intent) -> Result<(), Error> {
        let message = proof.serialize_message()?;
        assert!(
            message.starts_with(r#"{"type":"delete","#),
            "a delete proof proves a delete message, not {message}"
        );
        let psbt = &proof.proof;
        assert_eq!(
            psbt.unsigned_tx.output,
            vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::new_op_return([]),
            }],
            "a delete proof pays nothing"
        );
        let mut state = self.state.lock().unwrap();
        state.delete_proofs += 1;
        let keys = |outpoint: &OutPoint| {
            state
                .queued
                .iter()
                .find_map(|queued| queued.signers.get(outpoint))
                .or_else(|| self.signers.get(outpoint))
                .copied()
        };
        // Input 0 is the BIP322 message input, locked like the first escrow.
        let first = psbt.unsigned_tx.input[1].previous_output;
        if let Some(keys) = keys(&first) {
            MockArkd::check_leaf_signatures(psbt, 0, keys);
        }
        let mut spent = Vec::new();
        for index in 1..psbt.inputs.len() {
            let outpoint = psbt.unsigned_tx.input[index].previous_output;
            if let Some(keys) = keys(&outpoint) {
                MockArkd::check_leaf_signatures(psbt, index, keys);
            }
            spent.push(outpoint);
        }
        let (matching, kept) = std::mem::take(&mut state.queued)
            .into_iter()
            .partition::<Vec<_>, _>(|queued| {
                spent
                    .iter()
                    .any(|outpoint| queued.signers.contains_key(outpoint))
            });
        state.queued = kept;
        if matching.is_empty() {
            return Err(Error::NoMatchingIntent(
                "INVALID_INTENT_PROOF (23): no matching intents found for intent proof".into(),
            ));
        }
        state
            .deleted
            .extend(matching.into_iter().map(|queued| queued.id));
        Ok(())
    }

    /// Co-signs an offchain spend, as the server does between the owner's two signatures.
    ///
    /// The owner must have signed the Ark transaction already, since the server is second on it.
    /// A VTXO a queued intent spends is refused, as arkd refuses it.
    async fn submit_offchain(
        &self,
        ark_tx: Psbt,
        checkpoints: Vec<Psbt>,
    ) -> Result<crate::OffchainSubmission, Error> {
        assert!(
            !ark_tx.inputs[0].tap_script_sigs.is_empty(),
            "the owner signs the Ark transaction before submitting it"
        );
        let mut state = self.state.lock().unwrap();
        let held = checkpoints
            .iter()
            .flat_map(|checkpoint| &checkpoint.unsigned_tx.input)
            .any(|input| {
                state
                    .queued
                    .iter()
                    .any(|queued| queued.signers.contains_key(&input.previous_output))
            });
        if held {
            return Err(Error::VtxoAlreadyRegistered(
                "VTXO_ALREADY_REGISTERED (4): vtxo(s) already registered".into(),
            ));
        }
        // arkd spends a VTXO offchain only until it expires: `vtxo.Swept || vtxo.IsExpired()`.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_secs() as i64;
        let recoverable = checkpoints
            .iter()
            .flat_map(|checkpoint| &checkpoint.unsigned_tx.input)
            .map(|input| input.previous_output)
            .find(|outpoint| {
                state.expired.contains(outpoint)
                    || state.vtxos.iter().any(|vtxo| {
                        vtxo.outpoint == *outpoint && (vtxo.is_swept || vtxo.expires_at <= now)
                    })
            });
        if let Some(outpoint) = recoverable {
            return Err(Error::VtxoRecoverable(format!(
                "VTXO_RECOVERABLE (8): {outpoint} is recoverable"
            )));
        }
        state.offchain.push(OffchainSpend {
            ark_tx: ark_tx.clone(),
            checkpoints: checkpoints.clone(),
            finalized: Vec::new(),
        });
        Ok(crate::OffchainSubmission {
            ark_tx,
            checkpoints,
        })
    }

    async fn finalize_offchain(&self, ark_txid: Txid, checkpoints: Vec<Psbt>) -> Result<(), Error> {
        for checkpoint in &checkpoints {
            assert!(
                !checkpoint.inputs[0].tap_script_sigs.is_empty(),
                "the owner signs the checkpoint before finalizing it"
            );
        }
        let mut state = self.state.lock().unwrap();
        let spend = state
            .offchain
            .iter_mut()
            .find(|spend| spend.ark_tx.unsigned_tx.compute_txid() == ark_txid)
            .expect("finalizing a spend this server took");
        spend.finalized = checkpoints.clone();
        for checkpoint in &checkpoints {
            let checkpoint_txid = checkpoint.unsigned_tx.compute_txid();
            for input in &checkpoint.unsigned_tx.input {
                if let Some(vtxo) = state
                    .vtxos
                    .iter_mut()
                    .find(|vtxo| vtxo.outpoint == input.previous_output)
                {
                    vtxo.is_spent = true;
                    vtxo.spent_by = Some(checkpoint_txid);
                    vtxo.ark_txid = Some(ark_txid);
                }
            }
        }
        Ok(())
    }

    /// The VTXOs at `addresses`, spent or not, as arkd's indexer lists them.
    async fn vtxos(&self, addresses: Vec<String>) -> Result<Vec<VirtualTxOutPoint>, Error> {
        let scripts = addresses
            .iter()
            .map(|address| {
                ark_core::ArkAddress::decode(address)
                    .map(|address| address.to_p2tr_script_pubkey())
                    .map_err(|error| Error::ServerInfo(format!("invalid Ark address: {error}")))
            })
            .collect::<Result<HashSet<_>, _>>()?;
        Ok(self
            .state
            .lock()
            .unwrap()
            .vtxos
            .iter()
            .filter(|vtxo| scripts.contains(&vtxo.script))
            .cloned()
            .collect())
    }

    async fn confirm_registration(&self, intent_id: String) -> Result<(), Error> {
        assert_eq!(intent_id, INTENT_ID);
        let mut state = self.state.lock().unwrap();
        // The batch took the intent out of the queue when it selected it.
        state.queued.retain(|queued| queued.id != intent_id);
        let mut outputs = state.outputs.clone().unwrap();
        if self.commitment == Commitment::PaysSomeoneElse {
            outputs[0].script_pubkey = p2tr(66);
        }
        // arkd puts the VTXO tree's batch output first.
        if self.vtxo_tree.is_some() {
            outputs.insert(
                0,
                TxOut {
                    value: Amount::from_sat(5_000),
                    script_pubkey: p2tr(72),
                },
            );
        }
        let connector_vout = outputs.len() as u32;
        let escrows = self.signers.len() as u64;
        outputs.extend([
            TxOut {
                value: self.dust * escrows,
                script_pubkey: p2tr(70),
            },
            ark_core::anchor_output(),
        ]);
        let commitment = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(
                    Txid::from_byte_array([0xcc; 32]),
                    3 + u32::from(self.batch_nonce),
                ),
                ..Default::default()
            }],
            output: outputs,
        };
        let connectors = Transaction {
            version: Version::non_standard(3),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(commitment.compute_txid(), connector_vout),
                ..Default::default()
            }],
            output: (0..escrows)
                .map(|_| TxOut {
                    value: self.dust,
                    script_pubkey: p2tr(71),
                })
                .chain([ark_core::anchor_output()])
                .collect(),
        };
        state.commitment_txid = Some(commitment.compute_txid());
        state.connector_txid = Some(connectors.compute_txid());

        if let Some(cosigners) = &self.vtxo_tree {
            let leaf = Transaction {
                version: Version::non_standard(3),
                lock_time: LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: OutPoint::new(commitment.compute_txid(), 0),
                    ..Default::default()
                }],
                output: vec![
                    TxOut {
                        value: Amount::from_sat(5_000),
                        script_pubkey: p2tr(73),
                    },
                    ark_core::anchor_output(),
                ],
            };
            self.send(StreamEvent::TreeTx(TreeTxEvent {
                id: BATCH.into(),
                topic: Vec::new(),
                batch_tree_event_type: BatchTreeEventType::Vtxo,
                tx_graph_chunk: TxGraphChunk {
                    txid: Some(leaf.compute_txid()),
                    tx: Psbt::from_unsigned_tx(leaf).unwrap(),
                    children: HashMap::new(),
                },
            }));
            self.send(StreamEvent::TreeSigningStarted(TreeSigningStartedEvent {
                id: BATCH.into(),
                // Like arkd, it asks every selected intent's cosigners.
                cosigners_pubkeys: cosigners.iter().chain(&state.cosigners).copied().collect(),
                unsigned_commitment_tx: Psbt::from_unsigned_tx(commitment.clone()).unwrap(),
            }));
        }

        // A connector tree from the unrelated batch comes first.
        let mut stray = connectors.clone();
        stray.input[0].previous_output.vout = 9;
        for (id, tx) in [("batch-6", stray), (BATCH, connectors)] {
            self.send(StreamEvent::TreeTx(TreeTxEvent {
                id: id.into(),
                topic: Vec::new(),
                batch_tree_event_type: BatchTreeEventType::Connector,
                tx_graph_chunk: TxGraphChunk {
                    txid: Some(tx.compute_txid()),
                    tx: Psbt::from_unsigned_tx(tx).unwrap(),
                    children: HashMap::new(),
                },
            }));
        }
        self.send(StreamEvent::Heartbeat);
        self.send(StreamEvent::BatchFinalization(BatchFinalizationEvent {
            id: BATCH.into(),
            commitment_tx: Psbt::from_unsigned_tx(commitment).unwrap(),
        }));
        Ok(())
    }

    async fn event_stream(&self, topics: Vec<String>) -> Result<EventStream<'_>, Error> {
        for outpoint in self.signers.keys() {
            assert!(topics.contains(&outpoint.to_string()));
        }
        let receiver = self
            .receiver
            .lock()
            .unwrap()
            .take()
            .expect("one subscription");
        Ok(receiver.boxed())
    }

    async fn submit_forfeits(&self, forfeits: Vec<Psbt>) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        let connector_txid = state.connector_txid.unwrap();
        let mut spent = HashSet::new();
        for forfeit in &forfeits {
            let tx = &forfeit.unsigned_tx;
            assert_eq!(tx.input[0].previous_output.txid, connector_txid);
            let escrow = tx.input[1].previous_output;
            assert!(spent.insert(escrow), "two forfeits for {escrow}");
            assert_eq!(tx.output[0].script_pubkey, self.forfeit_script);
            assert_eq!(tx.output[0].value, self.amounts[&escrow] + self.dust);
            MockArkd::check_leaf_signatures(forfeit, 1, self.signers[&escrow]);
        }
        assert_eq!(spent.len(), self.signers.len());
        state.forfeits = forfeits;
        let commitment_txid = state.commitment_txid.unwrap();
        self.send(StreamEvent::BatchFinalized(BatchFinalizedEvent {
            id: BATCH.into(),
            commitment_txid,
        }));
        Ok(())
    }
}

/// The keys in a register message's `cosigners_public_keys`.
fn listed_cosigners(message: &str) -> Vec<PublicKey> {
    let (_, listed) = message
        .split_once(r#""cosigners_public_keys":["#)
        .expect("a register message lists its cosigners");
    let (listed, _) = listed.split_once(']').unwrap();
    listed
        .split(',')
        .filter(|key| !key.is_empty())
        .map(|key| PublicKey::from_str(key.trim_matches('"')).unwrap())
        .collect()
}
