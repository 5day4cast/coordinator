//! Virtual output scripts: a taproot output over a list of Arkade tapscripts.

use std::collections::VecDeque;

use bitcoin::key::Secp256k1;
use bitcoin::taproot::{ControlBlock, LeafVersion, NodeInfo, TaprootSpendInfo};
use bitcoin::{Script, ScriptBuf, XOnlyPublicKey};

use crate::{ArkAddress, Error, Tapscript};

/// BIP341's provably unspendable internal key, the `H` point. Arkade uses it for every VTXO.
pub const UNSPENDABLE_INTERNAL_KEY: [u8; 32] = [
    0x50, 0x92, 0x9b, 0x74, 0xc1, 0xa0, 0x49, 0x54, 0xb7, 0x8b, 0x4b, 0x60, 0x35, 0xe9, 0x7a, 0x5e,
    0x07, 0x8a, 0x5a, 0x0f, 0x28, 0xec, 0x96, 0xd5, 0x47, 0xbf, 0xee, 0x9a, 0xce, 0x80, 0x3a, 0xc0,
];

/// Assemble a taproot tree exactly as btcd's `txscript.AssembleTaprootScriptTree`, which arkd uses.
///
/// Pair the leaves left to right, merging an odd final leaf into the last pair.
/// Then combine the front two branches of a FIFO queue until one root remains.
/// A Huffman builder agrees with this only for power-of-two leaf counts.
pub fn assemble_btcd_tree(scripts: &[ScriptBuf]) -> Result<NodeInfo, Error> {
    let mut leaves = scripts
        .iter()
        .map(|script| NodeInfo::new_leaf_with_ver(script.clone(), LeafVersion::TapScript));
    let first = leaves.next().ok_or(Error::NoLeaves)?;
    let Some(second) = leaves.next() else {
        return Ok(first);
    };
    let combine = |left: NodeInfo, right: NodeInfo| {
        NodeInfo::combine(left, right).map_err(|error| Error::Taproot(error.to_string()))
    };

    let mut branches = VecDeque::from([combine(first, second)?]);
    loop {
        match (leaves.next(), leaves.next()) {
            (Some(left), Some(right)) => branches.push_back(combine(left, right)?),
            (Some(odd), None) => {
                let last = branches.pop_back().expect("at least one branch exists");
                branches.push_back(combine(last, odd)?);
                break;
            }
            (None, _) => break,
        }
    }

    while branches.len() > 1 {
        let left = branches.pop_front().expect("two branches remain");
        let right = branches.pop_front().expect("two branches remain");
        branches.push_back(combine(left, right)?);
    }
    Ok(branches.pop_front().expect("one root remains"))
}

/// A virtual output's script: leaf scripts, in order, committed under the unspendable key.
#[derive(Debug, Clone)]
pub struct VtxoScript {
    scripts: Vec<ScriptBuf>,
    spend_info: TaprootSpendInfo,
}

impl PartialEq for VtxoScript {
    fn eq(&self, other: &Self) -> bool {
        self.scripts == other.scripts
    }
}

impl Eq for VtxoScript {}

impl VtxoScript {
    /// Build from raw leaf scripts, in order.
    pub fn new(scripts: Vec<ScriptBuf>) -> Result<Self, Error> {
        let secp = Secp256k1::verification_only();
        let internal_key = XOnlyPublicKey::from_slice(&UNSPENDABLE_INTERNAL_KEY)
            .expect("the BIP341 H point is a valid x-only key");
        let tree = assemble_btcd_tree(&scripts)?;
        let spend_info = TaprootSpendInfo::from_node_info(&secp, internal_key, tree);
        Ok(Self {
            scripts,
            spend_info,
        })
    }

    /// Build from closures, in order.
    pub fn from_tapscripts(tapscripts: &[Tapscript]) -> Result<Self, Error> {
        Self::new(
            tapscripts
                .iter()
                .map(Tapscript::to_script)
                .collect::<Result<_, _>>()?,
        )
    }

    /// The leaf scripts, in the order they were given.
    pub fn scripts(&self) -> &[ScriptBuf] {
        &self.scripts
    }

    /// The tweaked taproot output key.
    pub fn tweaked_key(&self) -> XOnlyPublicKey {
        self.spend_info.output_key().to_x_only_public_key()
    }

    /// The P2TR output script.
    pub fn script_pubkey(&self) -> ScriptBuf {
        ScriptBuf::new_p2tr_tweaked(self.spend_info.output_key())
    }

    /// The control block for spending through `script`, if it is one of the leaves.
    pub fn control_block(&self, script: &Script) -> Option<ControlBlock> {
        self.spend_info
            .control_block(&(script.to_owned(), LeafVersion::TapScript))
    }

