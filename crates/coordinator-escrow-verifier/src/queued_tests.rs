//! Queued competitions: entries enroll under their competition's deposit scope, a pool binds the
//! contract its formation and the oracle's statement give, and a deposit refunds unbound.
use super::ark_escrow::{
    digests, escrow_with, funding_output, intent_proof, p2tr, policy_for, prepare as prepare_spend,
    refund_of, refund_swap, MAX_FEE_SATS,
};
use super::*;
use coordinator_escrow::{
    oracle_statement::{
        LineTerms, ObservationTerms, Outcomes, RankingOutcomes, ScoringRules, Statement, Terms,
    },
    pools::{self, PoolRules},
    queued::QueuedTerms,
};
use dlctix::bitcoin::{BlockHash, TxOut};
use dlctix::musig2::secp256k1::{Keypair, Secp256k1, SecretKey};
use keymeld_core::authorization::DepositScope;

const ORACLE_SECRET: [u8; 32] = [7; 32];
const START: i64 = 1_790_000_000;
/// The pool's entry keys, by slot. The escrow fixtures refund to entry key 14.
const KEYS: [u8; 3] = [14, 15, 16];
const MAKER: u8 = 18;

fn oracle_keypair(secret: [u8; 32]) -> Keypair {
    Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_byte_array(secret).unwrap(),
    )
}

fn sign_statement(statement: Statement, secret: [u8; 32]) -> SignedStatement {
    let signature = Secp256k1::new()
        .sign_schnorr_no_aux_rand(&statement.digest().unwrap(), &oracle_keypair(secret));
    SignedStatement {
        statement,
        signature: signature.to_string(),
    }
}

fn observation() -> ObservationTerms {
    ObservationTerms {
        source: "noaa_weather".into(),
        start_observation_date: START,
        end_observation_date: START + 86_400,
        targets: vec!["KORD".into(), "KSAW".into()],
        scoring_fields: vec!["temp_high".into()],
        number_of_values_per_entry: 2,
        scoring_rules: ScoringRules::Lines,
        lines: vec![
            LineTerms {
                target: "KORD".into(),
                metric: "temp_high".into(),
                lower: -2.5,
                upper: -0.5,
                window_hours: 24,
            },
            LineTerms {
                target: "KSAW".into(),
                metric: "temp_high".into(),
                lower: -1.25,
                upper: 1.75,
                window_hours: 24,
            },
        ],
    }
}

fn queued_terms() -> QueuedTerms {
    QueuedTerms {
        competition_id: Uuid::now_v7(),
        network: Network::Regtest,
        market_maker: MarketMaker {
            pubkey: Scalar::from_slice(&[MAKER; 32]).unwrap().base_point_mul(),
        },
        oracle_pubkey: oracle_keypair(ORACLE_SECRET)
            .x_only_public_key()
            .0
            .to_string(),
        signing_date: START + 2 * 86_400,
        expiry: (START + 3 * 86_400) as u32,
        observation: observation(),
        number_of_places_win: 1,
        // Five tickets form a pool of three and a pool of two.
        pool_rules: PoolRules::new(2, 3).unwrap(),
        stake_sats: 20_000,
        relative_locktime_block_delta: 72,
        max_fee_rate: FeeRate::from_sat_per_vb_u32(2),
    }
}

fn preimage(slot: usize) -> [u8; 32] {
    [60 + slot as u8; 32]
}

fn entry(terms: &QueuedTerms, entry_id: Uuid, slot: usize) -> QueuedEntryTerms {
    QueuedEntryTerms {
        terms: terms.clone(),
        entry_id,
        ticket_hash: payout::sha256(&[40 + slot as u8; 32]),
        payout_hash: payout::sha256(&preimage(slot)),
    }
}

/// The escrow context a deposit is sealed under: its competition's scope, as its own entry.
fn deposit_context(entry: &QueuedEntryTerms) -> EscrowContext {
    let (keygen_session_id, manifest_digest) = queued::deposit_scope(&entry.terms).unwrap();
    EscrowContext {
        keygen_session_id,
        user_id: UserId::from(entry.entry_id),
        escrow_id: entry.entry_id,
        manifest_digest,
        application: ApplicationContext::commit("placeholder".into(), 1, &[]).unwrap(),
    }
}

