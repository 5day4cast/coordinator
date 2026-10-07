//! A player recovers a two-player contract from their nsec, the records the coordinator
//! published, and the oracle's attestation: nothing else. The contract is built and signed with
//! dlctix as the coordinator would; the tool sees only the published events and the recovery file.

use std::collections::BTreeMap;
use std::io::Write;

use base64::Engine;
use bitcoin::hashes::Hash;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::{schnorr, Message};
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::taproot::LeafVersion;
use bitcoin::{
    Amount, CompressedPublicKey, EcdsaSighashType, FeeRate, Network, OutPoint, PrivateKey,
    ScriptBuf, TapLeafHash, TapSighashType, Transaction, TxOut, Txid, XOnlyPublicKey,
};
use coordinator_recover::chain::{Outspend, TxStatus};
use coordinator_recover::fees::{AnchorBumper, BumpStatus, FeeCoin};
use coordinator_recover::inspect::{Action, Location};
use coordinator_recover::{Error, Identity, Session, WalletSeed};
use dlctix::hashlock::{self, Preimage};
use dlctix::musig2::{PartialSignature, PubNonce};
use dlctix::secp::{MaybePoint, MaybeScalar, Point, Scalar};
use dlctix::{
    AnchorParams, ContractParameters, EventLockingConditions, MarketMaker, NonceSharingRound,
    Outcome, PayoutWeights, Player, SigMap, SignedContract, SigningSession, TicketedDLC,
    WinCondition,
};
use nostr::nips::nip44;
use nostr::{EventBuilder, Keys, Kind, Tag};
use serde_json::{json, Value};
use uuid::Uuid;

const DELTA: u16 = 72;
const EXPIRY: u32 = 1_700_000_000;

fn random_scalar() -> Scalar {
    loop {
        if let Ok(scalar) = Scalar::from_slice(&rand::random::<[u8; 32]>()) {
            return scalar;
        }
    }
}

/// Everything the coordinator and oracle know; the test hands the tool only what they publish.
struct World {
    player: Keys,
    coordinator: Keys,
    entry_id: Uuid,
    competition_id: Uuid,
    seed_hex: String,
    ticket_preimage: Preimage,
    signed: SignedContract,
    /// Outcome 0 pays this player, outcome 1 the other.
    attestations: [MaybeScalar; 2],
}

fn world() -> World {
    world_with(None)
}

/// As [`world`], with anchor outputs on the outcome, expiry and split transactions.
fn world_with(anchor: Option<AnchorParams>) -> World {
    let mut rng = rand::rng();
    let player = Keys::generate();
    let coordinator = Keys::generate();
    let seed: [u8; 32] = rand::random();
    let seed_hex = hex::encode(seed);
    let entry_id = Uuid::now_v7();
    let wallet = WalletSeed::from_backup(&format!("coordinator-wallet-v1:{seed_hex}")).unwrap();
    let key = wallet.entry_key(Network::Signet, entry_id).unwrap();
    let ticket_preimage = hashlock::preimage_random(&mut rng);

    let market_maker = random_scalar();
    let other = random_scalar();
    let players = vec![
        Player {
            pubkey: key.point(),
            ticket_hash: hashlock::sha256(&ticket_preimage),
            payout_hash: key.payout_hash(),
        },
        Player {
            pubkey: other.base_point_mul(),
            ticket_hash: hashlock::sha256(&hashlock::preimage_random(&mut rng)),
            payout_hash: hashlock::sha256(&hashlock::preimage_random(&mut rng)),
        },
    ];
    let oracle = random_scalar();
    let nonce = random_scalar();
    let messages = [b"player wins".as_slice(), b"other wins".as_slice()];
    let locking_points: Vec<MaybePoint> = messages
        .iter()
        .map(|message| {
            dlctix::attestation_locking_point(
                oracle.base_point_mul(),
                nonce.base_point_mul(),
                message,
            )
        })
        .collect();
    let attestations = messages.map(|message| dlctix::attestation_secret(oracle, nonce, message));
    let params = ContractParameters {
        market_maker: MarketMaker {
            pubkey: market_maker.base_point_mul(),
        },
        players,
        event: EventLockingConditions {
            locking_points,
            expiry: Some(EXPIRY),
        },
        outcome_payouts: BTreeMap::from([
            (Outcome::Attestation(0), PayoutWeights::from([(0, 1)])),
            (Outcome::Attestation(1), PayoutWeights::from([(1, 1)])),
            (Outcome::Expiry, PayoutWeights::from([(0, 1), (1, 1)])),
        ]),
        fee_rate: FeeRate::from_sat_per_vb(10).unwrap(),
        funding_value: Amount::from_sat(200_000),
        relative_locktime_block_delta: DELTA,
        anchor,
        outcome_bound_splits: false,
    };
    let funding = OutPoint::new(Txid::from_byte_array([7; 32]), 0);
    let dlc = TicketedDLC::new(params, funding).unwrap();
    let signed = musig_sign(&dlc, [key.scalar(), other, market_maker]);
    World {
        player,
        coordinator,
        entry_id,
        competition_id: Uuid::now_v7(),
        seed_hex,
        ticket_preimage,
        signed,
        attestations,
    }
}

