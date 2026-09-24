//! Entry escrow vectors built with `@arkade-os/sdk` (`tests/fixtures/escrow.json`).
//!
//! `generate-escrow-vectors.mjs` builds each leaf with the SDK's own closure encoders.
//! This crate must match every leaf, the tree, the tap tree field, and the address.

mod common;

use bitcoin::absolute::LockTime;
use coordinator_ark_escrow::{
    ArkAddress, EntryEscrow, EscrowPath, EscrowTerms, Tapscript, VtxoScript, TESTNET_HRP,
};
use serde::Deserialize;

use common::{fixture, xonly, JsonTimelock};

#[derive(Deserialize)]
struct Fixtures {
    generator: String,
    vectors: Vec<Vector>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vector {
    description: String,
    player: String,
    coordinator: String,
    server: String,
    refund_locktime: u32,
    exit_delay: JsonTimelock,
    unilateral_refund_delay: JsonTimelock,
    expected: Expected,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Expected {
    leaves: Vec<String>,
    leaf_types: Vec<String>,
    tweaked_public_key: String,
    tap_tree: String,
    address: String,
}

fn sdk_type(tapscript: &Tapscript) -> &'static str {
    match tapscript {
        Tapscript::Multisig { .. } => "multisig",
        Tapscript::CsvMultisig { .. } => "csv-multisig",
        Tapscript::CltvMultisig { .. } => "cltv-multisig",
        Tapscript::ConditionMultisig { .. } => "condition-multisig",
        Tapscript::ConditionCsvMultisig { .. } => "condition-csv-multisig",
    }
}

#[test]
fn escrow_matches_the_sdk() {
    let fixtures: Fixtures = fixture("escrow.json");
    assert!(fixtures.generator.starts_with("@arkade-os/sdk@"));
    assert_eq!(fixtures.vectors.len(), 6);
    for vector in fixtures.vectors {
        let name = &vector.description;
        let escrow = EntryEscrow::new(EscrowTerms {
            player: xonly(&vector.player),
            coordinator: xonly(&vector.coordinator),
            server: xonly(&vector.server),
            refund_locktime: LockTime::from_consensus(vector.refund_locktime),
            exit_delay: vector.exit_delay.to_timelock(),
            unilateral_refund_delay: vector.unilateral_refund_delay.to_timelock(),
        })
        .unwrap();

        for (index, path) in EscrowPath::ALL.into_iter().enumerate() {
            assert_eq!(
                hex::encode(escrow.script(path).as_bytes()),
                vector.expected.leaves[index],
                "{name}: {path:?}"
            );
            assert_eq!(
                sdk_type(escrow.tapscript(path)),
                vector.expected.leaf_types[index],
                "{name}: {path:?}"
            );
        }

        let vtxo = escrow.vtxo_script();
        assert_eq!(
            hex::encode(vtxo.tweaked_key().serialize()),
            vector.expected.tweaked_public_key,
            "{name}"
        );
        assert_eq!(
            hex::encode(vtxo.encode_tap_tree()),
            vector.expected.tap_tree,
            "{name}"
        );

        let address = escrow.address(TESTNET_HRP).unwrap();
        assert_eq!(address.encode(), vector.expected.address, "{name}");
        assert_eq!(
            ArkAddress::decode(&vector.expected.address).unwrap(),
            address
        );

        // A tap tree from an SDK client decodes back to the same escrow terms.
        let from_sdk =
            VtxoScript::decode_tap_tree(&hex::decode(&vector.expected.tap_tree).unwrap()).unwrap();
        assert_eq!(
            EntryEscrow::from_vtxo_script(&from_sdk).unwrap(),
            escrow,
            "{name}"
        );
    }
}
