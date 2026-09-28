use super::*;
use crate::{nostr::NostrClientCore, wallet::keys::WalletSeed};
use coordinator_ark_escrow::{EntryEscrow, EscrowTerms, RelativeTimelock};
use coordinator_core::keymeld::{payout::ContractAuthorization, ArkEscrowPolicy};
use dlctix::{
    bitcoin::{
        absolute::LockTime,
        hashes::sha256,
        secp256k1::{Secp256k1, SecretKey},
        Amount,
    },
    secp::Scalar,
    EventLockingConditions, MarketMaker,
};
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use time::format_description::well_known::Rfc3339;

/// A named change to one input, which the check must refuse.
type Change<T> = (&'static str, fn(&mut T));

const START: i64 = 1_790_000_000;
const DAY: i64 = 86_400;
const ENTRY_FEE: u64 = 5_000;
const COORDINATOR_FEE: u64 = 150;
const TICKET: u64 = ENTRY_FEE + COORDINATOR_FEE;

fn point(byte: u8) -> Point {
    Scalar::from_slice(&[byte; 32]).unwrap().base_point_mul()
}

fn xonly(point: Point) -> ark::XOnlyPublicKey {
    ark::XOnlyPublicKey::from_slice(&point.serialize_xonly()).unwrap()
}

fn oracle() -> Point {
    point(7)
}

fn market_maker() -> Point {
    point(3)
}

fn rfc3339(unix: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix)
        .unwrap()
        .format(&Rfc3339)
        .unwrap()
}

fn line(target: &str, metric: &str, lower: f64, upper: f64) -> LineTerms {
    LineTerms {
        target: target.into(),
        metric: metric.into(),
        lower,
        upper,
        window_hours: 24,
    }
}

/// The reference event as the oracle serves it: every field of `GET /oracle/events/{id}`, the
/// lines unsorted, with the fields a line's fit adds.
fn reference_event(competition_id: Uuid) -> Value {
    let line = |target: &str, metric: &str, lower: f64, upper: f64| {
        json!({
            "target": target, "metric": metric, "lower": lower, "upper": upper,
            "level": "station", "window_hours": 24, "windows": 60, "over": 20, "par": 20,
            "under": 20, "first_window": rfc3339(START - 60 * DAY),
            "last_window": rfc3339(START - DAY), "fitted_at": rfc3339(START - DAY),
        })
    };
    json!({
        "id": competition_id,
        "signing_date": rfc3339(START + 2 * DAY),
        "start_observation_date": rfc3339(START),
        "end_observation_date": rfc3339(START + DAY),
        "locations": ["KSAW", "KORD"],
        "number_of_values_per_entry": 2,
        "status": "live",
        "total_allowed_entries": 25,
        "entry_ids": [],
        "number_of_places_win": 1,
        "entries": [],
        "source": "noaa_weather",
        "readings": [],
        "weather": [],
        "nonce_point": point(9).to_string(),
        "event_announcement": { "locking_points": [], "expiry": START + 3 * DAY },
        "attestation": null,
        "coordinator_pubkey": "npub1coordinator",
        "scoring_fields": ["temp_high", "temp_low"],
        "unlisted": true,
        "scoring_rules": "lines",
        "lines": [
            line("KSAW", "temp_high", -1.25, 1.75),
            line("KORD", "temp_low", -2.5, -0.0),
            line("KORD", "temp_high", -2.5, -0.5),
        ],
    })
}

/// The terms a queued competition built from [`reference_event`] consents to.
fn terms(competition_id: Uuid) -> queued::QueuedTerms {
    queued::QueuedTerms {
        competition_id,
        network: Network::Signet,
        market_maker: MarketMaker {
            pubkey: market_maker(),
        },
        oracle_pubkey: hex::encode(oracle().serialize_xonly()),
        signing_date: START + 2 * DAY,
        expiry: (START + 3 * DAY) as u32,
        observation: ObservationTerms {
            source: "noaa_weather".into(),
            start_observation_date: START,
            end_observation_date: START + DAY,
            targets: vec!["KSAW".into(), "KORD".into()],
            scoring_fields: vec!["temp_high".into(), "temp_low".into()],
            number_of_values_per_entry: 2,
            scoring_rules: ScoringRules::Lines,
            lines: vec![
                line("KORD", "temp_high", -2.5, -0.5),
                line("KORD", "temp_low", -2.5, -0.0),
                line("KSAW", "temp_high", -1.25, 1.75),
            ],
        },
        number_of_places_win: 1,
        pool_rules: PoolRules::new(2, 25).unwrap(),
        stake_sats: ENTRY_FEE,
        relative_locktime_block_delta: 72,
        max_fee_rate: FeeRate::from_sat_per_vb_u32(10),
    }
}