/// The signing rounds every player runs with the market maker (dlctix's own test helper).
fn musig_sign(dlc: &TicketedDLC, keys: [Scalar; 3]) -> SignedContract {
    let mut rng = rand::rng();
    let mut sessions: BTreeMap<Point, SigningSession<NonceSharingRound>> = keys
        .into_iter()
        .map(|key| {
            (
                key.base_point_mul(),
                SigningSession::new(dlc.clone(), &mut rng, key).unwrap(),
            )
        })
        .collect();
    let nonces: BTreeMap<Point, SigMap<PubNonce>> = sessions
        .iter()
        .map(|(key, session)| (*key, session.our_public_nonces().clone()))
        .collect();
    let coordinator = sessions
        .remove(&dlc.params().market_maker.pubkey)
        .unwrap()
        .aggregate_nonces_and_compute_partial_signatures(nonces)
        .unwrap();
    let players: Vec<_> = sessions
        .into_values()
        .map(|session| {
            session
                .compute_partial_signatures(coordinator.aggregated_nonces().clone())
                .unwrap()
        })
        .collect();
    let partials: BTreeMap<Point, SigMap<PartialSignature>> = players
        .iter()
        .map(|session| {
            (
                session.our_public_key(),
                session.our_partial_signatures().clone(),
            )
        })
        .collect();
    coordinator.aggregate_all_signatures(partials).unwrap()
}

impl World {
    fn blind(&self) -> String {
        Identity::from_nsec(&self.player.secret_key().to_secret_hex())
            .unwrap()
            .blind_tag(&self.coordinator.public_key())
    }

    fn wallet_content(&self) -> String {
        let blob = nip44::encrypt(
            self.player.secret_key(),
            &self.player.public_key(),
            format!("coordinator-wallet-v1:{}", self.seed_hex),
            nip44::Version::V2,
        )
        .unwrap();
        self.to_player(&json!({"v": 1, "type": "wallet", "network": "signet", "wallet_blob": blob}))
    }

    fn entry_record(&self, entry_pubkey: &str, preimage: Option<&Preimage>) -> Value {
        json!({
            "v": 1, "type": "entry", "network": "signet",
            "coordinator_pubkey": self.coordinator.public_key().to_hex(),
            "user_pubkey": self.player.public_key().to_hex(),
            "competition_id": self.competition_id,
            "entry_id": self.entry_id,
            "entry_pubkey": entry_pubkey,
            "status": "in_contract",
            "escrow": null,
            "ticket": {
                "hash": hex::encode(hashlock::sha256(&self.ticket_preimage)),
                "preimage": preimage.map(hex::encode),
            },
            "contract": {
                "funding_outpoint": self.signed.dlc().funding_outpoint().to_string(),
                "player_index": 0,
                "contract_parameters_sha256": "",
                "pruned_signatures": self.signed.pruned_signatures(self.our_point()),
                "relative_locktime_block_delta": DELTA,
            },
            "updated_at": 1,
        })
    }

    fn our_point(&self) -> Point {
        self.signed.params().players[0].pubkey
    }

    fn competition(&self, attestation: Option<MaybeScalar>) -> Value {
        json!({
            "v": 1, "type": "competition", "network": "signet",
            "competition_id": self.competition_id,
            "contract_parameters": self.signed.params(),
            "signed_contract": self.signed,
            "funding_outpoint": self.signed.dlc().funding_outpoint().to_string(),
            "funding_tx": null,
            "oracle": {"pubkey": "", "event_id": self.competition_id, "expiry": EXPIRY},
            "attestation": attestation.map(|a| hex::encode(a.serialize())),
        })
    }

    fn to_player(&self, value: &Value) -> String {
        nip44::encrypt(
            self.coordinator.secret_key(),
            &self.player.public_key(),
            value.to_string(),
            nip44::Version::V2,
        )
        .unwrap()
    }

    fn event(&self, d: String, tag: (&str, String), content: String) -> nostr::Event {
        EventBuilder::new(Kind::from(30078u16), content)
            .tags([
                Tag::parse(["d".to_owned(), d]).unwrap(),
                Tag::parse([tag.0.to_owned(), tag.1]).unwrap(),
            ])
            .sign_with_keys(&self.coordinator)
            .unwrap()
    }

