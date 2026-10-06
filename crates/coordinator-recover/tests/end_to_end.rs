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
    Amount, FeeRate, Network, OutPoint, ScriptBuf, TapLeafHash, TapSighashType, Transaction, TxOut,
    Txid, XOnlyPublicKey,
};
use coordinator_recover::chain::{Outspend, TxStatus};
use coordinator_recover::inspect::{Action, Location};
use coordinator_recover::{Error, Identity, Session, WalletSeed};
use dlctix::hashlock::{self, Preimage};
use dlctix::musig2::{PartialSignature, PubNonce};
use dlctix::secp::{MaybePoint, MaybeScalar, Point, Scalar};
use dlctix::{
    ContractParameters, EventLockingConditions, MarketMaker, NonceSharingRound, Outcome,
    PayoutWeights, Player, SigMap, SignedContract, SigningSession, TicketedDLC, WinCondition,
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
        anchor: None,
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

    // The funding output is unspent and the oracle has attested: outcome, then split.
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
    assert_eq!(
        reports[0].now,
        [Action::BroadcastOutcome, Action::BroadcastSplit]
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
    assert_eq!(labels, ["outcome", "split"]);
    let outcome_tx = &plan.txs[0].tx;
    assert_eq!(
        *outcome_tx,
        world
            .signed
            .signed_outcome_tx(0, world.attestations[0])
            .unwrap()
    );
    assert_key_spend(outcome_tx, &world.funding_output());
    let win = WinCondition {
        outcome: Outcome::Attestation(0),
        player_index: 0,
    };
    assert_eq!(
        plan.txs[1].tx,
        world
            .signed
            .signed_split_tx(&win, world.ticket_preimage)
            .unwrap()
    );
    assert_eq!(
        plan.txs[1].tx.input[0].previous_output.txid,
        outcome_tx.compute_txid()
    );
    assert!(plan
        .txs
        .iter()
        .all(|step| step.bump.describe().contains("cannot be fee bumped")));

    // Both confirmed: the claim waits delta blocks after the split.
    let split_txid = plan.txs[1].tx.compute_txid();
    session
        .chain
        .insert_outspend(funding, spent(outcome_tx.compute_txid(), 1_001));
    session.chain.insert_outspend(
        OutPoint::new(outcome_tx.compute_txid(), 0),
        spent(split_txid, 1_002),
    );
    session.chain.set_tip(1_002, 0);
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
        (Action::ClaimWin, Some(1_002 + u32::from(DELTA)))
    );
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains(&format!("block {}", 1_002 + 2 * u32::from(DELTA)))));
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
        .contains(&(1_002 + u32::from(DELTA)).to_string()));

    // Delta blocks later the win transaction pays the player's address, signed by the entry key.
    session.chain.set_tip(1_002 + u32::from(DELTA) - 1, 0);
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
    session
        .chain
        .insert_outspend(input.previous_output, spent(win_tx.compute_txid(), 1_100));
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

    // After expiry: the expiry transaction, then the split, which needs the ticket preimage.
    session.chain.set_tip(101, u64::from(EXPIRY) + 1);
    let report = &session.inspect(0)[0];
    assert_eq!(
        report.now,
        [Action::BroadcastExpiry, Action::BroadcastSplit]
    );
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
    assert_eq!(plan.txs[0].tx, world.signed.expiry_tx().unwrap());
    assert_eq!(plan.txs[0].tx.lock_time.to_consensus_u32(), EXPIRY);
    assert_key_spend(&plan.txs[0].tx, &world.funding_output());
    assert!(plan.waiting.unwrap().contains("ticket preimage"));

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
    assert_eq!(
        plan.txs[1].tx,
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