fn invoice(amount_sats: u64) -> Bolt11Invoice {
    InvoiceBuilder::new(Currency::Signet)
        .description("ticket".into())
        .payment_hash(sha256::Hash::hash(&[1; 32]))
        .payment_secret(PaymentSecret([2; 32]))
        .amount_milli_satoshis(amount_sats * 1000)
        .current_timestamp()
        .min_final_cltv_expiry_delta(18)
        .build_signed(|hash| {
            Secp256k1::new().sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[3; 32]).unwrap())
        })
        .unwrap()
}

/// An escrow like the coordinator's: the player's and market maker's keys, refundable at
/// `refund_at`.
fn escrow_policy(player: Point, coordinator: Point, refund_at: u32) -> ArkEscrowPolicy {
    let refund_locktime = LockTime::from_time(refund_at).unwrap();
    let exit_delay = RelativeTimelock::Seconds(2048);
    let escrow = EntryEscrow::new(EscrowTerms {
        player: xonly(player),
        coordinator: xonly(coordinator),
        server: xonly(point(5)),
        refund_locktime,
        exit_delay,
        unilateral_refund_delay: EscrowTerms::unilateral_refund_delay_for(
            refund_locktime,
            exit_delay,
            refund_at - DAY as u32,
        )
        .unwrap(),
    })
    .unwrap();
    ArkEscrowPolicy {
        escrow_tap_tree: hex::encode(escrow.vtxo_script().encode_tap_tree()),
        max_fee_sats: COORDINATOR_FEE,
        max_refund_fee_sats: 100,
        checkpoint_exit_script: hex::encode([0x51]),
    }
}

struct Fixture {
    key: EntryKey,
    entry_id: Uuid,
    entry: QueuedEntryTerms,
    policy: PayoutPolicy,
    assignment: RegistrationAssignment,
    consent: QueuedConsent,
}

impl Fixture {
    /// A consistent queued entry for `key`, as an honest coordinator and oracle serve it.
    fn new(key: EntryKey, entry_id: Uuid) -> Self {
        let competition_id = Uuid::now_v7();
        let ticket = invoice(TICKET);
        let entry = QueuedEntryTerms {
            terms: terms(competition_id),
            entry_id,
            ticket_hash: ticket.payment_hash().to_byte_array(),
            payout_hash: key.payout_hash(),
        };
        let policy = PayoutPolicy {
            automatic_lightning_address: Some("alice@wallet.example".into()),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            contract_terms: String::new(),
            ark_escrow: Some(escrow_policy(
                key.point(),
                market_maker(),
                entry.terms.expiry,
            )),
            queued_entry: None,
        };
        let consent = QueuedConsent {
            competition_id,
            lightning_address: Some("alice@wallet.example".into()),
            allow_invoice_fallback: true,
            release_entry_key_after_payment: true,
            ticket_invoice: ticket.to_string(),
            ticket_amount_sats: TICKET,
            entry_fee_sats: ENTRY_FEE,
            pool_rules: PoolRules::new(2, 25).unwrap(),
            expected_relative_locktime_delta: 72,
            max_fee_rate_sat_vb: 10,
            oracle_pubkey: BASE64.encode(oracle().serialize()),
            reference_event: reference_event(competition_id).to_string(),
        };
        let assignment = RegistrationAssignment {
            session_id: String::new(),
            user_id: entry_id,
            manifest_hash: vec![],
            enclave_id: 1,
            enclave_key_epoch: 1,
            enclave_public_key: "key".into(),
            gateway_url: "http://127.0.0.1:1".into(),
            trusted_pcrs: BTreeMap::new(),
            dangerous_trust_unattested_enclaves: true,
            payout_policy: None,
        };
        let mut fixture = Self {
            key,
            entry_id,
            entry,
            policy,
            assignment,
            consent,
        };
        fixture.reseal();
        fixture
    }