    /// The coordinator's published events for this player and competition.
    fn events(&self, entry: Value, attestation: Option<MaybeScalar>) -> Vec<nostr::Event> {
        let blind = self.blind();
        vec![
            self.event(
                format!("{blind}:wallet"),
                ("b", blind.clone()),
                self.wallet_content(),
            ),
            self.event(
                format!("{blind}:entry:{}", self.entry_id),
                ("b", blind.clone()),
                self.to_player(&entry),
            ),
            self.event(
                format!("competition:{}", self.competition_id),
                ("c", self.competition_id.to_string()),
                self.competition(attestation).to_string(),
            ),
        ]
    }

    /// A session holding only the player's nsec and the published events.
    fn session(&self, events: Vec<nostr::Event>) -> Session {
        let identity = Identity::from_nsec(&self.player.secret_key().to_secret_hex()).unwrap();
        let mut session = Session::new(identity, Network::Signet);
        session.set_coordinator(self.coordinator.public_key());
        session.add_events(events);
        session.load().unwrap();
        session
    }

    fn funding_output(&self) -> TxOut {
        self.signed.dlc().funding_output()
    }
}

fn destination() -> ScriptBuf {
    let key =
        XOnlyPublicKey::from_slice(&random_scalar().base_point_mul().serialize_xonly()).unwrap();
    ScriptBuf::new_p2tr(&Secp256k1::new(), key, None)
}

/// A key-path signature on `tx`'s only input, checked against the output key of `prevout`.
fn assert_key_spend(tx: &Transaction, prevout: &TxOut) {
    let sighash = SighashCache::new(tx)
        .taproot_key_spend_signature_hash(0, &Prevouts::All(&[prevout]), TapSighashType::Default)
        .unwrap();
    let key = XOnlyPublicKey::from_slice(&prevout.script_pubkey.as_bytes()[2..34]).unwrap();
    let signature = schnorr::Signature::from_slice(&tx.input[0].witness[0]).unwrap();
    Secp256k1::verification_only()
        .verify_schnorr(
            &signature,
            &Message::from_digest(sighash.to_byte_array()),
            &key,
        )
        .expect("the outcome transaction's signature must verify against the funding key");
}

fn unspent() -> Outspend {
    Outspend {
        spent_by: None,
        confirmed_height: None,
    }
}

fn spent(by: Txid, height: u32) -> Outspend {
    Outspend {
        spent_by: Some(by),
        confirmed_height: Some(height),
    }
}

