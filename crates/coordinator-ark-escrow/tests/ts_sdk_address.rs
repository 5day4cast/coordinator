//! `@arkade-os/sdk`'s address fixtures (`test/fixtures/encoding.json`).

mod common;

use coordinator_ark_escrow::{ArkAddress, ADDRESS_VERSION};
use serde::Deserialize;

use common::{fixture, xonly};

#[derive(Deserialize)]
struct Fixtures {
    address: Addresses,
}

#[derive(Deserialize)]
struct Addresses {
    valid: Vec<Valid>,
    invalid: Vec<Invalid>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Valid {
    addr: String,
    expected_version: u8,
    expected_prefix: String,
    expected_user_key: String,
    expected_server_key: String,
}

#[derive(Deserialize)]
struct Invalid {
    addr: String,
}

#[test]
fn valid_addresses_decode_and_re_encode() {
    let fixtures: Fixtures = fixture("encoding.json");
    for case in fixtures.address.valid {
        let address = ArkAddress::decode(&case.addr).unwrap();
        assert_eq!(case.expected_version, ADDRESS_VERSION);
        assert_eq!(address.hrp(), case.expected_prefix);
        assert_eq!(address.vtxo_taproot_key(), xonly(&case.expected_user_key));
        assert_eq!(address.server(), xonly(&case.expected_server_key));
        assert_eq!(address.encode(), case.addr);
    }
}

#[test]
fn invalid_addresses_are_rejected() {
    let fixtures: Fixtures = fixture("encoding.json");
    assert!(!fixtures.address.invalid.is_empty());
    for case in fixtures.address.invalid {
        assert!(ArkAddress::decode(&case.addr).is_err(), "{}", case.addr);
    }
}