    fn generated() -> Self {
        let entry_id = Uuid::now_v7();
        let key = WalletSeed::generate()
            .entry_key(&Secp256k1::new(), Network::Signet, entry_id)
            .unwrap();
        Self::new(key, entry_id)
    }

    /// Write the entry into the policy and scope the deposit to its terms, as a coordinator does,
    /// so a changed term is caught by the check for that term and not only by the digest.
    fn reseal(&mut self) {
        let (session, digest) = queued::deposit_scope(&self.entry.terms).unwrap();
        self.assignment.session_id = session.as_string();
        self.assignment.manifest_hash = digest.to_vec();
        self.policy.queued_entry = Some(self.entry.to_json().unwrap());
        self.assignment.payout_policy = Some(serde_json::to_string(&self.policy).unwrap());
    }

    fn check(&self) -> Result<(), WalletError> {
        validate_registration(
            Network::Signet,
            &self.key,
            self.entry_id,
            &self.assignment,
            &self.consent,
        )
    }

    fn with_terms(mut self, change: impl FnOnce(&mut queued::QueuedTerms)) -> Self {
        change(&mut self.entry.terms);
        self.reseal();
        self
    }

    fn with_entry(mut self, change: impl FnOnce(&mut QueuedEntryTerms)) -> Self {
        change(&mut self.entry);
        self.reseal();
        self
    }

    fn with_policy(mut self, change: impl FnOnce(&mut PayoutPolicy)) -> Self {
        change(&mut self.policy);
        self.reseal();
        self
    }

    fn with_event(mut self, change: impl FnOnce(&mut Value)) -> Self {
        let mut event: Value = serde_json::from_str(&self.consent.reference_event).unwrap();
        change(&mut event);
        self.consent.reference_event = event.to_string();
        self
    }
}

#[test]
fn an_entry_matching_the_competition_its_oracle_and_the_ticket_is_accepted() {
    let fixture = Fixture::generated();
    fixture.check().unwrap();
    // The oracle's line order and extra fields don't matter; its values do.
    let event: Value = serde_json::from_str(&fixture.consent.reference_event).unwrap();
    assert_eq!(event["lines"][0]["target"], "KSAW");
    assert!(fixture.consent.reference_event.contains("\"upper\":-0.0"));
}

#[test]
fn every_term_differing_from_the_oracle_or_the_form_is_refused() {
    let terms: &[Change<queued::QueuedTerms>] = &[
        ("competition", |t| t.competition_id = Uuid::now_v7()),
        ("network", |t| t.network = Network::Regtest),
        ("oracle key", |t| {
            t.oracle_pubkey = hex::encode(point(8).serialize_xonly())
        }),
        ("signing date", |t| t.signing_date -= 1),
        ("expiry", |t| t.expiry -= 1),
        ("observation start", |t| {
            t.observation.start_observation_date += 1
        }),
        ("observation end", |t| {
            t.observation.end_observation_date -= 1
        }),
        ("source", |t| t.observation.source = "noaa".into()),
        ("targets order", |t| t.observation.targets.reverse()),
        ("scoring fields", |t| {
            t.observation.scoring_fields.pop();
        }),
        ("values per entry", |t| {
            t.observation.number_of_values_per_entry = 3
        }),
        ("scoring rules", |t| {
            t.observation.scoring_rules = ScoringRules::Fixed
        }),
        ("line", |t| t.observation.lines[2].upper = 1.5),
        ("line zero sign", |t| t.observation.lines[1].upper = 0.0),
        ("line window", |t| t.observation.lines[0].window_hours = 48),
        ("line order", |t| t.observation.lines.reverse()),
        ("missing line", |t| t.observation.lines.truncate(2)),
        ("winners", |t| {
            t.number_of_places_win = 2;
            t.pool_rules = PoolRules::new(3, 25).unwrap();
        }),
        ("pool rules", |t| {
            t.pool_rules = PoolRules::new(3, 25).unwrap()
        }),
        ("stake", |t| t.stake_sats = ENTRY_FEE - 1),
        ("locktime", |t| t.relative_locktime_block_delta = 144),
        ("fee ceiling", |t| {
            t.max_fee_rate = FeeRate::from_sat_per_vb_u32(11)
        }),
        ("market maker", |t| t.market_maker.pubkey = point(4)),
    ];
    for (label, change) in terms {
        assert!(
            Fixture::generated().with_terms(change).check().is_err(),
            "{label}"
        );
    }
}

