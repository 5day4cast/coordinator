//! `@arkade-os/sdk`'s VHTLC fixtures (`test/fixtures/vhtlc.json`), rebuilt with this crate's closures.
//!
//! A VHTLC uses every closure the entry escrow does, plus a hash condition, in a six-leaf tree.
//! Six is not a power of two, so the btcd assembly matters here.

mod common;

use bitcoin::absolute::LockTime;
use bitcoin::opcodes::all::{OP_EQUAL, OP_HASH160};
use bitcoin::script::Builder;
use bitcoin::ScriptBuf;
use coordinator_ark_escrow::{ArkAddress, Tapscript, VtxoScript, TESTNET_HRP};
use serde::Deserialize;

use common::{fixture, xonly, JsonTimelock};

#[derive(Deserialize)]
struct Fixtures {
    valid: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    description: String,
    preimage_hash: String,
    receiver: String,
    sender: String,
    server: String,
    refund_locktime: u32,
    unilateral_claim_delay: JsonTimelock,
    unilateral_refund_delay: JsonTimelock,
    unilateral_refund_without_receiver_delay: JsonTimelock,
    expected: String,
    scripts: Scripts,
    taproot: Taproot,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scripts {
    claim_script: String,
    refund_script: String,
    refund_without_receiver_script: String,
    unilateral_claim_script: String,
    unilateral_refund_script: String,
    unilateral_refund_without_receiver_script: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Taproot {
    tweaked_public_key: String,
    tap_tree: String,
    internal_key: String,
}

#[test]
fn vhtlc_fixtures() {
    let fixtures: Fixtures = fixture("vhtlc.json");
    assert_eq!(fixtures.valid.len(), 3);
    for case in fixtures.valid {
        let name = &case.description;
        let (sender, receiver, server) = (
            xonly(&case.sender),
            xonly(&case.receiver),
            xonly(&case.server),
        );
        let hash: [u8; 20] = hex::decode(&case.preimage_hash)
            .unwrap()
            .try_into()
            .unwrap();
        let condition = Builder::new()
            .push_opcode(OP_HASH160)
            .push_slice(hash)
            .push_opcode(OP_EQUAL)
            .into_script();

        let closures = [
            Tapscript::ConditionMultisig {
                condition: condition.clone(),
                pubkeys: vec![receiver, server],
            },
            Tapscript::Multisig {
                pubkeys: vec![sender, receiver, server],
            },
            Tapscript::CltvMultisig {
                locktime: LockTime::from_consensus(case.refund_locktime),
                pubkeys: vec![sender, server],
            },
            Tapscript::ConditionCsvMultisig {
                condition,
                timelock: case.unilateral_claim_delay.to_timelock(),
                pubkeys: vec![receiver],
            },
            Tapscript::CsvMultisig {
                timelock: case.unilateral_refund_delay.to_timelock(),
                pubkeys: vec![sender, receiver],
            },
            Tapscript::CsvMultisig {
                timelock: case.unilateral_refund_without_receiver_delay.to_timelock(),
                pubkeys: vec![sender],
            },
        ];
        let expected_leaves = [
            &case.scripts.claim_script,
            &case.scripts.refund_script,
            &case.scripts.refund_without_receiver_script,
            &case.scripts.unilateral_claim_script,
            &case.scripts.unilateral_refund_script,
            &case.scripts.unilateral_refund_without_receiver_script,
        ];
        for (closure, expected) in closures.iter().zip(expected_leaves) {
            let script = closure.to_script().unwrap();
            assert_eq!(&hex::encode(script.as_bytes()), expected, "{name}");
            let decoded = Tapscript::decode(&ScriptBuf::from_bytes(hex::decode(expected).unwrap()));
            assert_eq!(&decoded.unwrap(), closure, "{name}");
        }

        let vtxo = VtxoScript::from_tapscripts(&closures).unwrap();
        assert_eq!(
            hex::encode(vtxo.tweaked_key().serialize()),
            case.taproot.tweaked_public_key,
            "{name}"
        );
        assert_eq!(
            hex::encode(vtxo.encode_tap_tree()),
            case.taproot.tap_tree,
            "{name}"
        );
        assert_eq!(
            &case.taproot.internal_key[2..],
            hex::encode(coordinator_ark_escrow::UNSPENDABLE_INTERNAL_KEY)
        );

        let decoded = VtxoScript::decode_tap_tree(&hex::decode(&case.taproot.tap_tree).unwrap());
        assert_eq!(decoded.unwrap(), vtxo, "{name}");

        let address = vtxo.address(TESTNET_HRP, server).unwrap();
        assert_eq!(address.encode(), case.expected, "{name}");
        assert_eq!(
            ArkAddress::decode(&case.expected).unwrap(),
            address,
            "{name}"
        );
    }
}
