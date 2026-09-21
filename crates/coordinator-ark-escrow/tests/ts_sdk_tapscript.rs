//! Ported from `@arkade-os/sdk`'s `test/tapscript.test.ts`
//! (arkade-os/ts-sdk 60d258f5dc9e1c7cc28440cc3eafce6b65f61667).

mod common;

use bitcoin::absolute::LockTime;
use bitcoin::opcodes::all::{OP_EQUAL, OP_PUSHNUM_1, OP_SHA256};
use bitcoin::script::Builder;
use bitcoin::{Script, ScriptBuf};
use coordinator_ark_escrow::{Error, RelativeTimelock, Tapscript, VtxoScript};
use serde::Deserialize;

use common::{fixture, xonly};

const EX_PUBKEY_1: &str = "f8352deebdf5658d95875d89656112b1dd150f176c702eea4f91a91527e48e26";
const EX_PUBKEY_2: &str = "fc68d5ea9279cc9d2c57e6885e21bbaee9c3aec85089f1d6c705c017d321ea84";
const EX_HASH: &str = "628850cb844fe63c308c62afc8bc5351f1952a7f";

fn round_trips(tapscript: Tapscript) {
    let script = tapscript.to_script().unwrap();
    assert_eq!(Tapscript::decode(&script).unwrap(), tapscript);
}

#[test]
fn multisig_single_key() {
    round_trips(Tapscript::Multisig {
        pubkeys: vec![xonly(EX_PUBKEY_1)],
    });
}

#[test]
fn multisig_two_of_two() {
    round_trips(Tapscript::Multisig {
        pubkeys: vec![xonly(EX_PUBKEY_1), xonly(EX_PUBKEY_2)],
    });
}

#[test]
fn empty_script_fails() {
    assert_eq!(Tapscript::decode(Script::new()), Err(Error::EmptyScript));
}

#[test]
fn invalid_pubkey_length_fails() {
    let script = Builder::new()
        .push_slice(b"invalid")
        .push_opcode(bitcoin::opcodes::all::OP_CHECKSIG)
        .into_script();
    assert!(Tapscript::decode(&script).is_err());
}

#[test]
fn csv_multisig_with_blocks() {
    round_trips(Tapscript::CsvMultisig {
        timelock: RelativeTimelock::Blocks(144),
        pubkeys: vec![xonly(EX_PUBKEY_1)],
    });
}

#[test]
fn csv_multisig_with_seconds() {
    round_trips(Tapscript::CsvMultisig {
        timelock: RelativeTimelock::Seconds(512 * 4),
        pubkeys: vec![xonly(EX_PUBKEY_1), xonly(EX_PUBKEY_2)],
    });
}

#[test]
fn too_short_timelocked_script_fails() {
    let script = ScriptBuf::from_bytes(vec![0x01, 0x02]);
    assert!(Tapscript::decode(&script).is_err());
}

#[test]
fn cltv_multisig_with_timestamp() {
    round_trips(Tapscript::CltvMultisig {
        locktime: LockTime::from_consensus(1_687_459_200),
        pubkeys: vec![xonly(EX_PUBKEY_1)],
    });
}

#[test]
fn cltv_multisig_with_small_height() {
    round_trips(Tapscript::CltvMultisig {
        locktime: LockTime::from_consensus(10),
        pubkeys: vec![xonly(EX_PUBKEY_1)],
    });
}

#[test]
fn condition_csv_multisig() {
    round_trips(Tapscript::ConditionCsvMultisig {
        condition: Builder::new().push_opcode(OP_PUSHNUM_1).into_script(),
        timelock: RelativeTimelock::Blocks(144),
        pubkeys: vec![xonly(EX_PUBKEY_1)],
    });
}

#[test]
fn condition_multisig_with_hash() {
    let hash: [u8; 20] = hex::decode(EX_HASH).unwrap().try_into().unwrap();
    round_trips(Tapscript::ConditionMultisig {
        condition: Builder::new()
            .push_opcode(OP_SHA256)
            .push_slice(hash)
            .push_opcode(OP_EQUAL)
            .into_script(),
        pubkeys: vec![xonly(EX_PUBKEY_1), xonly(EX_PUBKEY_2)],
    });
}

#[derive(Deserialize)]
struct VtxoScriptFixture {
    name: String,
    scripts: Vec<String>,
    #[serde(rename = "taprootKey")]
    taproot_key: String,
}

/// Golden vectors whose keys were produced with btcd's `AssembleTaprootScriptTree`.
#[test]
fn vtxo_script_fixtures() {
    let fixtures: Vec<VtxoScriptFixture> = fixture("vtxoscript.json");
    assert_eq!(fixtures.len(), 10);
    for case in fixtures {
        let scripts = case
            .scripts
            .iter()
            .map(|script| ScriptBuf::from_bytes(hex::decode(script).unwrap()))
            .collect();
        let vtxo = VtxoScript::new(scripts).unwrap();
        assert_eq!(
            hex::encode(vtxo.tweaked_key().serialize()),
            case.taproot_key,
            "{}",
            case.name
        );
    }
}