fn register(
    context: EscrowContext,
    key: u8,
    preimage: [u8; 32],
    policy: PayoutPolicy,
) -> SignedEscrowPolicy {
    registration(
        context,
        &[key; 32],
        policy,
        &preimage,
        Recipient {
            encryption_public_key: PublicKeyBytes::new(&public(MAKER)).unwrap(),
        },
    )
    .unwrap()
    .policy
    .clone()
}

fn queued_policy(entry: &QueuedEntryTerms, key: u8, automatic: bool) -> PayoutPolicy {
    PayoutPolicy {
        queued_entry: Some(entry.to_json().unwrap()),
        automatic_lightning_address: automatic.then(|| "alice+prize@wallet.example".into()),
        allow_invoice_fallback: true,
        release_entry_key_after_payment: true,
        contract_terms: String::new(),
        ark_escrow: Some(policy_for(&escrow_with(key, MAKER))),
    }
}

fn deposit(entry: &QueuedEntryTerms, key: u8, slot: usize, automatic: bool) -> SignedEscrowPolicy {
    register(
        deposit_context(entry),
        key,
        preimage(slot),
        queued_policy(entry, key, automatic),
    )
}

fn deposit_scope(terms: &QueuedTerms, evidence: &DepositEvidence) -> DepositScope {
    let (deposit_session_id, digest) = queued::deposit_scope(terms).unwrap();
    DepositScope {
        deposit_session_id,
        deposit_digest: digest.to_vec(),
        evidence: evidence.encode().unwrap(),
    }
}

fn manifest(
    session: SessionId,
    maker: &UserId,
    players: &[Uuid],
    scope: Option<DepositScope>,
) -> SignedSessionManifest {
    let mut verifiers: BTreeMap<UserId, Vec<u8>> = players
        .iter()
        .enumerate()
        .map(|(index, id)| (UserId::from(*id), public(30 + index as u8)))
        .collect();
    verifiers.insert(maker.clone(), public(17));
    SignedSessionManifest::sign(
        SessionAuthorizationManifest {
            keygen_session_id: session,
            coordinator_user_id: maker.clone(),
            creator_pubkey: public(11),
            signing_pubkey: public(12),
            session_public_key: public(10),
            participant_verifiers: verifiers,
            timeout_secs: 3600,
            max_signing_sessions: None,
            encrypted_taproot_tweak: "encrypted".into(),
            subset_definitions: vec![],
            deposit_scope: scope,
        },
        &[11; 32],
    )
    .unwrap()
}

fn statement_for(terms: &QueuedTerms, members: &[Uuid]) -> Statement {
    Statement {
        event_id: Uuid::now_v7(),
        signing_date: terms.signing_date,
        expiry: terms.expiry,
        nonce_point: Scalar::from_slice(&[9; 32]).unwrap().base_point_mul(),
        outcomes: Outcomes::Ranking(RankingOutcomes {
            number_of_places_win: 1,
            entry_ids: members.to_vec(),
        }),
        terms: Terms::Observation(terms.observation.clone()),
    }
}

/// Re-sign a statement after changing it, as a dishonest oracle event would be signed.
fn resign(signed: &SignedStatement, change: impl FnOnce(&mut Statement)) -> SignedStatement {
    let mut statement = signed.statement.clone();
    change(&mut statement);
    sign_statement(statement, ORACLE_SECRET)
}

/// A kickoff of five tickets: a pool of three, whose session these tests act in, and a pool of
/// two.
struct Pool {
    f: Fixture,
    terms: QueuedTerms,
    tickets: Vec<Uuid>,
    block_hash: BlockHash,
    pool_index: usize,
    /// Slot `i` is `members[i]`, with entry key `KEYS[i]`.
    members: Vec<Uuid>,
    /// The other pool's tickets.
    others: Vec<Uuid>,
    statement: SignedStatement,
    maker: UserId,
}