#[test]
fn another_entry_ticket_or_payout_hash_is_refused() {
    let entries: &[Change<QueuedEntryTerms>] = &[
        ("entry", |e| e.entry_id = Uuid::now_v7()),
        ("ticket hash", |e| e.ticket_hash = [9; 32]),
        ("payout hash", |e| e.payout_hash = [9; 32]),
    ];
    for (label, change) in entries {
        assert!(
            Fixture::generated().with_entry(change).check().is_err(),
            "{label}"
        );
    }
}

#[test]
fn a_reference_event_differing_from_the_terms_is_refused() {
    let events: &[Change<Value>] = &[
        ("event id", |e| e["id"] = json!(Uuid::now_v7())),
        ("signing date", |e| {
            e["signing_date"] = json!(rfc3339(START + 2 * DAY + 1))
        }),
        ("start", |e| {
            e["start_observation_date"] = json!(rfc3339(START - 1))
        }),
        ("end", |e| {
            e["end_observation_date"] = json!(rfc3339(START + DAY + 1))
        }),
        ("locations", |e| e["locations"] = json!(["KORD", "KSAW"])),
        ("scoring fields", |e| {
            e["scoring_fields"] = json!(["temp_high"])
        }),
        ("values per entry", |e| {
            e["number_of_values_per_entry"] = json!(3)
        }),
        ("scoring rules", |e| e["scoring_rules"] = json!("fixed")),
        ("unknown scoring rules", |e| {
            e["scoring_rules"] = json!("tides")
        }),
        ("no scoring rules", |e| {
            e.as_object_mut().unwrap().remove("scoring_rules");
        }),
        ("line bound", |e| e["lines"][0]["lower"] = json!(-1.0)),
        ("line zero sign", |e| e["lines"][1]["upper"] = json!(0.0)),
        ("line window", |e| e["lines"][2]["window_hours"] = json!(48)),
        ("extra line", |e| {
            let mut extra = e["lines"][0].clone();
            extra["metric"] = json!("wind_speed");
            e["lines"].as_array_mut().unwrap().push(extra);
        }),
        ("missing line", |e| {
            e["lines"].as_array_mut().unwrap().pop();
        }),
        ("expiry", |e| {
            e["event_announcement"]["expiry"] = json!(START + 3 * DAY + 1)
        }),
        ("no expiry", |e| {
            e["event_announcement"]["expiry"] = Value::Null
        }),
        ("winners", |e| e["number_of_places_win"] = json!(2)),
        ("source", |e| e["source"] = json!("noaa")),
    ];
    for (label, change) in events {
        assert!(
            Fixture::generated().with_event(change).check().is_err(),
            "{label}"
        );
    }
    let mut not_json = Fixture::generated();
    not_json.consent.reference_event = "{".into();
    assert!(not_json.check().is_err());
}

#[test]
fn a_ticket_or_choice_differing_from_the_form_is_refused() {
    let consents: &[Change<QueuedConsent>] = &[
        ("competition", |c| c.competition_id = Uuid::now_v7()),
        ("address", |c| {
            c.lightning_address = Some("mallory@wallet.example".into())
        }),
        ("no address", |c| c.lightning_address = None),
        ("fallback", |c| c.allow_invoice_fallback = false),
        ("sellback consent", |c| {
            c.release_entry_key_after_payment = false
        }),
        ("ticket price", |c| c.ticket_amount_sats = TICKET + 1),
        ("invoice amount", |c| {
            c.ticket_invoice = invoice(TICKET + 1).to_string()
        }),
        ("entry fee", |c| c.entry_fee_sats = ENTRY_FEE + 1),
        ("pool rules", |c| {
            c.pool_rules = PoolRules::new(2, 24).unwrap()
        }),
        ("locktime", |c| c.expected_relative_locktime_delta = 144),
        ("fee ceiling below the terms", |c| c.max_fee_rate_sat_vb = 9),
        ("no fee ceiling", |c| c.max_fee_rate_sat_vb = 0),
        ("another oracle", |c| {
            c.oracle_pubkey = BASE64.encode(point(8).serialize())
        }),
        ("x-only oracle key", |c| {
            c.oracle_pubkey = BASE64.encode(oracle().serialize_xonly())
        }),
        ("hex oracle key", |c| c.oracle_pubkey = oracle().to_string()),
        ("not an invoice", |c| c.ticket_invoice = "lnbc1".into()),
    ];
    for (label, change) in consents {
        let mut fixture = Fixture::generated();
        change(&mut fixture.consent);
        assert!(fixture.check().is_err(), "{label}");
    }
}

