//! A scripted arkd, for testing a kickoff without a server.
//!
//! The mock checks every signature the way arkd would, from the PSBTs alone.
//! It plays one batch: selection, a connector tree, the commitment transaction, and finalization.
//! Events from an unrelated batch are mixed in, and the kickoff must ignore them.
//! The batch can also create other users' VTXOs, and start signing their VTXO tree.
//!
//! An intent that names no on-chain output is a recovery: it spends one swept VTXO into one new
//! VTXO. Its batch has no connectors and takes no forfeit. It builds a VTXO tree of one leaf, asks
//! the intent's cosigner for its nonces and signatures, and finalizes once it has them. The
//! partial signatures are taken, not verified: ark-core does not expose what that needs.
//!
//! Like arkd, it keeps a registered intent queued until a batch confirms it or a delete proof
//! removes it, and meanwhile refuses offchain spends of its VTXOs with
//! [`Error::VtxoAlreadyRegistered`]. It refuses an offchain spend of a VTXO that expired or was
//! swept with [`Error::VtxoRecoverable`].

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Mutex;

use ark_core::batch::generate_nonce_tree;
use ark_core::intent::Intent;
use ark_core::server::{
    BatchFailed, BatchFinalizationEvent, BatchFinalizedEvent, BatchStartedEvent,
    BatchTreeEventType, Info, NoncePks, PartialSigTree, StreamEvent, TreeNoncesEvent,
    TreeSigningStartedEvent, TreeTxEvent, TreeTxNoncePks, VirtualTxOutPoint,
};
use ark_core::{TxGraph, TxGraphChunk};
use async_trait::async_trait;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::DisplayHex;
use bitcoin::key::{Keypair, Secp256k1, TweakedPublicKey};
use bitcoin::script::Instruction;
use bitcoin::secp256k1::rand::rngs::OsRng;
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
    /// The server's signer key, which signs no proof.
    pub server: XOnlyPublicKey,
    /// Where events go: the latest subscription. Each recovery subscribes anew.
    pub sender: Mutex<mpsc::UnboundedSender<Result<StreamEvent, Error>>>,
    pub receiver: Mutex<Option<mpsc::UnboundedReceiver<Result<StreamEvent, Error>>>>,
    pub state: Mutex<MockState>,
}

/// The server's own cosigner of a recovery's VTXO tree.
fn tree_cosigner() -> Keypair {
    keypair(74)
}

/// An offchain spend the mock took, so a test can see what it was given.
#[derive(Clone)]
pub struct OffchainSpend {
    pub ark_tx: Psbt,
    pub checkpoints: Vec<Psbt>,
    pub finalized: Vec<Psbt>,
}

/// An intent waiting in the batch queue, with the VTXOs it spends, and for each the keys that
/// sign a proof over it through the leaf its own proof used.
#[derive(Clone)]
pub struct QueuedIntent {
    pub id: String,
    pub signers: HashMap<OutPoint, Vec<XOnlyPublicKey>>,
}