#[test]
fn claims_a_win_from_the_nsec_records_and_attestation_alone() {
    let world = world();
    let entry = world.entry_record(
        &hex::encode(world.our_point().serialize()),
        Some(&world.ticket_preimage),
    );
    let mut session = world.session(world.events(entry, Some(world.attestations[0])));
    let funding = world.signed.dlc().funding_outpoint();

    // The funding output is unspent and the oracle has attested: the outcome transaction now.
    session.chain.set_tip(1_000, u64::from(EXPIRY) - 10_000);
    session.chain.insert_tx(
        funding.txid,
        Some(TxStatus {
            confirmed_height: Some(900),
        }),
    );
    session.chain.insert_outspend(funding, unspent());
    let reports = session.inspect(0);
    assert!(session.chain.missing().is_empty());
    // The split cannot go with it: its input waits for `delta` confirmations of the outcome.
    assert_eq!(reports[0].now, [Action::BroadcastOutcome]);
    let next = reports[0].next.as_ref().unwrap();
    assert_eq!(
        (next.action, next.at_height),
        (Action::BroadcastSplit, None)
    );
    assert!(matches!(
        reports[0].location,
        Location::Funding {
            confirmed: true,
            ..
        }
    ));

    let plan = session
        .claim(
            world.entry_id,
            Some(destination()),
            FeeRate::from_sat_per_vb(2).unwrap(),
            None,
        )
        .unwrap();
    let labels: Vec<&str> = plan.txs.iter().map(|step| step.label).collect();
    assert_eq!(labels, ["outcome"]);
    let outcome_tx = plan.txs[0].tx.clone();
    assert_eq!(
        outcome_tx,
        world
            .signed
            .signed_outcome_tx(0, world.attestations[0])
            .unwrap()
    );
    assert_key_spend(&outcome_tx, &world.funding_output());
    assert!(plan.txs[0].bump.describe().contains("cannot be fee bumped"));
    assert!(plan.waiting.as_deref().unwrap().contains(&format!(
        "{DELTA} blocks after the outcome transaction confirms"
    )));
    // Until it confirms, the expiry transaction can take its place from the expiry on.
    assert_eq!(
        plan.deadline.as_ref().unwrap().time,
        Some(u64::from(EXPIRY))
    );

    // The outcome confirmed: the split opens `delta` blocks after it, and the claim says when.
    let outcome_txid = outcome_tx.compute_txid();
    let split_opens = 1_001 + u32::from(DELTA);
    session
        .chain
        .insert_outspend(funding, spent(outcome_txid, 1_001));
    session
        .chain
        .insert_outspend(OutPoint::new(outcome_txid, 0), unspent());
    session.chain.set_tip(1_001, 0);
    session.chain.set_block_interval(30);
    let report = &session.inspect(0)[0];
    assert!(report.now.is_empty());
    let next = report.next.as_ref().unwrap();
    assert_eq!(
        (next.action, next.at_height, next.blocks_left),
        (
            Action::BroadcastSplit,
            Some(split_opens),
            Some(split_opens - 1_002)
        )
    );
    assert_eq!(next.eta, Some(u64::from(split_opens - 1_002) * 30));
    assert!(report
        .to_string()
        .contains(&format!("from block {split_opens} (in")));
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains(&format!("block {}", 1_001 + 2 * u32::from(DELTA)))));
    let plan = session
        .claim(
            world.entry_id,
            Some(destination()),
            FeeRate::from_sat_per_vb(2).unwrap(),
            None,
        )
        .unwrap();
    assert!(plan.txs.is_empty(), "nodes refuse the split until it opens");
    assert!(plan
        .waiting
        .as_deref()
        .unwrap()
        .contains(&format!("block {split_opens}")));
    assert_eq!(
        plan.deadline.as_ref().unwrap().height,
        Some(1_001 + 2 * u32::from(DELTA))
    );

    // From the block before it opens, the split goes out.
    session.chain.set_tip(split_opens - 1, 0);
    assert_eq!(session.inspect(0)[0].now, [Action::BroadcastSplit]);
    let plan = session
        .claim(
            world.entry_id,
            Some(destination()),
            FeeRate::from_sat_per_vb(2).unwrap(),
            None,
        )
        .unwrap();
    let labels: Vec<&str> = plan.txs.iter().map(|step| step.label).collect();
    assert_eq!(labels, ["split"]);
    let win = WinCondition {
        outcome: Outcome::Attestation(0),
        player_index: 0,
    };
    let split_tx = plan.txs[0].tx.clone();
    assert_eq!(
        split_tx,
        world
            .signed
            .signed_split_tx(&win, world.ticket_preimage)
            .unwrap()
    );
    assert_eq!(split_tx.input[0].previous_output.txid, outcome_txid);
    assert_eq!(
        split_tx.input[0].sequence,
        bitcoin::Sequence::from_height(DELTA)
    );

    // The split confirmed: the claim waits delta blocks after it.
    let split_at = split_opens;
    let split_txid = split_tx.compute_txid();
    session
        .chain
        .insert_outspend(OutPoint::new(outcome_txid, 0), spent(split_txid, split_at));
    session.chain.set_tip(split_at, 0);
    let _ = session.inspect(0);
    for query in session.chain.missing() {
        match query {
            coordinator_recover::chain::Query::Outspend(outpoint) => {
                session.chain.insert_outspend(outpoint, unspent())
            }
            coordinator_recover::chain::Query::Tx(txid) => session.chain.insert_tx(txid, None),
        }
    }
    let report = &session.inspect(0)[0];
    assert!(report.now.is_empty());
    let next = report.next.as_ref().unwrap();
    assert_eq!(
        (next.action, next.at_height),
        (Action::ClaimWin, Some(split_at + u32::from(DELTA)))
    );
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains(&format!("block {}", split_at + 2 * u32::from(DELTA)))));
    let plan = session
        .claim(
            world.entry_id,
            Some(destination()),
            FeeRate::from_sat_per_vb(2).unwrap(),
            None,
        )
        .unwrap();
    assert!(plan.txs.is_empty());
    assert!(plan
        .waiting
        .unwrap()
        .contains(&(split_at + u32::from(DELTA)).to_string()));

    // Delta blocks later the win transaction pays the player's address, signed by the entry key.
    session.chain.set_tip(split_at + u32::from(DELTA) - 1, 0);
    assert_eq!(session.inspect(0)[0].now, [Action::ClaimWin]);
    let to = destination();
    let plan = session
        .claim(
            world.entry_id,
            Some(to.clone()),
            FeeRate::from_sat_per_vb(2).unwrap(),
            None,
        )
        .unwrap();
    assert_eq!(plan.txs.len(), 1);
    let win_tx = &plan.txs[0].tx;
    assert_eq!(win_tx.output[0].script_pubkey, to);
    let (input, prevout) = world.signed.split_win_tx_input_and_prevout(&win).unwrap();
    assert_eq!(win_tx.input[0].previous_output, input.previous_output);
    assert_eq!(
        win_tx.input[0].sequence,
        bitcoin::Sequence::from_height(DELTA)
    );
    let witness: Vec<&[u8]> = win_tx.input[0].witness.iter().collect();
    assert_eq!(witness[1], world.ticket_preimage);
    let sighash = SighashCache::new(win_tx)
        .taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(&[prevout]),
            TapLeafHash::from_script(
                bitcoin::Script::from_bytes(witness[2]),
                LeafVersion::TapScript,
            ),
            TapSighashType::Default,
        )
        .unwrap();
    Secp256k1::verification_only()
        .verify_schnorr(
            &schnorr::Signature::from_slice(witness[0]).unwrap(),
            &Message::from_digest(sighash.to_byte_array()),
            &XOnlyPublicKey::from_slice(&world.our_point().serialize_xonly()).unwrap(),
        )
        .expect("the win transaction is signed with the derived entry key");
    let fee = prevout.value - win_tx.output[0].value;
    assert!(fee >= Amount::from_sat(100) && fee < Amount::from_sat(1_000));

    // Once the claim is on chain there is nothing left to do.
    session.chain.insert_outspend(
        input.previous_output,
        spent(win_tx.compute_txid(), split_at + u32::from(DELTA)),
    );
    let report = &session.inspect(0)[0];
    assert!(matches!(report.location, Location::Spent { .. }));
    assert!(report.now.is_empty());
}