fn pool_fixture(automatic: bool) -> Pool {
    let terms = queued_terms();
    let tickets: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
    let block_hash = BlockHash::from_byte_array([5; 32]);
    let pools::Formation::Pools { pools, .. } = pools::form(
        &terms.pool_rules,
        terms.competition_id,
        &tickets,
        &block_hash,
    )
    .unwrap() else {
        panic!("five tickets form pools");
    };
    let pool_index = pools.iter().position(|pool| pool.len() == 3).unwrap();
    let mut members = pools[pool_index].clone();
    members.sort_unstable();
    let others = tickets
        .iter()
        .filter(|ticket| !members.contains(ticket))
        .copied()
        .collect();
    let statement = sign_statement(statement_for(&terms, &members), ORACLE_SECRET);
    let maker = UserId::new_v7();
    let evidence = DepositEvidence::Pool {
        competition_id: terms.competition_id,
        tickets: tickets.clone(),
        block_hash,
        pool_index,
    };
    let manifest = manifest(
        SessionId::from(statement.statement.event_id),
        &maker,
        &members,
        Some(deposit_scope(&terms, &evidence)),
    );
    let policies: BTreeMap<_, _> = members
        .iter()
        .enumerate()
        .map(|(slot, id)| {
            (
                UserId::from(*id),
                deposit(&entry(&terms, *id, slot), KEYS[slot], slot, automatic),
            )
        })
        .collect();
    let mut keys: BTreeMap<_, _> = members
        .iter()
        .enumerate()
        .map(|(slot, id)| {
            (
                UserId::from(*id),
                PublicKeyBytes::new(&public(KEYS[slot])).unwrap(),
            )
        })
        .collect();
    keys.insert(maker.clone(), PublicKeyBytes::new(&public(MAKER)).unwrap());
    let contract = contract_for(&terms, &members, &statement);
    Pool {
        f: Fixture {
            manifest,
            policy: policies[&UserId::from(members[0])].clone(),
            policies,
            keys,
            contract,
        },
        terms,
        tickets,
        block_hash,
        pool_index,
        members,
        others,
        statement,
        maker,
    }
}

/// The pool's contract, built from every member's derived terms.
fn contract_for(
    terms: &QueuedTerms,
    members: &[Uuid],
    statement: &SignedStatement,
) -> ContractCommitment {
    let derived: Vec<_> = members
        .iter()
        .enumerate()
        .map(|(slot, id)| {
            queued::pool_authorization(&entry(terms, *id, slot), members, statement).unwrap()
        })
        .collect();
    let first = &derived[0];
    ContractCommitment {
        contract_parameters: ContractParameters {
            market_maker: first.market_maker.clone(),
            players: derived
                .iter()
                .enumerate()
                .map(|(slot, terms)| Player {
                    pubkey: Scalar::from_slice(&[KEYS[slot]; 32])
                        .unwrap()
                        .base_point_mul(),
                    ticket_hash: terms.ticket_hash,
                    payout_hash: terms.payout_hash,
                })
                .collect(),
            event: first.event.clone(),
            outcome_payouts: first.outcome_payouts.clone(),
            fee_rate: FeeRate::from_sat_per_vb_u32(1),
            funding_value: first.funding_value,
            relative_locktime_block_delta: first.relative_locktime_block_delta,
        },
        funding_outpoint: OutPoint::null(),
    }
}

/// What one bind call is given, for a test to change.
struct BindInputs {
    manifest: SignedSessionManifest,
    policies: BTreeMap<UserId, SignedEscrowPolicy>,
    keys: BTreeMap<UserId, PublicKeyBytes>,
    contract: ContractCommitment,
    statement: Option<SignedStatement>,
}

impl Pool {
    fn evidence(&self) -> DepositEvidence {
        DepositEvidence::Pool {
            competition_id: self.terms.competition_id,
            tickets: self.tickets.clone(),
            block_hash: self.block_hash,
            pool_index: self.pool_index,
        }
    }
    fn refund_evidence(&self) -> DepositEvidence {
        DepositEvidence::Refund {
            competition_id: self.terms.competition_id,
        }
    }
    /// This pool's session, with other players or another deposit scope.
    fn manifest_with(
        &self,
        players: &[Uuid],
        scope: Option<DepositScope>,
    ) -> SignedSessionManifest {
        manifest(
            self.f.manifest.manifest.keygen_session_id.clone(),
            &self.maker,
            players,
            scope,
        )
    }
    fn policy(&self, slot: usize) -> &SignedEscrowPolicy {
        &self.f.policies[&UserId::from(self.members[slot])]
    }
    fn entry(&self, slot: usize) -> QueuedEntryTerms {
        entry(&self.terms, self.members[slot], slot)
    }
    fn enroll(
        &self,
        manifest: &SignedSessionManifest,
        policy: &SignedEscrowPolicy,
    ) -> Result<(), VerificationError> {
        CoordinatorVerifier::default().validate_registration(RegistrationView {
            manifest,
            policy,
            restoring: false,
        })
    }
    /// Bind as slot 0, after `change`.
    fn bind(
        &self,
        change: impl FnOnce(&Pool, &mut BindInputs),
    ) -> Result<Payload, VerificationError> {
        let mut inputs = BindInputs {
            manifest: self.f.manifest.clone(),
            policies: self.f.policies.clone(),
            keys: self.f.keys.clone(),
            contract: self.f.contract.clone(),
            statement: Some(self.statement.clone()),
        };
        change(self, &mut inputs);
        CoordinatorVerifier::default().bind(
            BindView {
                manifest: &inputs.manifest,
                policy: &self.f.policy,
                participant_policies: &inputs.policies,
                participant_public_keys: &inputs.keys,
            },
            &Payload::encode(&ContractBinding {
                contract: inputs.contract,
                statement: inputs.statement,
            })
            .unwrap(),
        )
    }
}

