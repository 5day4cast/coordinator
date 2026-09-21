//! Arkade tapscript closures.
//!
//! Each closure encodes to the same bytes as arkd and `@arkade-os/sdk`.
//! Decoding accepts only canonical scripts: a decoded closure must re-encode to the input bytes.

use bitcoin::absolute::LockTime;
use bitcoin::opcodes::all::{
    OP_CHECKSIG, OP_CHECKSIGVERIFY, OP_CLTV, OP_CSV, OP_DROP, OP_PUSHNUM_1, OP_PUSHNUM_16,
    OP_VERIFY,
};
use bitcoin::opcodes::{Opcode, OP_0};
use bitcoin::script::{Builder, Instruction, PushBytesBuf};
use bitcoin::{Script, ScriptBuf, Sequence, XOnlyPublicKey};

use crate::Error;

/// Relative timelock for `CHECKSEQUENCEVERIFY`, encoded as a BIP68 sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelativeTimelock {
    /// A number of blocks.
    Blocks(u16),
    /// A number of seconds. BIP68 counts seconds in units of 512, so this must be a multiple of 512.
    Seconds(u32),
}

impl RelativeTimelock {
    /// Encode as a BIP68 sequence.
    pub fn to_sequence(self) -> Result<Sequence, Error> {
        match self {
            RelativeTimelock::Blocks(blocks) => Ok(Sequence::from_height(blocks)),
            RelativeTimelock::Seconds(seconds) => {
                if seconds % 512 != 0 {
                    return Err(Error::InvalidTimelock(format!(
                        "{seconds} seconds is not a multiple of 512"
                    )));
                }
                let intervals = u16::try_from(seconds / 512).map_err(|_| {
                    Error::InvalidTimelock(format!("{seconds} seconds exceeds the BIP68 maximum"))
                })?;
                Ok(Sequence::from_512_second_intervals(intervals))
            }
        }
    }

    /// Decode a BIP68 sequence.
    pub fn from_sequence(sequence: Sequence) -> Result<Self, Error> {
        use bitcoin::relative::LockTime as Relative;
        match sequence.to_relative_lock_time() {
            Some(Relative::Blocks(height)) => Ok(RelativeTimelock::Blocks(height.value())),
            Some(Relative::Time(time)) => {
                Ok(RelativeTimelock::Seconds(u32::from(time.value()) * 512))
            }
            None => Err(Error::InvalidTimelock(format!(
                "sequence {:#x} is not a relative locktime",
                sequence.to_consensus_u32()
            ))),
        }
    }
}

/// One Arkade tapscript closure.
///
/// Every closure ends in a multisig of x-only keys:
/// `<pk_1> CHECKSIGVERIFY ... <pk_n> CHECKSIG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tapscript {
    /// `<multisig>`
    Multisig { pubkeys: Vec<XOnlyPublicKey> },
    /// `<sequence> CHECKSEQUENCEVERIFY DROP <multisig>`
    CsvMultisig {
        timelock: RelativeTimelock,
        pubkeys: Vec<XOnlyPublicKey>,
    },
    /// `<locktime> CHECKLOCKTIMEVERIFY DROP <multisig>`
    CltvMultisig {
        locktime: LockTime,
        pubkeys: Vec<XOnlyPublicKey>,
    },
    /// `<condition> VERIFY <multisig>`
    ConditionMultisig {
        condition: ScriptBuf,
        pubkeys: Vec<XOnlyPublicKey>,
    },
    /// `<condition> VERIFY <sequence> CHECKSEQUENCEVERIFY DROP <multisig>`
    ConditionCsvMultisig {
        condition: ScriptBuf,
        timelock: RelativeTimelock,
        pubkeys: Vec<XOnlyPublicKey>,
    },
}

type Decoder = fn(&Script) -> Result<Tapscript, Error>;

impl Tapscript {
    /// The multisig keys, in script order.
    pub fn pubkeys(&self) -> &[XOnlyPublicKey] {
        match self {
            Tapscript::Multisig { pubkeys }
            | Tapscript::CsvMultisig { pubkeys, .. }
            | Tapscript::CltvMultisig { pubkeys, .. }
            | Tapscript::ConditionMultisig { pubkeys, .. }
            | Tapscript::ConditionCsvMultisig { pubkeys, .. } => pubkeys,
        }
    }

