//! arkd's own verdicts on the entry escrows (`tests/fixtures/arkd-rules.json`).
//!
//! `arkd-rules/escrow_rules_test.go` ran arkd's `TapscriptsVtxoScript.Validate` on every escrow in `escrow.json`, under several rule sets.
//! `ServerRules::check` must accept and reject exactly the same cases.

mod common;

use coordinator_ark_escrow::{ServerRules, VtxoScript};
use serde::Deserialize;

use common::{fixture, xonly, JsonTimelock};

#[derive(Deserialize)]
struct Verdicts {
    generator: String,
    verdicts: Vec<Verdict>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Verdict {
    description: String,
    tweaked_public_key: String,
    checks: Vec<Check>,
}

#[derive(Deserialize)]
struct Check {
    rules: Rules,
    accepted: bool,
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rules {
    signer: String,
    min_exit_delay: JsonTimelock,
    block_timelocks_allowed: bool,
}

#[derive(Deserialize)]
struct Escrows {
    vectors: Vec<Escrow>,
}

#[derive(Deserialize)]
struct Escrow {
    description: String,
    expected: EscrowExpected,
}

#[derive(Deserialize)]
struct EscrowExpected {
    leaves: Vec<String>,
}

#[test]
fn server_rules_agree_with_arkd() {
    let verdicts: Verdicts = fixture("arkd-rules.json");
    let escrows: Escrows = fixture("escrow.json");
    assert!(verdicts.generator.starts_with("arkd "));
    assert_eq!(verdicts.verdicts.len(), escrows.vectors.len());

    let mut accepted_on_mutinynet = 0;
    for (verdict, escrow) in verdicts.verdicts.iter().zip(&escrows.vectors) {
        let name = &verdict.description;
        assert_eq!(name, &escrow.description);
        let vtxo = VtxoScript::new(
            escrow
                .expected
                .leaves
                .iter()
                .map(|leaf| hex::decode(leaf).unwrap().into())
                .collect(),
        )
        .unwrap();
        assert_eq!(
            hex::encode(vtxo.tweaked_key().serialize()),
            verdict.tweaked_public_key,
            "{name}: arkd built a different tree"
        );

        for check in &verdict.checks {
            let rules = ServerRules {
                signer: xonly(&check.rules.signer),
                min_exit_delay: check.rules.min_exit_delay.to_timelock(),
                block_timelocks_allowed: check.rules.block_timelocks_allowed,
            };
            let ours = rules.check(&vtxo);
            assert_eq!(
                ours.is_ok(),
                check.accepted,
                "{name}: arkd said {:?} under {rules:?}, this crate said {ours:?}",
                check.error
            );
            if check.accepted && !rules.block_timelocks_allowed {
                accepted_on_mutinynet += 1;
            }
        }
    }
    // At least one escrow must pass a seconds-only server such as Mutinynet.
    assert!(accepted_on_mutinynet > 0);
}
