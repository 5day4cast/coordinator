//! A scripted arkd, for testing a kickoff without a server.
//!
//! The mock checks every signature the way arkd would, from the PSBTs alone.
//! It plays one batch: selection, a connector tree, the commitment transaction, and finalization.
//! Events from an unrelated batch are mixed in, and the kickoff must ignore them.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Mutex;

use ark_core::intent::Intent;
use ark_core::server::{
    BatchFailed, BatchFinalizationEvent, BatchFinalizedEvent, BatchStartedEvent,
    BatchTreeEventType, Info, StreamEvent, TreeTxEvent,
};
use ark_core::TxGraphChunk;
use async_trait::async_trait;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::DisplayHex;
use bitcoin::key::{Keypair, Secp256k1, TweakedPublicKey};
use bitcoin::secp256k1::{Message, SecretKey};
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
    pub sender: mpsc::UnboundedSender<Result<StreamEvent, Error>>,
    pub receiver: Mutex<Option<mpsc::UnboundedReceiver<Result<StreamEvent, Error>>>>,
    pub state: Mutex<MockState>,
}

#[derive(Default)]
pub struct MockState {
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
            sender,
            receiver: Mutex::new(Some(receiver)),
            state: Mutex::default(),
        }
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
        self.state.lock().unwrap().outputs = Some(outputs);

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

    async fn confirm_registration(&self, intent_id: String) -> Result<(), Error> {
        assert_eq!(intent_id, INTENT_ID);
        let mut state = self.state.lock().unwrap();
        let mut outputs = state.outputs.clone().unwrap();
        if self.commitment == Commitment::PaysSomeoneElse {
            outputs[0].script_pubkey = p2tr(66);
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