#[test]
fn takes_the_expiry_path_when_the_oracle_never_attests() {
    let world = world();
    let entry = world.entry_record(&hex::encode(world.our_point().serialize()), None);
    let mut session = world.session(world.events(entry, None));
    let funding = world.signed.dlc().funding_outpoint();
    session.chain.insert_tx(
        funding.txid,
        Some(TxStatus {
            confirmed_height: Some(10),
        }),
    );
    session.chain.insert_outspend(funding, unspent());

    // Before expiry: wait, and say when.
    session.chain.set_tip(100, u64::from(EXPIRY) - 1);
    let report = &session.inspect(0)[0];
    assert!(report.now.is_empty());
    let next = report.next.as_ref().unwrap();
    assert_eq!(next.action, Action::BroadcastExpiry);
    assert_eq!(next.at_time, Some(u64::from(EXPIRY) + 1));

    // After expiry: the expiry transaction; the split, which needs the ticket preimage, opens
    // `delta` blocks after it confirms.
    session.chain.set_tip(101, u64::from(EXPIRY) + 1);
    let report = &session.inspect(0)[0];
    assert_eq!(report.now, [Action::BroadcastExpiry]);
    assert_eq!(report.next.as_ref().unwrap().action, Action::BroadcastSplit);
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("ticket preimage")));
    let plan = session
        .claim(
            world.entry_id,
            None,
            FeeRate::from_sat_per_vb(2).unwrap(),
            None,
        )
        .unwrap();
    assert_eq!(plan.txs.len(), 1);
    assert_eq!(plan.txs[0].label, "expiry");
    let expiry_tx = plan.txs[0].tx.clone();
    assert_eq!(expiry_tx, world.signed.expiry_tx().unwrap());
    assert_eq!(expiry_tx.lock_time.to_consensus_u32(), EXPIRY);
    assert_key_spend(&expiry_tx, &world.funding_output());
    let waiting = plan.waiting.unwrap();
    assert!(waiting.contains("after the expiry transaction confirms"));
    assert!(waiting.contains("ticket preimage"));
    // Nothing can replace the expiry transaction, so there is no deadline yet.
    assert!(plan.deadline.is_none());

    // Once it has `delta` confirmations, the split goes out with the preimage given.
    let expiry_txid = expiry_tx.compute_txid();
    session
        .chain
        .insert_outspend(funding, spent(expiry_txid, 102));
    session
        .chain
        .insert_outspend(OutPoint::new(expiry_txid, 0), unspent());
    session
        .chain
        .set_tip(102 + u32::from(DELTA) - 1, u64::from(EXPIRY) + 1);
    let preimage = hex::encode(world.ticket_preimage);
    let plan = session
        .claim(
            world.entry_id,
            None,
            FeeRate::from_sat_per_vb(2).unwrap(),
            Some(&preimage),
        )
        .unwrap();
    let win = WinCondition {
        outcome: Outcome::Expiry,
        player_index: 0,
    };
    assert_eq!(plan.txs.len(), 1);
    assert_eq!(
        plan.txs[0].tx,
        world
            .signed
            .signed_split_tx(&win, world.ticket_preimage)
            .unwrap()
    );
    // A wrong preimage is refused rather than producing an invalid split.
    assert!(session
        .claim(
            world.entry_id,
            None,
            FeeRate::from_sat_per_vb(2).unwrap(),
            Some(&"00".repeat(32))
        )
        .is_err());
}