/// The result is refused, for the reason given.
#[track_caller]
fn refused<T: std::fmt::Debug>(result: Result<T, VerificationError>, reason: &str) {
    let error = format!("{:?}", result.expect_err(reason));
    assert!(error.contains(reason), "expected {reason:?}, got {error}");
}

#[test]
fn a_pool_member_enrolls_under_its_competitions_deposit_scope() {
    let pool = pool_fixture(false);
    for slot in 0..3 {
        pool.enroll(&pool.f.manifest, pool.policy(slot)).unwrap();
    }
    // Deposits registered only to be refunded enroll in any roster, such as a ticket alone.
    let refund = manifest(
        SessionId::new_v7(),
        &pool.maker,
        &pool.members[..1],
        Some(deposit_scope(&pool.terms, &pool.refund_evidence())),
    );
    pool.enroll(&refund, pool.policy(0)).unwrap();
}

#[test]
fn enrollment_is_refused_outside_the_pool_its_formation_gives() {
    let pool = pool_fixture(false);
    let scope = deposit_scope(&pool.terms, &pool.evidence());
    let players_differ = "Session players differ from the pool its formation gives";

    // A ticket the kickoff put in the other pool, added to this session.
    let outsider = pool.others[0];
    let with_outsider: Vec<Uuid> = pool.members.iter().copied().chain([outsider]).collect();
    let manifest = pool.manifest_with(&with_outsider, Some(scope.clone()));
    let outsider_policy = deposit(&entry(&pool.terms, outsider, 0), 20, 0, false);
    refused(pool.enroll(&manifest, &outsider_policy), players_differ);
    refused(pool.enroll(&manifest, pool.policy(0)), players_differ);
    // Its own session, but the evidence names the pool it is not in.
    let other_pool = DepositEvidence::Pool {
        competition_id: pool.terms.competition_id,
        tickets: pool.tickets.clone(),
        block_hash: pool.block_hash,
        pool_index: 1 - pool.pool_index,
    };
    let manifest = pool.manifest_with(&pool.members, Some(deposit_scope(&pool.terms, &other_pool)));
    refused(pool.enroll(&manifest, pool.policy(0)), players_differ);

    // A session missing one of the pool's members.
    let manifest = pool.manifest_with(&pool.members[..2], Some(scope.clone()));
    refused(pool.enroll(&manifest, pool.policy(0)), players_differ);

    // Evidence of another competition's kickoff.
    let elsewhere = DepositEvidence::Pool {
        competition_id: Uuid::now_v7(),
        tickets: pool.tickets.clone(),
        block_hash: pool.block_hash,
        pool_index: pool.pool_index,
    };
    let manifest = pool.manifest_with(
        &pool.members,
        Some(DepositScope {
            evidence: elsewhere.encode().unwrap(),
            ..scope.clone()
        }),
    );
    refused(
        pool.enroll(&manifest, pool.policy(0)),
        "Deposit evidence belongs to another competition",
    );

    // A deposit scope, and a policy sealed under it, that are not the competition's terms.
    let scope_differs = "Deposit scope differs from the queued competition and its terms";
    let mut other_terms = pool.terms.clone();
    other_terms.stake_sats += 1;
    let wrong_digest = other_terms.digest().unwrap();
    let wrong_session = SessionId::new_v7();
    for (session, digest) in [
        (scope.deposit_session_id.clone(), wrong_digest),
        (wrong_session, pool.terms.digest().unwrap()),
    ] {
        let manifest = pool.manifest_with(
            &pool.members,
            Some(DepositScope {
                deposit_session_id: session.clone(),
                deposit_digest: digest.to_vec(),
                evidence: scope.evidence.clone(),
            }),
        );
        let entry = pool.entry(0);
        let context = EscrowContext {
            keygen_session_id: session,
            manifest_digest: digest,
            ..deposit_context(&entry)
        };
        let policy = register(
            context,
            KEYS[0],
            preimage(0),
            queued_policy(&entry, KEYS[0], false),
        );
        refused(pool.enroll(&manifest, &policy), scope_differs);
    }

    // A session without a deposit scope.
    let manifest = pool.manifest_with(&pool.members, None);
    refused(
        pool.enroll(&manifest, pool.policy(0)),
        "invalid participant or manifest scope",
    );

    // A queued entry registered as another member.
    let entry = pool.entry(0);
    let context = EscrowContext {
        user_id: UserId::from(pool.members[1]),
        ..deposit_context(&entry)
    };
    let policy = register(
        context,
        KEYS[0],
        preimage(0),
        queued_policy(&entry, KEYS[0], false),
    );
    refused(
        pool.enroll(&pool.f.manifest, &policy),
        "A queued entry registers as its own entry",
    );

    // A single competition's entry sealed under the deposit scope.
    let terms = queued::pool_authorization(&entry, &pool.members, &pool.statement).unwrap();
    let single = PayoutPolicy {
        queued_entry: None,
        contract_terms: serde_json::to_string(&terms).unwrap(),
        ..queued_policy(&entry, KEYS[0], false)
    };
    let policy = register(deposit_context(&entry), KEYS[0], preimage(0), single);
    refused(
        pool.enroll(&pool.f.manifest, &policy),
        "A single competition's entry cannot join a session of key deposits",
    );
}