/// A recovery the server was asked for: a swept VTXO, given back as a new one.
#[derive(Clone)]
pub struct MockRecovery {
    /// The swept VTXO the intent spends.
    pub outpoint: OutPoint,
    /// The intent's only output.
    pub paid: TxOut,
    pub cosigner: PublicKey,
    /// The batch's VTXO tree, a single leaf, and its commitment transaction, once a batch
    /// selected the intent.
    pub tree: Option<(Psbt, Psbt)>,
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
    /// The recovery the registered intent asks for, until its batch finalizes.
    pub recovery: Option<MockRecovery>,
    /// The recoveries whose batch finalized: the swept VTXO, and the new one that replaced it.
    pub recovered: Vec<(OutPoint, OutPoint)>,
    /// How many batches selected an intent, to tell their commitment transactions apart.
    pub batches: u32,
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
            server: info.signer_pk.x_only_public_key().0,
            sender: Mutex::new(sender),
            receiver: Mutex::new(Some(receiver)),
            state: Mutex::default(),
        }
    }

    /// A server that runs no kickoff: it takes offchain spends and lists VTXOs, as an escrow's
    /// funding check and refund need, and recovers a swept VTXO in a batch.
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
            server: info.signer_pk.x_only_public_key().0,
            sender: Mutex::new(sender),
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
            signers: signers
                .into_iter()
                .map(|(outpoint, keys)| (outpoint, keys.to_vec()))
                .collect(),
        });
    }

    /// Mark the listed VTXO at `outpoint` as expired at `expires_at` (UNIX seconds), and swept
    /// or not.
    pub fn expire_vtxo(&self, outpoint: OutPoint, expires_at: i64, swept: bool) {
        let mut state = self.state.lock().unwrap();
        let vtxo = state
            .vtxos
            .iter_mut()
            .find(|vtxo| vtxo.outpoint == outpoint)
            .expect("a listed VTXO");
        vtxo.expires_at = expires_at;
        vtxo.is_swept = swept;
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
        // A subscriber that has gone, as after a recovery returned, misses nothing it needs.
        let _ = self.sender.lock().unwrap().unbounded_send(Ok(event));
    }

    pub fn forfeits(&self) -> Vec<Psbt> {
        self.state.lock().unwrap().forfeits.clone()
    }

    /// Check input `index` of a proof as arkd does, knowing only the PSBT: its one leaf must be
    /// in the tree of the output it spends, and every key in that leaf but the server's must
    /// have signed. Returns those keys.
    pub fn check_tapscript_signatures(&self, psbt: &Psbt, index: usize) -> Vec<XOnlyPublicKey> {
        let input = &psbt.inputs[index];
        let mut leaves = input.tap_scripts.iter();
        let (Some((control_block, (script, _))), None) = (leaves.next(), leaves.next()) else {
            panic!("input {index} needs exactly one leaf script");
        };
        let prevout = input.witness_utxo.as_ref().expect("a witness UTXO");
        assert!(
            prevout.script_pubkey.is_p2tr(),
            "input {index} is not taproot"
        );
        let output_key = XOnlyPublicKey::from_slice(&prevout.script_pubkey.as_bytes()[2..])
            .expect("a taproot output key");
        assert!(
            control_block.verify_taproot_commitment(
                &Secp256k1::verification_only(),
                output_key,
                script
            ),
            "input {index}'s leaf is not in the tree of the output it spends"
        );
        let keys: Vec<XOnlyPublicKey> = script
            .instructions()
            .filter_map(|instruction| match instruction {
                Ok(Instruction::PushBytes(bytes)) if bytes.len() == 32 => {
                    XOnlyPublicKey::from_slice(bytes.as_bytes()).ok()
                }
                _ => None,
            })
            .filter(|key| *key != self.server)
            .collect();
        assert!(!keys.is_empty(), "input {index}'s leaf names no signer");
        MockArkd::check_leaf_signatures(psbt, index, &keys);
        keys
    }

    /// Check that `keys` signed input `index` of `psbt` through its only leaf, as arkd does.
    pub fn check_leaf_signatures(psbt: &Psbt, index: usize, keys: &[XOnlyPublicKey]) {
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
                .get(&(*key, leaf_hash))
                .unwrap_or_else(|| panic!("input {index} lacks a signature from {key}"));
            secp.verify_schnorr(&signature.signature, &message, key)
                .unwrap_or_else(|_| panic!("input {index} has a bad signature from {key}"));
        }
    }

    /// Tell the intent's owner that a batch selected it, amid another batch's traffic.
    fn announce_batch(&self) {
        // Another batch's traffic, which the intent's owner must ignore.
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
    }

    /// Queue a recovery: an intent spending one swept VTXO into one new VTXO.
    ///
    /// arkd takes such an intent without a forfeit, since it already holds a swept VTXO's
    /// coins. It checks the proof's signatures from the PSBT alone, whatever leaf it spends.
    fn register_recovery(&self, intent: &Intent, message: &str) -> Result<String, Error> {
        let proof = &intent.proof;
        assert_eq!(
            proof.inputs.len(),
            2,
            "a recovery spends the message input and one VTXO"
        );
        let outpoint = proof.unsigned_tx.input[1].previous_output;
        let [paid] = proof.unsigned_tx.output.as_slice() else {
            panic!("a recovery pays one output");
        };
        assert!(paid.script_pubkey.is_p2tr(), "a VTXO's script is taproot");
        let signers = self.check_tapscript_signatures(proof, 1);
        assert_eq!(self.check_tapscript_signatures(proof, 0), signers);
        let cosigners = listed_cosigners(message);
        let [cosigner] = cosigners.as_slice() else {
            panic!("an intent that receives a VTXO lists its cosigner");
        };
        {
            let mut state = self.state.lock().unwrap();
            let vtxo = state
                .vtxos
                .iter()
                .find(|vtxo| vtxo.outpoint == outpoint)
                .expect("a recovery spends a VTXO this server lists");
            if vtxo.is_spent {
                return Err(Error::Protocol(format!(
                    "VTXO_ALREADY_SPENT (6): input {outpoint} already spent"
                )));
            }
            // An unswept VTXO would need a forfeit, which a recovery does not sign.
            assert!(vtxo.is_swept, "only a swept VTXO is recovered");
            let spent = proof.inputs[1].witness_utxo.as_ref().unwrap();
            assert_eq!(
                (spent.value, &spent.script_pubkey),
                (vtxo.amount, &vtxo.script)
            );
            // This server takes no fee, and cannot pay out more than the VTXO held.
            assert!(paid.value <= vtxo.amount);
            if state
                .queued
                .iter()
                .any(|queued| queued.signers.contains_key(&outpoint))
            {
                return Err(Error::Protocol(
                    "duplicated input, already registered by another intent".into(),
                ));
            }
            state.cosigners = cosigners.clone();
            state.recovery = Some(MockRecovery {
                outpoint,
                paid: paid.clone(),
                cosigner: *cosigner,
                tree: None,
            });
            state.queued.push(QueuedIntent {
                id: INTENT_ID.into(),
                signers: HashMap::from([(outpoint, signers)]),
            });
        }
        if self.selects {
            self.announce_batch();
        }
        Ok(INTENT_ID.into())
    }

    /// Start the batch of a recovery: a commitment transaction, and a VTXO tree of one leaf
    /// that pays the intent's output, for the intent's cosigner and the server's to sign.
    fn start_recovery(&self, state: &mut MockState) {
        state.batches += 1;
        let recovery = state.recovery.as_mut().expect("a recovery");
        let mut paid = recovery.paid.clone();
        if self.commitment == Commitment::PaysSomeoneElse {
            paid.script_pubkey = p2tr(66);
        }
        let commitment = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(
                    Txid::from_byte_array([0xcc; 32]),
                    100 + state.batches,
                ),
                ..Default::default()
            }],
            output: vec![
                // The batch output, which the tree spends.
                TxOut {
                    value: paid.value,
                    script_pubkey: p2tr(72),
                },
                ark_core::anchor_output(),
            ],
        };
        let mut leaf = Psbt::from_unsigned_tx(Transaction {
            version: Version::non_standard(3),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(commitment.compute_txid(), 0),
                ..Default::default()
            }],
            output: vec![paid, ark_core::anchor_output()],
        })
        .unwrap();
        // arkd names a tree transaction's cosigners in its input's PSBT fields.
        let cosigners = [recovery.cosigner, tree_cosigner().public_key()];
        for (index, cosigner) in cosigners.iter().enumerate() {
            let mut key = ark_core::VTXO_COSIGNER_PSBT_KEY.to_vec();
            key.push(index as u8);
            leaf.inputs[0].unknown.insert(
                bitcoin::psbt::raw::Key {
                    type_value: 222,
                    key,
                },
                cosigner.serialize().to_vec(),
            );
        }
        let commitment = Psbt::from_unsigned_tx(commitment).unwrap();
        self.send(StreamEvent::TreeTx(TreeTxEvent {
            id: BATCH.into(),
            topic: Vec::new(),
            batch_tree_event_type: BatchTreeEventType::Vtxo,
            tx_graph_chunk: TxGraphChunk {
                txid: Some(leaf.unsigned_tx.compute_txid()),
                tx: leaf.clone(),
                children: HashMap::new(),
            },
        }));
        self.send(StreamEvent::TreeSigningStarted(TreeSigningStartedEvent {
            id: BATCH.into(),
            cosigners_pubkeys: vec![recovery.cosigner],
            unsigned_commitment_tx: commitment.clone(),
        }));
        recovery.tree = Some((leaf, commitment));
    }
}