    /// The Arkade address for this script under `server`.
    pub fn address(&self, hrp: &str, server: XOnlyPublicKey) -> Result<ArkAddress, Error> {
        ArkAddress::new(hrp, server, self.tweaked_key())
    }

    /// Encode as a PSBT `TapTree` field, the form arkd and `@arkade-os/sdk` exchange.
    ///
    /// Every leaf is written at depth 1. The receiver rebuilds the tree with the btcd algorithm.
    pub fn encode_tap_tree(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for script in &self.scripts {
            bytes.push(1);
            bytes.push(LeafVersion::TapScript.to_consensus());
            write_compact_size(&mut bytes, script.len() as u64);
            bytes.extend_from_slice(script.as_bytes());
        }
        bytes
    }

    /// Decode a PSBT `TapTree` field. Depths are ignored, and the tree is rebuilt with the btcd algorithm.
    pub fn decode_tap_tree(bytes: &[u8]) -> Result<Self, Error> {
        let mut scripts = Vec::new();
        let mut cursor = bytes;
        while !cursor.is_empty() {
            let [_depth, version, rest @ ..] = cursor else {
                return Err(Error::TapTree("truncated leaf header".into()));
            };
            if *version != LeafVersion::TapScript.to_consensus() {
                return Err(Error::TapTree(format!(
                    "unsupported leaf version {version:#x}"
                )));
            }
            let (length, rest) = read_compact_size(rest)?;
            let length = usize::try_from(length)
                .map_err(|_| Error::TapTree("leaf length overflows".into()))?;
            if rest.len() < length {
                return Err(Error::TapTree("truncated leaf script".into()));
            }
            scripts.push(ScriptBuf::from_bytes(rest[..length].to_vec()));
            cursor = &rest[length..];
        }
        Self::new(scripts)
    }
}

fn write_compact_size(bytes: &mut Vec<u8>, value: u64) {
    match value {
        0..=0xfc => bytes.push(value as u8),
        0xfd..=0xffff => {
            bytes.push(0xfd);
            bytes.extend_from_slice(&(value as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            bytes.push(0xfe);
            bytes.extend_from_slice(&(value as u32).to_le_bytes());
        }
        _ => {
            bytes.push(0xff);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
}

fn read_compact_size(bytes: &[u8]) -> Result<(u64, &[u8]), Error> {
    let truncated = || Error::TapTree("truncated length".into());
    let (&first, rest) = bytes.split_first().ok_or_else(truncated)?;
    let (width, minimum) = match first {
        0xfd => (2, 0xfd),
        0xfe => (4, 0x1_0000),
        0xff => (8, 0x1_0000_0000),
        value => return Ok((u64::from(value), rest)),
    };
    if rest.len() < width {
        return Err(truncated());
    }
    let mut le = [0u8; 8];
    le[..width].copy_from_slice(&rest[..width]);
    let value = u64::from_le_bytes(le);
    if value < minimum {
        return Err(Error::TapTree("non-minimal length".into()));
    }
    Ok((value, &rest[width..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy(count: u8) -> Vec<ScriptBuf> {
        (0..count)
            .map(|byte| ScriptBuf::from_bytes(vec![0x01, byte]))
            .collect()
    }

    #[test]
    fn every_leaf_gets_a_control_block() {
        for count in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 15, 16, 17, 32] {
            let vtxo = VtxoScript::new(dummy(count)).unwrap();
            let secp = Secp256k1::verification_only();
            for script in vtxo.scripts() {
                let control = vtxo.control_block(script).unwrap();
                assert!(control.verify_taproot_commitment(&secp, vtxo.tweaked_key(), script));
            }
        }
    }

    #[test]
    fn empty_scripts_are_rejected() {
        assert_eq!(VtxoScript::new(vec![]).unwrap_err(), Error::NoLeaves);
    }

    #[test]
    fn tap_tree_round_trips() {
        for count in [1, 3, 10] {
            let original = VtxoScript::new(dummy(count)).unwrap();
            let decoded = VtxoScript::decode_tap_tree(&original.encode_tap_tree()).unwrap();
            assert_eq!(decoded, original);
            assert_eq!(decoded.tweaked_key(), original.tweaked_key());
        }
    }

    #[test]
    fn compact_size_round_trips() {
        for value in [
            0u64,
            0xfc,
            0xfd,
            0xffff,
            0x1_0000,
            0xffff_ffff,
            0x1_0000_0000,
        ] {
            let mut bytes = Vec::new();
            write_compact_size(&mut bytes, value);
            assert_eq!(read_compact_size(&bytes).unwrap(), (value, &[][..]));
        }
        assert!(read_compact_size(&[0xfd, 0x10, 0x00]).is_err());
    }
}