#[test]
fn refuses_to_sign_when_the_record_names_another_entry_key() {
    let world = world();
    let foreign = hex::encode(random_scalar().base_point_mul().serialize());
    let entry = world.entry_record(&foreign, Some(&world.ticket_preimage));
    let mut session = world.session(world.events(entry, Some(world.attestations[0])));
    let funding = world.signed.dlc().funding_outpoint();
    session.chain.set_tip(1_000, 0);
    session.chain.insert_tx(
        funding.txid,
        Some(TxStatus {
            confirmed_height: Some(900),
        }),
    );
    session.chain.insert_outspend(funding, unspent());

    let entry = session.entry(world.entry_id).unwrap();
    assert!(
        matches!(session.entry_key(entry), Err(Error::ForeignEntry(id)) if id == world.entry_id)
    );
    assert!(matches!(session.contract(world.entry_id), Some(Err(_))));
    let report = &session.inspect(0)[0];
    assert!(report.now.is_empty());
    assert!(
        matches!(&report.location, Location::Unknown { reason } if reason.contains("not created by this wallet"))
    );
    assert!(session
        .claim(
            world.entry_id,
            Some(destination()),
            FeeRate::from_sat_per_vb(2).unwrap(),
            None
        )
        .is_err());
}

#[test]
fn reads_the_recovery_file_with_a_compressed_competition() {
    let world = world();
    let entry = world.entry_record(
        &hex::encode(world.our_point().serialize()),
        Some(&world.ticket_preimage),
    );
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(
            world
                .competition(Some(world.attestations[1]))
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let compressed = base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap());
    let kit = json!({
        "type": "coordinator-recovery-kit", "v": 1, "network": "signet", "created_at": 0,
        "coordinator_pubkey": world.coordinator.public_key().to_hex(),
        "user_pubkey": world.player.public_key().to_hex(),
        "relays": ["wss://relay.example"],
        "wallet": world.wallet_content(),
        "entries": [world.to_player(&entry)],
        "competitions": [{"v": 1, "type": "competition", "encoding": "gzip+base64", "data": compressed}],
    });

    let identity = Identity::from_nsec(&world.player.secret_key().to_secret_hex()).unwrap();
    let mut session = Session::new(identity, Network::Signet);
    session
        .add_kit(coordinator_recover::spec::Kit::parse(&kit.to_string()).unwrap())
        .unwrap();
    assert_eq!(
        session.coordinator().unwrap(),
        world.coordinator.public_key()
    );
    assert_eq!(session.kit_relays(), ["wss://relay.example"]);
    session.load().unwrap();
    assert!(session.warnings.is_empty(), "{:?}", session.warnings);
    assert_eq!(
        session.attestation(world.competition_id),
        Some(world.attestations[1])
    );

    // The attested outcome pays the other player, so there is nothing to claim.
    let funding = world.signed.dlc().funding_outpoint();
    session.chain.set_tip(1_000, 0);
    session.chain.insert_tx(
        funding.txid,
        Some(TxStatus {
            confirmed_height: Some(900),
        }),
    );
    session.chain.insert_outspend(funding, unspent());
    let report = &session.inspect(0)[0];
    assert!(matches!(&report.location, Location::Lost { outcome } if outcome == "att1"));
    assert!(report.now.is_empty());

    // Another account's nsec cannot use the file.
    let stranger = Identity::from_nsec(&Keys::generate().secret_key().to_secret_hex()).unwrap();
    let mut session = Session::new(stranger, Network::Signet);
    assert!(session
        .add_kit(coordinator_recover::spec::Kit::parse(&kit.to_string()).unwrap())
        .is_err());
}

#[test]
fn ignores_records_signed_by_anyone_but_the_coordinator() {
    let world = world();
    let entry = world.entry_record(&hex::encode(world.our_point().serialize()), None);
    let mut events = world.events(entry, None);
    // An impostor's newer wallet record for the same blind tag is skipped.
    let impostor = Keys::generate();
    let blind = world.blind();
    events.push(
        EventBuilder::new(Kind::from(30078u16), "garbage")
            .tags([
                Tag::parse(["d".to_owned(), format!("{blind}:wallet")]).unwrap(),
                Tag::parse(["b".to_owned(), blind]).unwrap(),
            ])
            .sign_with_keys(&impostor)
            .unwrap(),
    );
    let session = world.session(events);
    assert_eq!(session.entries().count(), 1);
    assert!(session.warnings.is_empty(), "{:?}", session.warnings);
}