    /// Encode the closure.
    pub fn to_script(&self) -> Result<ScriptBuf, Error> {
        let multisig = multisig_bytes(self.pubkeys())?;
        let bytes = match self {
            Tapscript::Multisig { .. } => multisig,
            Tapscript::CsvMultisig { timelock, .. } => [csv_prefix(*timelock)?, multisig].concat(),
            Tapscript::CltvMultisig { locktime, .. } => [cltv_prefix(*locktime), multisig].concat(),
            Tapscript::ConditionMultisig { condition, .. } => {
                [condition.to_bytes(), vec![OP_VERIFY.to_u8()], multisig].concat()
            }
            Tapscript::ConditionCsvMultisig {
                condition,
                timelock,
                ..
            } => [
                condition.to_bytes(),
                vec![OP_VERIFY.to_u8()],
                csv_prefix(*timelock)?,
                multisig,
            ]
            .concat(),
        };
        Ok(ScriptBuf::from_bytes(bytes))
    }

    /// Decode a closure, trying each type in the same order as `@arkade-os/sdk`.
    pub fn decode(script: &Script) -> Result<Self, Error> {
        if script.is_empty() {
            return Err(Error::EmptyScript);
        }
        let decoders: [Decoder; 5] = [
            decode_multisig,
            decode_csv_multisig,
            decode_condition_csv_multisig,
            decode_condition_multisig,
            decode_cltv_multisig,
        ];
        let mut last = Error::InvalidScript("not an Arkade tapscript".into());
        for decoder in decoders {
            match decoder(script) {
                Ok(tapscript) => return Ok(tapscript),
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

/// `<locktime> CHECKLOCKTIMEVERIFY`: a condition that holds from `locktime` on.
///
/// Used before `VERIFY` in a condition closure.
/// `CHECKLOCKTIMEVERIFY` leaves the locktime on the stack, and `VERIFY` consumes it.
/// A non-zero locktime is always true.
pub fn cltv_condition(locktime: LockTime) -> ScriptBuf {
    push_number(Builder::new(), i64::from(locktime.to_consensus_u32()))
        .push_opcode(OP_CLTV)
        .into_script()
}

fn multisig_bytes(pubkeys: &[XOnlyPublicKey]) -> Result<Vec<u8>, Error> {
    if pubkeys.is_empty() {
        return Err(Error::NoPubkeys);
    }
    let mut builder = Builder::new();
    for (index, pubkey) in pubkeys.iter().enumerate() {
        builder = builder.push_x_only_key(pubkey);
        builder = if index + 1 < pubkeys.len() {
            builder.push_opcode(OP_CHECKSIGVERIFY)
        } else {
            builder.push_opcode(OP_CHECKSIG)
        };
    }
    Ok(builder.into_bytes())
}

fn csv_prefix(timelock: RelativeTimelock) -> Result<Vec<u8>, Error> {
    let sequence = timelock.to_sequence()?;
    Ok(
        push_number(Builder::new(), i64::from(sequence.to_consensus_u32()))
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .into_bytes(),
    )
}

fn cltv_prefix(locktime: LockTime) -> Vec<u8> {
    push_number(Builder::new(), i64::from(locktime.to_consensus_u32()))
        .push_opcode(OP_CLTV)
        .push_opcode(OP_DROP)
        .into_bytes()
}

/// Push a non-negative number the way `@scure/btc-signer` does:
/// `OP_0`, `OP_1` to `OP_16`, or the minimal script-number bytes.
fn push_number(builder: Builder, value: i64) -> Builder {
    debug_assert!(value >= 0);
    match value {
        0 => builder.push_opcode(OP_0),
        1..=16 => builder.push_opcode(Opcode::from(OP_PUSHNUM_1.to_u8() + (value - 1) as u8)),
        _ => {
            let bytes = PushBytesBuf::try_from(script_number_bytes(value))
                .expect("a script number is at most 9 bytes");
            builder.push_slice(bytes)
        }
    }
}

fn script_number_bytes(value: i64) -> Vec<u8> {
    let mut magnitude = value.unsigned_abs();
    let mut bytes = Vec::new();
    while magnitude > 0 {
        bytes.push((magnitude & 0xff) as u8);
        magnitude >>= 8;
    }
    if bytes.last().is_some_and(|last| last & 0x80 != 0) {
        bytes.push(if value < 0 { 0x80 } else { 0x00 });
    } else if value < 0 {
        if let Some(last) = bytes.last_mut() {
            *last |= 0x80;
        }
    }
    bytes
}

/// Read a number pushed by [`push_number`], rejecting non-minimal encodings.
fn read_number(instruction: &Instruction<'_>) -> Result<i64, Error> {
    match instruction {
        Instruction::Op(op) => {
            let code = op.to_u8();
            if (OP_PUSHNUM_1.to_u8()..=OP_PUSHNUM_16.to_u8()).contains(&code) {
                Ok(i64::from(code - OP_PUSHNUM_1.to_u8() + 1))
            } else {
                Err(Error::InvalidScript(format!("expected a number, got {op}")))
            }
        }
        Instruction::PushBytes(bytes) => {
            let bytes = bytes.as_bytes();
            if bytes.is_empty() {
                return Ok(0);
            }
            if bytes.len() > 5 {
                return Err(Error::InvalidScript("number is longer than 5 bytes".into()));
            }
            let last = bytes[bytes.len() - 1];
            if last & 0x7f == 0 && (bytes.len() == 1 || bytes[bytes.len() - 2] & 0x80 == 0) {
                return Err(Error::NonCanonical);
            }
            let mut value: i64 = 0;
            for (index, byte) in bytes.iter().enumerate() {
                value |= i64::from(*byte) << (8 * index);
            }
            if last & 0x80 != 0 {
                value &= !(0x80_i64 << (8 * (bytes.len() - 1)));
                value = -value;
            }
            Ok(value)
        }
    }
}

fn instructions(script: &Script) -> Result<Vec<(usize, Instruction<'_>)>, Error> {
    script
        .instruction_indices_minimal()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::InvalidScript(error.to_string()))
}

fn require_canonical(decoded: Tapscript, script: &Script) -> Result<Tapscript, Error> {
    if decoded.to_script()?.as_script() == script {
        Ok(decoded)
    } else {
        Err(Error::NonCanonical)
    }
}

fn decode_multisig(script: &Script) -> Result<Tapscript, Error> {
    if script.is_empty() {
        return Err(Error::EmptyScript);
    }
    let instructions = instructions(script)?;
    if instructions.is_empty() || instructions.len() % 2 != 0 {
        return Err(Error::InvalidScript("not a multisig".into()));
    }
    let count = instructions.len() / 2;
    let mut pubkeys = Vec::with_capacity(count);
    for (index, pair) in instructions.chunks(2).enumerate() {
        let key = match &pair[0].1 {
            Instruction::PushBytes(bytes) if bytes.len() == 32 => {
                XOnlyPublicKey::from_slice(bytes.as_bytes())
                    .map_err(|error| Error::InvalidScript(error.to_string()))?
            }
            _ => return Err(Error::InvalidScript("expected a 32-byte public key".into())),
        };
        let expected = if index + 1 < count {
            OP_CHECKSIGVERIFY
        } else {
            OP_CHECKSIG
        };
        match pair[1].1 {
            Instruction::Op(op) if op == expected => {}
            _ => return Err(Error::InvalidScript(format!("expected {expected}"))),
        }
        pubkeys.push(key);
    }
    require_canonical(Tapscript::Multisig { pubkeys }, script)
}

/// Split `<number> <op> DROP <multisig>`.
fn decode_timelocked(script: &Script, expected: Opcode) -> Result<(i64, &Script), Error> {
    if script.is_empty() {
        return Err(Error::EmptyScript);
    }
    let instructions = instructions(script)?;
    if instructions.len() < 3 {
        return Err(Error::TooShort { expected: 3 });
    }
    let number = read_number(&instructions[0].1)?;
    match (&instructions[1].1, &instructions[2].1) {
        (Instruction::Op(op), Instruction::Op(drop)) if *op == expected && *drop == OP_DROP => {}
        _ => return Err(Error::InvalidScript(format!("expected {expected} DROP"))),
    }
    let rest = instructions
        .get(3)
        .map(|(offset, _)| *offset)
        .ok_or_else(|| Error::InvalidScript("missing multisig".into()))?;
    Ok((number, Script::from_bytes(&script.as_bytes()[rest..])))
}

fn decode_csv_multisig(script: &Script) -> Result<Tapscript, Error> {
    let (number, rest) = decode_timelocked(script, OP_CSV)?;
    let sequence = u32::try_from(number)
        .map(Sequence::from_consensus)
        .map_err(|_| Error::InvalidTimelock(format!("{number} is not a sequence")))?;
    let timelock = RelativeTimelock::from_sequence(sequence)?;
    let Tapscript::Multisig { pubkeys } = decode_multisig(rest)? else {
        unreachable!("decode_multisig returns a multisig")
    };
    require_canonical(Tapscript::CsvMultisig { timelock, pubkeys }, script)
}

fn decode_cltv_multisig(script: &Script) -> Result<Tapscript, Error> {
    let (number, rest) = decode_timelocked(script, OP_CLTV)?;
    let locktime = u32::try_from(number)
        .map(LockTime::from_consensus)
        .map_err(|_| Error::InvalidScript(format!("{number} is not a locktime")))?;
    let Tapscript::Multisig { pubkeys } = decode_multisig(rest)? else {
        unreachable!("decode_multisig returns a multisig")
    };
    require_canonical(Tapscript::CltvMultisig { locktime, pubkeys }, script)
}

/// Split `<condition> VERIFY <tail>` at the last `VERIFY`, as `@arkade-os/sdk` does.
fn split_condition(script: &Script) -> Result<(ScriptBuf, &Script), Error> {
    if script.is_empty() {
        return Err(Error::EmptyScript);
    }
    let instructions = instructions(script)?;
    let verify = instructions
        .iter()
        .rev()
        .find(|(_, instruction)| matches!(instruction, Instruction::Op(op) if *op == OP_VERIFY))
        .map(|(offset, _)| *offset)
        .ok_or_else(|| Error::InvalidScript("missing VERIFY".into()))?;
    let bytes = script.as_bytes();
    Ok((
        ScriptBuf::from_bytes(bytes[..verify].to_vec()),
        Script::from_bytes(&bytes[verify + 1..]),
    ))
}

fn decode_condition_multisig(script: &Script) -> Result<Tapscript, Error> {
    let (condition, rest) = split_condition(script)?;
    let Tapscript::Multisig { pubkeys } = decode_multisig(rest)? else {
        unreachable!("decode_multisig returns a multisig")
    };
    require_canonical(Tapscript::ConditionMultisig { condition, pubkeys }, script)
}

fn decode_condition_csv_multisig(script: &Script) -> Result<Tapscript, Error> {
    let (condition, rest) = split_condition(script)?;
    let Tapscript::CsvMultisig { timelock, pubkeys } = decode_csv_multisig(rest)? else {
        unreachable!("decode_csv_multisig returns a CSV multisig")
    };
    require_canonical(
        Tapscript::ConditionCsvMultisig {
            condition,
            timelock,
            pubkeys,
        },
        script,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_numbers_are_minimal() {
        for (value, bytes) in [
            (17_i64, vec![0x11]),
            (127, vec![0x7f]),
            (128, vec![0x80, 0x00]),
            (144, vec![0x90, 0x00]),
            (265, vec![0x09, 0x01]),
            (0x40_0001, vec![0x01, 0x00, 0x40]),
            (1_687_459_200, vec![0x80, 0x95, 0x94, 0x64]),
            (0xffff_ffff, vec![0xff, 0xff, 0xff, 0xff, 0x00]),
        ] {
            assert_eq!(script_number_bytes(value), bytes, "{value}");
            let pushed = push_number(Builder::new(), value).into_script();
            let instruction = pushed.instructions_minimal().next().unwrap().unwrap();
            assert_eq!(read_number(&instruction).unwrap(), value);
        }
    }

    #[test]
    fn small_numbers_use_opcodes() {
        assert_eq!(push_number(Builder::new(), 10).into_bytes(), vec![0x5a]);
        assert_eq!(push_number(Builder::new(), 16).into_bytes(), vec![0x60]);
        assert_eq!(
            push_number(Builder::new(), 17).into_bytes(),
            vec![0x01, 0x11]
        );
    }

    #[test]
    fn non_minimal_numbers_are_rejected() {
        let script = Builder::new()
            .push_slice([0x90, 0x00, 0x00])
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .into_script();
        let instruction = script.instructions_minimal().next().unwrap().unwrap();
        assert_eq!(read_number(&instruction), Err(Error::NonCanonical));
    }

    #[test]
    fn relative_timelocks_match_bip68() {
        assert_eq!(
            RelativeTimelock::Blocks(144)
                .to_sequence()
                .unwrap()
                .to_consensus_u32(),
            144
        );
        assert_eq!(
            RelativeTimelock::Seconds(512 * 4)
                .to_sequence()
                .unwrap()
                .to_consensus_u32(),
            0x40_0004
        );
        assert!(RelativeTimelock::Seconds(1000).to_sequence().is_err());
        assert!(RelativeTimelock::from_sequence(Sequence::MAX).is_err());
    }
}