#[test]
fn the_deposit_must_be_scoped_to_the_competition_terms_and_ticket() {
    let assignments: &[Change<RegistrationAssignment>] = &[
        ("session", |a| a.session_id = Uuid::now_v7().to_string()),
        ("session spelling", |a| {
            a.session_id = a.session_id.to_uppercase()
        }),
        ("digest", |a| a.manifest_hash[0] ^= 1),
        ("ticket", |a| a.user_id = Uuid::now_v7()),
        ("no policy", |a| a.payout_policy = None),
    ];
    for (label, change) in assignments {
        let mut fixture = Fixture::generated();
        change(&mut fixture.assignment);
        assert!(fixture.check().is_err(), "{label}");
    }
}

#[test]
fn the_policy_must_hold_a_queued_entry_in_a_consented_escrow() {
    let policies: &[Change<PayoutPolicy>] = &[
        ("address", |p| {
            p.automatic_lightning_address = Some("mallory@wallet.example".into())
        }),
        ("fallback", |p| p.allow_invoice_fallback = false),
        ("sellback consent", |p| {
            p.release_entry_key_after_payment = false
        }),
        ("contract terms", |p| p.contract_terms = "{}".into()),
        ("no escrow", |p| p.ark_escrow = None),
        ("fee beyond the ticket", |p| {
            p.ark_escrow.as_mut().unwrap().max_fee_sats = COORDINATOR_FEE + 1
        }),
        ("escrow for another key", |p| {
            p.ark_escrow = Some(escrow_policy(
                point(6),
                market_maker(),
                (START + 3 * DAY) as u32,
            ))
        }),
    ];
    for (label, change) in policies {
        assert!(
            Fixture::generated().with_policy(change).check().is_err(),
            "{label}"
        );
    }
    // The escrow holds this entry's key, but funds another coordinator or refunds too late.
    let expiry = (START + 3 * DAY) as u32;
    for (label, coordinator, refund_at) in [
        ("another coordinator", point(4), expiry),
        ("refund after the expiry", market_maker(), expiry + 1),
    ] {
        let fixture = Fixture::generated();
        let escrow = escrow_policy(fixture.key.point(), coordinator, refund_at);
        assert!(
            fixture
                .with_policy(|p| p.ark_escrow = Some(escrow))
                .check()
                .is_err(),
            "{label}"
        );
    }
}

/// A refund may cost the swap service more than the coordinator's fee, even a zero one, but never
/// more than the stake it returns.
#[test]
fn a_refund_fee_beyond_the_stake_is_refused() {
    let mut small = Fixture::generated().with_terms(|t| t.stake_sats = 50);
    small.consent.entry_fee_sats = 50;
    small.consent.ticket_amount_sats = 50 + COORDINATOR_FEE;
    small.consent.ticket_invoice = invoice(50 + COORDINATOR_FEE).to_string();
    assert!(small.check().is_err());
    small
        .with_policy(|p| p.ark_escrow.as_mut().unwrap().max_refund_fee_sats = 50)
        .check()
        .unwrap();
    Fixture::generated()
        .with_policy(|p| p.ark_escrow.as_mut().unwrap().max_refund_fee_sats = COORDINATOR_FEE + 1)
        .check()
        .unwrap();
    Fixture::generated()
        .with_policy(|p| p.ark_escrow.as_mut().unwrap().max_fee_sats = 0)
        .check()
        .unwrap();
}