/// A player's own coin for CPFP children: P2WPKH or single-key P2TR, worth 100,000 sats.
fn fee_coin(taproot: bool) -> (FeeCoin, TxOut) {
    let secp = Secp256k1::new();
    let key = PrivateKey::new(
        bitcoin::secp256k1::SecretKey::from_slice(&random_scalar().serialize()).unwrap(),
        Network::Signet,
    );
    let script_pubkey = if taproot {
        ScriptBuf::new_p2tr(
            &secp,
            key.public_key(&secp).inner.x_only_public_key().0,
            None,
        )
    } else {
        ScriptBuf::new_p2wpkh(
            &CompressedPublicKey::from_private_key(&secp, &key)
                .unwrap()
                .wpubkey_hash(),
        )
    };
    let prevout = TxOut {
        value: Amount::from_sat(100_000),
        script_pubkey,
    };
    let outpoint = OutPoint::new(Txid::from_byte_array([9; 32]), 1);
    let coin = FeeCoin::new(outpoint, prevout.clone(), key, Some(destination())).unwrap();
    (coin, prevout)
}

/// `child` spends `parent`'s anchor with an empty witness and the fee coin with a valid
/// signature, and the two together pay at least `rate`.
fn assert_cpfp(
    parent: &Transaction,
    parent_fee: Amount,
    child: &Transaction,
    coin: &TxOut,
    rate: FeeRate,
) {
    let (anchor, anchor_output) = dlctix::anchor::find_anchor(parent).unwrap();
    assert_eq!(child.input.len(), 2);
    assert_eq!(child.input[0].previous_output, anchor);
    assert!(child.input[0].witness.is_empty());
    assert_eq!(child.output.len(), 1);
    let child_fee = anchor_output.value + coin.value - child.output[0].value;
    let package_vsize = parent.weight().to_vbytes_ceil() + child.weight().to_vbytes_ceil();
    assert!(parent_fee + child_fee >= rate.fee_vb(package_vsize).unwrap());
    // The child alone pays at least the rate too, and not much more than the package needs.
    assert!(child_fee >= rate.fee_vb(child.weight().to_vbytes_ceil()).unwrap());
    assert!(parent_fee + child_fee <= rate.fee_vb(package_vsize + 10).unwrap());

    let secp = Secp256k1::verification_only();
    let prevouts = [anchor_output.clone(), coin.clone()];
    let mut cache = SighashCache::new(child);
    if coin.script_pubkey.is_p2wpkh() {
        let sighash = cache
            .p2wpkh_signature_hash(1, &coin.script_pubkey, coin.value, EcdsaSighashType::All)
            .unwrap();
        let witness: Vec<&[u8]> = child.input[1].witness.iter().collect();
        let pubkey = bitcoin::PublicKey::from_slice(witness[1]).unwrap();
        assert_eq!(
            ScriptBuf::new_p2wpkh(&pubkey.wpubkey_hash().unwrap()),
            coin.script_pubkey
        );
        let signature = bitcoin::ecdsa::Signature::from_slice(witness[0]).unwrap();
        secp.verify_ecdsa(&Message::from(sighash), &signature.signature, &pubkey.inner)
            .expect("the fee coin's P2WPKH signature must verify");
    } else {
        let sighash = cache
            .taproot_key_spend_signature_hash(1, &Prevouts::All(&prevouts), TapSighashType::Default)
            .unwrap();
        let key = XOnlyPublicKey::from_slice(&coin.script_pubkey.as_bytes()[2..34]).unwrap();
        let signature = schnorr::Signature::from_slice(&child.input[1].witness[0]).unwrap();
        secp.verify_schnorr(&signature, &Message::from(sighash), &key)
            .expect("the fee coin's key-path signature must verify");
    }
}