#[tokio::test]
async fn a_pool_binds_the_contract_its_formation_and_statement_give() {
    let pool = pool_fixture(false);
    let bound = pool.bind(|_, _| {}).unwrap();
    // Every member binds the same contract.
    for slot in 1..3 {
        CoordinatorVerifier::default()
            .bind(
                BindView {
                    manifest: &pool.f.manifest,
                    policy: pool.policy(slot),
                    participant_policies: &pool.f.policies,
                    participant_public_keys: &pool.f.keys,
                },
                &Payload::encode(&ContractBinding {
                    contract: pool.f.contract.clone(),
                    statement: Some(pool.statement.clone()),
                })
                .unwrap(),
            )
            .unwrap();
    }

    // Restoring derives the same terms again, from the kept statement.
    let (restored, _, terms) = restore_binding(&pool.f.manifest, &pool.f.policy, &bound).unwrap();
    assert_eq!(restored.statement.as_ref(), Some(&pool.statement));
    assert_eq!(
        terms,
        queued::pool_authorization(&pool.entry(0), &pool.members, &pool.statement).unwrap()
    );
    assert_eq!(terms.competition_id, pool.statement.statement.event_id);
    assert_eq!(terms.player_index, 0);
    // The kept statement is what the terms come from: without it, or with another, nothing
    // restores.
    let mut state: serde_json::Value = serde_json::from_slice(bound.as_bytes()).unwrap();
    state.as_object_mut().unwrap().remove("statement");
    let stripped = Payload::new(serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(restore_binding(&pool.f.manifest, &pool.f.policy, &stripped).is_err());
    let mut state: BoundContract = bound.decode().unwrap();
    state.statement = Some(resign(&pool.statement, |s| s.expiry += 1));
    let changed = Payload::encode(&state).unwrap();
    assert!(restore_binding(&pool.f.manifest, &pool.f.policy, &changed).is_err());

    // A bound action acts on the derived terms: the escrow is spent only into this contract.
    let verifier = CoordinatorVerifier::default();
    let escrow = escrow_with(KEYS[0], MAKER);
    let fee = TxOut {
        value: Amount::from_sat(2 * MAX_FEE_SATS),
        script_pubkey: p2tr(40),
    };
    let spend = ArkEscrowSpend::IntentProof {
        proof_psbt: coordinator_escrow::ark::psbt_hex(&intent_proof(
            &escrow,
            vec![funding_output(&pool.f), fee],
        )),
    };
    let (prepared, attempt) = prepare_spend(&verifier, &pool.f, &bound, spend)
        .await
        .unwrap();
    verifier
        .verify_execution(
            pool.f.execute_view(&bound, &attempt, SIGN_ARK_ESCROW),
            &prepared,
            &Payload::default(),
        )
        .await
        .unwrap();
}

#[test]
fn a_pool_binding_is_refused_unless_every_term_matches() {
    let pool = pool_fixture(false);
    pool.bind(|_, _| {}).unwrap();

    refused(
        pool.bind(|_, inputs| inputs.statement = None),
        "binds the oracle's statement of its event",
    );
    // Signed by another key.
    refused(
        pool.bind(|pool, inputs| {
            inputs.statement = Some(sign_statement(pool.statement.statement.clone(), [8; 32]))
        }),
        "not signed by this oracle key",
    );
    // Signed by the oracle, but not the lines, entries or order the players entered under.
    let statement_differs = "the pool's oracle statement differs from the terms";
    let changes: [fn(&Pool, &mut Statement); 4] = [
        |_, s| match &mut s.terms {
            Terms::Observation(terms) => terms.lines[1].upper = 2.0,
        },
        |_, s| match &mut s.outcomes {
            Outcomes::Ranking(ranking) => ranking.entry_ids.swap(0, 1),
        },
        |pool, s| match &mut s.outcomes {
            Outcomes::Ranking(ranking) => ranking.entry_ids[2] = pool.others[0],
        },
        |pool, s| match &mut s.outcomes {
            Outcomes::Ranking(ranking) => ranking.entry_ids.push(pool.others[0]),
        },
    ];
    for change in changes {
        refused(
            pool.bind(|pool, inputs| {
                inputs.statement = Some(resign(&pool.statement, |s| change(pool, s)))
            }),
            statement_differs,
        );
    }
    // Deposits registered to be refunded cannot bind.
    refused(
        pool.bind(|pool, inputs| {
            inputs.manifest = pool.manifest_with(
                &pool.members,
                Some(deposit_scope(&pool.terms, &pool.refund_evidence())),
            )
        }),
        "Deposits registered to be refunded cannot bind a contract",
    );
    // The same pool, in a session other than its oracle event's.
    refused(
        pool.bind(|pool, inputs| {
            inputs.manifest = manifest(
                SessionId::new_v7(),
                &pool.maker,
                &pool.members,
                Some(deposit_scope(&pool.terms, &pool.evidence())),
            )
        }),
        "The oracle statement is of another pool's event than this session",
    );
    // The contract's economics or slots differ from the derived terms.
    let economics = "Contract differs from authorized economics";
    refused(
        pool.bind(|_, inputs| {
            let value = &mut inputs.contract.contract_parameters.funding_value;
            *value = Amount::from_sat(value.to_sat() + 1);
        }),
        economics,
    );
    refused(
        pool.bind(|_, inputs| {
            let value = &mut inputs.contract.contract_parameters.funding_value;
            *value = Amount::from_sat(value.to_sat() - 1);
        }),
        economics,
    );
    refused(
        pool.bind(|_, inputs| {
            inputs
                .contract
                .contract_parameters
                .outcome_payouts
                .insert(Outcome::Attestation(0), BTreeMap::from([(1, 100)]));
        }),
        economics,
    );
    refused(
        pool.bind(|_, inputs| inputs.contract.contract_parameters.players.swap(0, 1)),
        "Contract slots differ from the authenticated participant roster",
    );
    // A player whose deposit consented to other terms.
    refused(
        pool.bind(|pool, inputs| {
            let mut terms = pool.terms.clone();
            terms.stake_sats += 1;
            let other = entry(&terms, pool.members[2], 2);
            let context = EscrowContext {
                keygen_session_id: pool.f.policy.policy.context.keygen_session_id.clone(),
                manifest_digest: pool.f.policy.policy.context.manifest_digest,
                ..deposit_context(&other)
            };
            inputs.policies.insert(
                UserId::from(pool.members[2]),
                register(
                    context,
                    KEYS[2],
                    preimage(2),
                    queued_policy(&other, KEYS[2], false),
                ),
            );
        }),
        "Deposit scope differs from the queued competition and its terms",
    );
}

#[test]
fn a_pool_roster_follows_its_entry_order() {
    // Slots follow entry ids, and user ids order as their UUIDs do, so the roster and the
    // oracle's outcomes agree. Checked rather than assumed: the verifier refuses otherwise.
    let pool = pool_fixture(false);
    let ids: Vec<Uuid> = pool.f.keys.keys().map(UserId::uuid).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted);
    let as_strings: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let mut sorted_strings = as_strings.clone();
    sorted_strings.sort_unstable();
    assert_eq!(as_strings, sorted_strings);
}