#[async_trait]
impl ArkTransport for MockArkd {
    async fn register_intent(&self, intent: Intent) -> Result<String, Error> {
        let message = intent.serialize_message()?;
        if message.contains(r#""onchain_output_indexes":[]"#) {
            return self.register_recovery(&intent, &message);
        }
        let proof = &intent.proof;
        let outputs = proof.unsigned_tx.output.clone();
        let indexes: Vec<String> = (0..outputs.len()).map(|index| index.to_string()).collect();
        assert!(intent.serialize_message().unwrap().contains(&format!(
            r#""onchain_output_indexes":[{}]"#,
            indexes.join(",")
        )));
        // Input 0 is the BIP322 message input, locked like the first escrow.
        let first = proof.unsigned_tx.input[1].previous_output;
        MockArkd::check_leaf_signatures(proof, 0, &self.signers[&first]);
        for index in 1..proof.inputs.len() {
            let outpoint = proof.unsigned_tx.input[index].previous_output;
            MockArkd::check_leaf_signatures(proof, index, &self.signers[&outpoint]);
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
                    .map(|outpoint| (*outpoint, self.signers[outpoint].to_vec()))
                    .collect(),
            });
        }
        if self.selects {
            self.announce_batch();
        }
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
        // A delete proof spends an escrow through its funding leaf, whatever leaf the intent
        // it deletes used, so both funding keys sign it when this server knows them.
        let keys = |outpoint: &OutPoint| {
            self.signers
                .get(outpoint)
                .map(|keys| keys.to_vec())
                .or_else(|| {
                    state
                        .queued
                        .iter()
                        .find_map(|queued| queued.signers.get(outpoint))
                        .cloned()
                })
        };
        // Input 0 is the BIP322 message input, locked like the first escrow.
        let first = psbt.unsigned_tx.input[1].previous_output;
        if let Some(keys) = keys(&first) {
            MockArkd::check_leaf_signatures(psbt, 0, &keys);
        }
        let mut spent = Vec::new();
        for index in 1..psbt.inputs.len() {
            let outpoint = psbt.unsigned_tx.input[index].previous_output;
            if let Some(keys) = keys(&outpoint) {
                MockArkd::check_leaf_signatures(psbt, index, &keys);
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
        // A recovery that was queued goes with its intent.
        if state
            .recovery
            .as_ref()
            .is_some_and(|recovery| spent.contains(&recovery.outpoint))
        {
            state.recovery = None;
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
        if state.recovery.is_some() {
            self.start_recovery(&mut state);
            return Ok(());
        }
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
        // The first subscription takes the events sent so far. A later one, as each recovery
        // makes, starts a stream of its own.
        let receiver = self.receiver.lock().unwrap().take().unwrap_or_else(|| {
            let (sender, receiver) = mpsc::unbounded();
            *self.sender.lock().unwrap() = sender;
            receiver
        });
        Ok(receiver.boxed())
    }

    /// Answers a recovery's cosigner with every cosigner's nonce for the tree's one transaction:
    /// its own, and the server's.
    async fn submit_tree_nonces(
        &self,
        batch_id: &str,
        cosigner: PublicKey,
        nonces: NoncePks,
    ) -> Result<(), Error> {
        assert_eq!(batch_id, BATCH);
        let state = self.state.lock().unwrap();
        let recovery = state.recovery.as_ref().expect("a recovery's batch");
        assert_eq!(cosigner, recovery.cosigner);
        let (leaf, commitment) = recovery.tree.clone().expect("a tree being signed");
        let txid = leaf.unsigned_tx.compute_txid();
        let own = nonces
            .get(&txid)
            .expect("a nonce for the tree's transaction");
        let graph = TxGraph::new(vec![TxGraphChunk {
            txid: Some(txid),
            tx: leaf,
            children: HashMap::new(),
        }])?;
        let server = tree_cosigner().public_key();
        let servers = generate_nonce_tree(&mut OsRng, &graph, server, &commitment)?
            .to_nonce_pks()
            .get(&txid)
            .expect("the server's nonce");
        self.send(StreamEvent::TreeNonces(TreeNoncesEvent {
            id: BATCH.into(),
            topic: Vec::new(),
            txid,
            nonces: TreeTxNoncePks::new(HashMap::from([
                (cosigner.x_only_public_key().0, own),
                (server.x_only_public_key().0, servers),
            ])),
        }));
        Ok(())
    }

    /// Takes the cosigner's partial signature, and finalizes the recovery's batch: the swept
    /// VTXO is settled, and the new one listed.
    async fn submit_tree_signatures(
        &self,
        batch_id: &str,
        cosigner: PublicKey,
        signatures: PartialSigTree,
    ) -> Result<(), Error> {
        assert_eq!(batch_id, BATCH);
        let mut state = self.state.lock().unwrap();
        let recovery = state.recovery.take().expect("a recovery's batch");
        assert_eq!(cosigner, recovery.cosigner);
        let (leaf, commitment) = recovery.tree.expect("a tree being signed");
        let txid = leaf.unsigned_tx.compute_txid();
        assert_eq!(
            signatures.0.keys().collect::<Vec<_>>(),
            vec![&txid],
            "one partial signature, for the tree's transaction"
        );
        let commitment_txid = commitment.unsigned_tx.compute_txid();
        let swept = state
            .vtxos
            .iter_mut()
            .find(|vtxo| vtxo.outpoint == recovery.outpoint)
            .expect("the swept VTXO");
        swept.is_spent = true;
        swept.settled_by = Some(commitment_txid);
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_secs() as i64;
        let paid = &leaf.unsigned_tx.output[0];
        let vtxo = OutPoint::new(txid, 0);
        state.vtxos.push(VirtualTxOutPoint {
            outpoint: vtxo,
            created_at,
            expires_at: created_at + 7 * 24 * 60 * 60,
            amount: paid.value,
            script: paid.script_pubkey.clone(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: vec![commitment_txid],
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
            depth: 0,
        });
        state.recovered.push((recovery.outpoint, vtxo));
        state.commitment_txid = Some(commitment_txid);
        self.send(StreamEvent::BatchFinalization(BatchFinalizationEvent {
            id: BATCH.into(),
            commitment_tx: commitment,
        }));
        self.send(StreamEvent::BatchFinalized(BatchFinalizedEvent {
            id: BATCH.into(),
            commitment_txid,
        }));
        Ok(())
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
            MockArkd::check_leaf_signatures(forfeit, 1, &self.signers[&escrow]);
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
