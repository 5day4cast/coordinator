//! Arkade addresses.
//!
//! An address is bech32m over `version (1 byte) || server x-only key (32) || VTXO taproot key (32)`.
//! Such an address is longer than bech32's usual 90-character limit.

use bitcoin::bech32::primitives::decode::CheckedHrpstring;
use bitcoin::bech32::{self, Bech32m, Hrp};
use bitcoin::XOnlyPublicKey;

use crate::Error;

/// The only address version arkd defines.
pub const ADDRESS_VERSION: u8 = 0;

/// Human-readable prefix for mainnet addresses.
pub const MAINNET_HRP: &str = "ark";
/// Human-readable prefix for test network addresses, including Mutinynet.
pub const TESTNET_HRP: &str = "tark";

/// An Arkade address: where a VTXO with a given taproot key lives on a given server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArkAddress {
    hrp: Hrp,
    server: XOnlyPublicKey,
    vtxo_taproot_key: XOnlyPublicKey,
}

impl ArkAddress {
    pub fn new(
        hrp: &str,
        server: XOnlyPublicKey,
        vtxo_taproot_key: XOnlyPublicKey,
    ) -> Result<Self, Error> {
        let hrp = Hrp::parse(hrp).map_err(|error| Error::Address(error.to_string()))?;
        Ok(Self {
            hrp,
            server,
            vtxo_taproot_key,
        })
    }

    pub fn hrp(&self) -> &str {
        self.hrp.as_str()
    }

    pub fn server(&self) -> XOnlyPublicKey {
        self.server
    }

    pub fn vtxo_taproot_key(&self) -> XOnlyPublicKey {
        self.vtxo_taproot_key
    }

    pub fn encode(&self) -> String {
        let mut data = Vec::with_capacity(65);
        data.push(ADDRESS_VERSION);
        data.extend_from_slice(&self.server.serialize());
        data.extend_from_slice(&self.vtxo_taproot_key.serialize());
        bech32::encode::<Bech32m>(self.hrp, &data).expect("a 65-byte payload fits bech32m")
    }

    pub fn decode(address: &str) -> Result<Self, Error> {
        let checked = CheckedHrpstring::new::<Bech32m>(address)
            .map_err(|error| Error::Address(error.to_string()))?;
        let hrp = checked.hrp();
        let data: Vec<u8> = checked.byte_iter().collect();
        let [version, keys @ ..] = data.as_slice() else {
            return Err(Error::Address("empty payload".into()));
        };
        if *version != ADDRESS_VERSION {
            return Err(Error::Address(format!("unsupported version {version}")));
        }
        if keys.len() != 64 {
            return Err(Error::Address(format!(
                "expected 64 key bytes, got {}",
                keys.len()
            )));
        }
        let key = |bytes: &[u8]| {
            XOnlyPublicKey::from_slice(bytes).map_err(|error| Error::Address(error.to_string()))
        };
        Ok(Self {
            hrp,
            server: key(&keys[..32])?,
            vtxo_taproot_key: key(&keys[32..])?,
        })
    }
}

impl std::fmt::Display for ArkAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.encode())
    }
}