#[test]
fn a_single_competition_binds_no_statement() {
    let verifier = CoordinatorVerifier::default();
    let f = fixture(false);
    let pool = pool_fixture(false);
    let bind = |statement: Option<SignedStatement>| {
        verifier.bind(
            BindView {
                manifest: &f.manifest,
                policy: &f.policy,
                participant_policies: &f.policies,
                participant_public_keys: &f.keys,
            },
            &Payload::encode(&ContractBinding {
                contract: f.contract.clone(),
                statement,
            })
            .unwrap(),
        )
    };
    refused(
        bind(Some(pool.statement.clone())),
        "Only a pool of a queued competition binds an oracle statement",
    );
    // Its bound state reads and writes exactly as before statements existed.
    let bound = bind(None).unwrap();
    let state: serde_json::Value = serde_json::from_slice(bound.as_bytes()).unwrap();
    assert!(state.get("statement").is_none());
    restore_binding(&f.manifest, &f.policy, &bound).unwrap();
}

#[test]
fn a_queued_deposit_refunds_under_refund_or_pool_evidence() {
    let pool = pool_fixture(false);
    let refund_manifest = manifest(
        SessionId::new_v7(),
        &pool.maker,
        &pool.members[..1],
        Some(deposit_scope(&pool.terms, &pool.refund_evidence())),
    );
    let escrow = escrow_with(KEYS[0], MAKER);
    for manifest in [&refund_manifest, &pool.f.manifest] {
        let (policy, consent) = validate_static(manifest, &pool.f.policy).unwrap();
        assert!(matches!(consent, EntryConsent::Queued(_)));
        let (action, _, _) = ark_refund_action(
            &pool.f.policy,
            &policy,
            &consent,
            &refund_of(&escrow, &refund_swap([9u8; 32])),
        )
        .unwrap();
        assert_eq!(digests(&action).len(), 1);
    }
}