/// An anchored contract's outcome and split transactions pay 10 sat/vB; the fee market wants
/// 50. The player's coin pays a child for each, the outcome's before it confirms too.
#[test]
fn bumps_an_anchored_outcome_and_split_with_a_child_from_the_players_coin() {
    let world = world_with(Some(AnchorParams::default()));
    let entry = world.entry_record(
        &hex::encode(world.our_point().serialize()),
        Some(&world.ticket_preimage),
    );
    let mut session = world.session(world.events(entry, Some(world.attestations[0])));
    let funding = world.signed.dlc().funding_outpoint();
    session.chain.set_tip(1_000, u64::from(EXPIRY) - 10_000);
    session.chain.insert_tx(
        funding.txid,
        Some(TxStatus {
            confirmed_height: Some(900),
        }),
    );
    session.chain.insert_outspend(funding, unspent());
    let rate = FeeRate::from_sat_per_vb(50).unwrap();
    let check = |step: &coordinator_recover::ClaimTx, taproot: bool| {
        let parent = &step.tx;
        assert!(matches!(
            step.bump,
            BumpStatus::Anchor {
                value_sat: 240,
                fee_sat: Some(_),
                ..
            }
        ));
        assert!(step.bump.describe().contains("can be fee bumped"));
        let parent_fee = world.signed.presigned_tx_fee(parent).unwrap();
        assert_eq!(step.bump.below(parent, rate), Some(parent_fee));
        // At or below the contract's own rate there is nothing to bump.
        assert_eq!(
            step.bump
                .below(parent, FeeRate::from_sat_per_vb(5).unwrap()),
            None
        );
        let (coin, prevout) = fee_coin(taproot);
        let child = coin.bump(parent, parent_fee, rate).unwrap();
        assert_eq!(child.input[1].previous_output, coin.outpoint());
        assert_cpfp(parent, parent_fee, &child, &prevout, rate);
    };

    // The outcome transaction, bumped as it goes out.
    let plan = session
        .claim(world.entry_id, Some(destination()), rate, None)
        .unwrap();
    assert_eq!(plan.txs.len(), 1);
    assert_eq!(plan.txs[0].label, "outcome");
    check(&plan.txs[0], false);
    let outcome_tx = plan.txs[0].tx.clone();
    let outcome_txid = outcome_tx.compute_txid();

    // Broadcast and not yet confirmed: still bumpable, and the split waits.
    session.chain.insert_outspend(
        funding,
        Outspend {
            spent_by: Some(outcome_txid),
            confirmed_height: None,
        },
    );
    session
        .chain
        .insert_outspend(OutPoint::new(outcome_txid, 0), unspent());
    let plan = session
        .claim(world.entry_id, Some(destination()), rate, None)
        .unwrap();
    assert!(plan.txs.is_empty());
    assert_eq!(plan.unconfirmed.len(), 1);
    assert_eq!(plan.unconfirmed[0].label, "outcome");
    assert_eq!(plan.unconfirmed[0].tx, outcome_tx);
    check(&plan.unconfirmed[0], true);

    // Confirmed and `delta` deep: the split, bumped from a taproot coin.
    session
        .chain
        .insert_outspend(funding, spent(outcome_txid, 1_001));
    session.chain.set_tip(1_001 + u32::from(DELTA) - 1, 0);
    let plan = session
        .claim(world.entry_id, Some(destination()), rate, None)
        .unwrap();
    assert_eq!(plan.txs.len(), 1);
    assert_eq!(plan.txs[0].label, "split");
    check(&plan.txs[0], true);
}

#[test]
fn a_contract_without_anchors_still_cannot_be_bumped() {
    let world = world();
    let entry = world.entry_record(
        &hex::encode(world.our_point().serialize()),
        Some(&world.ticket_preimage),
    );
    let mut session = world.session(world.events(entry, Some(world.attestations[0])));
    let funding = world.signed.dlc().funding_outpoint();
    session.chain.set_tip(1_000, u64::from(EXPIRY) - 10_000);
    session.chain.insert_tx(
        funding.txid,
        Some(TxStatus {
            confirmed_height: Some(900),
        }),
    );
    session.chain.insert_outspend(funding, unspent());
    let rate = FeeRate::from_sat_per_vb(50).unwrap();
    let plan = session
        .claim(world.entry_id, Some(destination()), rate, None)
        .unwrap();
    assert_eq!(plan.txs.len(), 1);
    let (coin, _) = fee_coin(false);
    for step in &plan.txs {
        assert!(matches!(step.bump, BumpStatus::Fixed { fee_sat: Some(_) }));
        assert!(step.bump.describe().contains("cannot be fee bumped"));
        assert_eq!(step.bump.below(&step.tx, rate), None);
        assert!(coin.bump(&step.tx, Amount::ZERO, rate).is_err());
    }
}

/// Without --network, the network is the one the player's records name: Mutinynet players
/// need not pass `--network signet`.
#[test]
fn takes_the_network_from_the_records() {
    let world = world();
    let entry = world.entry_record(&hex::encode(world.our_point().serialize()), None);
    let identity = Identity::from_nsec(&world.player.secret_key().to_secret_hex()).unwrap();
    let mut session = Session::new(identity, Network::Bitcoin);
    assert!(session.recorded_networks().is_empty(), "no coordinator yet");
    session.set_coordinator(world.coordinator.public_key());
    session.add_events(world.events(entry, None));
    assert_eq!(session.recorded_networks(), [Network::Signet]);
    // Read as mainnet, the signet records are refused.
    assert!(matches!(session.load(), Err(Error::NoWallet)));
    session.set_network(Network::Signet);
    session.load().unwrap();
    assert_eq!(session.entries().count(), 1);
    assert!(matches!(session.contract(world.entry_id), Some(Ok(_))));
    // Another coordinator's key finds nothing.
    session.set_coordinator(Keys::generate().public_key());
    assert!(session.recorded_networks().is_empty());
}