/// A ticket's network fee, fixed when it is issued, is part of its price: the form hands the
/// wallet the price it showed plus that fee, the invoice charges it, and the escrow may pay it out
/// with the coordinator's fee, but never more than the ticket above the stake.
#[test]
fn a_network_fee_is_part_of_the_ticket_price() {
    const NETWORK_FEE: u64 = 50;
    const PRICED: u64 = TICKET + NETWORK_FEE;
    let priced = |max_fee_sats: u64, amount_sats: u64, invoiced_sats: u64| {
        let mut fixture = Fixture::generated()
            .with_policy(|p| p.ark_escrow.as_mut().unwrap().max_fee_sats = max_fee_sats);
        fixture.consent.ticket_amount_sats = amount_sats;
        fixture.consent.ticket_invoice = invoice(invoiced_sats).to_string();
        fixture.check()
    };
    priced(COORDINATOR_FEE + NETWORK_FEE, PRICED, PRICED).unwrap();
    // A network fee the escrow does not claim is the coordinator's loss, not the player's.
    priced(COORDINATOR_FEE, PRICED, PRICED).unwrap();
    for (label, max_fee_sats, amount_sats, invoiced_sats) in [
        ("escrow fee beyond the ticket", COORDINATOR_FEE + NETWORK_FEE + 1, PRICED, PRICED),
        ("the fee without the price", COORDINATOR_FEE + NETWORK_FEE, TICKET, TICKET),
        ("an invoice without the fee", COORDINATOR_FEE + NETWORK_FEE, PRICED, TICKET),
        ("an invoice above the price", COORDINATOR_FEE + NETWORK_FEE, PRICED, PRICED + 1),
    ] {
        assert!(
            priced(max_fee_sats, amount_sats, invoiced_sats).is_err(),
            "{label}"
        );
    }
}

#[test]
fn a_concrete_contract_policy_is_not_a_queued_entry() {
    let fixture = Fixture::generated();
    let terms = &fixture.entry.terms;
    let contract = ContractAuthorization {
        competition_id: terms.competition_id,
        entry_id: fixture.entry_id,
        network: terms.network,
        player_index: 0,
        player_count: 2,
        ticket_hash: fixture.entry.ticket_hash,
        payout_hash: fixture.entry.payout_hash,
        market_maker: terms.market_maker.clone(),
        event: EventLockingConditions {
            locking_points: vec![point(9).into(), point(10).into()],
            expiry: Some(terms.expiry),
        },
        outcome_payouts: queued::pool_payouts(2, 1).unwrap(),
        funding_value: Amount::from_sat(2 * ENTRY_FEE),
        relative_locktime_block_delta: terms.relative_locktime_block_delta,
        max_fee_rate: terms.max_fee_rate,
    };
    let mut concrete = fixture.policy.clone();
    concrete.queued_entry = None;
    concrete.contract_terms = serde_json::to_string(&contract).unwrap();
    let mut fixture = fixture;
    fixture.assignment.payout_policy = Some(serde_json::to_string(&concrete).unwrap());
    assert!(fixture.check().is_err());
}

#[test]
fn the_oracle_key_is_read_from_its_compressed_base64_form() {
    let key = oracle();
    assert_eq!(
        oracle_xonly_key(&BASE64.encode(key.serialize())),
        Some(key.serialize_xonly())
    );
    for refused in [
        BASE64.encode(key.serialize_xonly()),
        BASE64.encode(key.serialize_uncompressed()),
        BASE64.encode([4; 33]),
        key.to_string(),
        String::new(),
    ] {
        assert_eq!(oracle_xonly_key(&refused), None, "{refused}");
    }
}

/// A refused entry fails before the wallet looks up any enclave, so nothing leaves the browser.
#[test]
fn a_refused_entry_fails_before_any_network_request() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let wallet = super::super::DlcWalletCore::create(&NostrClientCore::default(), Network::Signet);
    let fixture = Fixture::generated();
    let mut future = std::pin::pin!(wallet.keymeld_queued_registration(
        fixture.entry_id,
        &fixture.assignment,
        &fixture.consent,
    ));
    // This wallet's seed derives another key for the entry, so its payout hash differs.
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(WalletError::Keymeld(_)))
    ));
}