#[cfg(feature = "lnurl")]
#[tokio::test]
async fn a_queued_refund_pays_the_players_own_address() {
    use super::ark_escrow::{attempt, refund_parameters, MAX_REFUND_FEE_SATS, REFUNDED_SATS};
    use crate::lnurl_transport::fixtures::{discovery_tls_fixture, FIXTURE_METADATA};

    let pool = pool_fixture(true);
    let escrow = escrow_with(KEYS[0], MAKER);
    let preimage = [9u8; 32];
    let invoice = address_invoice_for(
        REFUNDED_SATS - MAX_REFUND_FEE_SATS,
        preimage,
        FIXTURE_METADATA,
    );
    let refund_manifest = manifest(
        SessionId::new_v7(),
        &pool.maker,
        &pool.members[..1],
        Some(deposit_scope(&pool.terms, &pool.refund_evidence())),
    );
    for manifest in [refund_manifest, pool.f.manifest.clone()] {
        let f = Fixture {
            manifest,
            policy: pool.f.policy.clone(),
            policies: pool.f.policies.clone(),
            keys: pool.f.keys.clone(),
            contract: pool.f.contract.clone(),
        };
        let (client, server) = discovery_tls_fixture().await;
        let verifier = CoordinatorVerifier::with_lnurl(client);
        let unbound = Payload::default();
        let first = attempt();
        let prepared = verifier
            .prepare(
                f.prepare_view(&unbound, &first, SIGN_ARK_REFUND, &BTreeMap::new()),
                &refund_parameters(
                    refund_of(&escrow, &refund_swap(preimage)),
                    invoice.clone(),
                    MAX_REFUND_FEE_SATS,
                ),
            )
            .await
            .unwrap();
        server.await.unwrap();
        verifier
            .verify_execution(
                f.execute_view(&unbound, &first, SIGN_ARK_REFUND),
                &prepared,
                &Payload::default(),
            )
            .await
            .unwrap();
        verifier
            .restore_execution(f.execute_view(&unbound, &first, SIGN_ARK_REFUND), &prepared)
            .await
            .unwrap();
    }
}
