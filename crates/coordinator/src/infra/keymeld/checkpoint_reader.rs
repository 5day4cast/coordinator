//! Reconstruct checkpoint JSON without retaining its complete plaintext.
//! AEAD authenticates each compressed part before it is read. Its content MAC
//! and gzip trailer are checked before advancing to the next manifest token.
use super::{context, part_mac, Manifest};
use flate2::read::GzDecoder;
use hmac::{Hmac, Mac};
use keymeld_core::{
    crypto::{EncryptedData, SessionSecret},
    SessionId,
};
use sha2::Sha256;
use std::{
    collections::{btree_map, BTreeMap},
    io::{self, Cursor, Read},
};
use zeroize::Zeroizing;

enum Task<'a> {
    Value(&'a Manifest),
    Object(btree_map::Iter<'a, String, Manifest>, bool),
    Array(std::slice::Iter<'a, Manifest>, bool),
    Token(&'static [u8]),
    Name(&'a str),
}

struct Compressed {
    bytes: Zeroizing<Vec<u8>>,
    offset: usize,
}
impl Read for Compressed {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = output.len().min(self.bytes.len() - self.offset);
        output[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

enum Chunk {
    Token(Cursor<Vec<u8>>),
    Part(Box<Part>),
}
struct Part {
    decoder: GzDecoder<Compressed>,
    mac: Hmac<Sha256>,
    digest: [u8; 32],
}

pub(super) struct Reader<'a> {
    tasks: Vec<Task<'a>>,
    current: Option<Chunk>,
    key: &'a SessionSecret,
    session: &'a SessionId,
    bodies: &'a BTreeMap<Vec<u8>, Vec<u8>>,
    limit: usize,
    read: usize,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Amortize Serde's byte reads without leaving the plaintext staging buffer
/// allocated after decoding or a failed authentication check.
pub(super) struct Buffered<R> {
    inner: R,
    buffer: Zeroizing<Vec<u8>>,
    start: usize,
    end: usize,
}
impl<R> Buffered<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Zeroizing::new(vec![0; 32 * 1024]),
            start: 0,
            end: 0,
        }
    }
}
impl<R: Read> Read for Buffered<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.start == self.end {
            self.end = self.inner.read(&mut self.buffer)?;
            self.start = 0;
        }
        let count = output.len().min(self.end - self.start);
        output[..count].copy_from_slice(&self.buffer[self.start..self.start + count]);
        self.start += count;
        Ok(count)
    }
}

impl<'a> Reader<'a> {
    pub(super) fn new(
        manifest: &'a Manifest,
        key: &'a SessionSecret,
        session: &'a SessionId,
        bodies: &'a BTreeMap<Vec<u8>, Vec<u8>>,
        limit: usize,
    ) -> Self {
        Self {
            tasks: vec![Task::Value(manifest)],
            current: None,
            key,
            session,
            bodies,
            limit,
            read: 0,
        }
    }

    pub(super) fn bytes_read(&self) -> usize {
        self.read
    }

    fn next_chunk(&mut self) -> io::Result<Option<Chunk>> {
        while let Some(task) = self.tasks.pop() {
            let token = match task {
                Task::Value(Manifest::Part(digest)) => {
                    let body = self
                        .bodies
                        .get(digest.as_slice())
                        .ok_or_else(|| invalid("Checkpoint part missing"))?;
                    let encrypted = EncryptedData::from_bytes(body)
                        .map_err(|_| invalid("Invalid checkpoint part encoding"))?;
                    let bytes = Zeroizing::new(
                        self.key
                            .decrypt(&encrypted, &context(self.session, digest))
                            .map_err(|_| invalid("Checkpoint part authentication failed"))?,
                    );
                    return Ok(Some(Chunk::Part(Box::new(Part {
                        decoder: GzDecoder::new(Compressed { bytes, offset: 0 }),
                        mac: part_mac(self.key, self.session),
                        digest: *digest,
                    }))));
                }
                Task::Value(Manifest::Object(values)) => {
                    self.tasks.push(Task::Object(values.iter(), true));
                    b"{".to_vec()
                }
                Task::Value(Manifest::Array(values)) => {
                    self.tasks.push(Task::Array(values.iter(), true));
                    b"[".to_vec()
                }
                Task::Object(mut fields, first) => {
                    if let Some((name, value)) = fields.next() {
                        self.tasks.push(Task::Object(fields, false));
                        self.tasks.push(Task::Value(value));
                        self.tasks.push(Task::Token(b":"));
                        self.tasks.push(Task::Name(name));
                        if !first {
                            self.tasks.push(Task::Token(b","));
                        }
                        continue;
                    }
                    b"}".to_vec()
                }
                Task::Array(mut values, first) => {
                    if let Some(value) = values.next() {
                        self.tasks.push(Task::Array(values, false));
                        self.tasks.push(Task::Value(value));
                        if !first {
                            self.tasks.push(Task::Token(b","));
                        }
                        continue;
                    }
                    b"]".to_vec()
                }
                Task::Token(token) => token.to_vec(),
                Task::Name(name) => serde_json::to_vec(name)
                    .map_err(|_| invalid("Invalid checkpoint field name"))?,
            };
            return Ok(Some(Chunk::Token(Cursor::new(token))));
        }
        Ok(None)
    }
}

impl Read for Reader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if self.current.is_none() {
                self.current = self.next_chunk()?;
            }
            let Some(chunk) = self.current.as_mut() else {
                return Ok(0);
            };
            let allowed = output
                .len()
                .min(self.limit.saturating_sub(self.read).saturating_add(1));
            let count = match chunk {
                Chunk::Token(token) => token.read(&mut output[..allowed])?,
                Chunk::Part(part) => {
                    let count = part.decoder.read(&mut output[..allowed])?;
                    part.mac.update(&output[..count]);
                    count
                }
            };
            if count > self.limit.saturating_sub(self.read) {
                return Err(invalid("Checkpoint exceeds size limit"));
            }
            self.read += count;
            if count > 0 {
                return Ok(count);
            }
            if let Some(Chunk::Part(part)) = self.current.take() {
                let Part { mac, digest, .. } = *part;
                mac.verify_slice(&digest)
                    .map_err(|_| invalid("Checkpoint part digest differs"))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{part_digest, split};
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::{collections::BTreeSet, io::Write};

    fn part(
        key: &SessionSecret,
        session: &SessionId,
        digest: &[u8; 32],
        json: &[u8],
        corrupt_gzip: bool,
    ) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(json).unwrap();
        let mut compressed = encoder.finish().unwrap();
        if corrupt_gzip {
            let trailer = compressed.len() - 8;
            compressed[trailer] ^= 1;
        }
        key.encrypt(&compressed, &context(session, digest))
            .unwrap()
            .to_bytes()
            .unwrap()
    }

    #[test]
    fn incremental_reader_preserves_nested_values_and_enforces_the_exact_size_limit() {
        let key = SessionSecret::from_bytes([71; 32]);
        let session = SessionId::new_v7();
        let value = serde_json::json!({"quoted\"name": [0, {"bytes": vec![255_u8; 40_000]}, "snow ☃", null], "empty": {}});
        let json = serde_json::to_string(&value).unwrap();
        let mut bodies = BTreeMap::new();
        let manifest = split(
            &key,
            &session,
            serde_json::from_str(&json).unwrap(),
            &mut bodies,
            &BTreeSet::new(),
        )
        .unwrap();
        let bodies = bodies.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();
        for limit in [json.len() - 1, json.len()] {
            let mut reader = Reader::new(&manifest, &key, &session, &bodies, limit);
            let result =
                serde_json::from_reader::<_, serde_json::Value>(Buffered::new(&mut reader));
            if limit == json.len() {
                assert_eq!(result.unwrap(), value);
                assert_eq!(reader.bytes_read(), json.len());
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn complete_json_is_not_returned_before_final_part_authentication() {
        let key = SessionSecret::from_bytes([72; 32]);
        let session = SessionId::new_v7();
        let digest = part_digest(&key, &session, b"null");
        let manifest = Manifest::Part(digest);
        for (text, corrupt_gzip) in [(b"true".as_slice(), false), (b"null".as_slice(), true)] {
            let bodies = BTreeMap::from([(
                digest.to_vec(),
                part(&key, &session, &digest, text, corrupt_gzip),
            )]);
            assert!(manifest
                .decode_as::<serde_json::Value>(&key, &session, &bodies, "test")
                .is_err());
        }
        let bodies = BTreeMap::from([(
            digest.to_vec(),
            part(&key, &session, &digest, b"null", false),
        )]);
        assert_eq!(
            manifest
                .decode_as::<serde_json::Value>(&key, &session, &bodies, "test")
                .unwrap(),
            serde_json::Value::Null
        );
        let mut tampered = bodies.clone();
        tampered.values_mut().next().unwrap()[0] ^= 1;
        assert!(manifest
            .decode_as::<serde_json::Value>(&key, &session, &tampered, "test")
            .is_err());
        assert!(manifest
            .decode_as::<serde_json::Value>(&key, &session, &BTreeMap::new(), "test")
            .is_err());
        let text = b"null true";
        let digest = part_digest(&key, &session, text);
        let manifest = Manifest::Part(digest);
        let bodies =
            BTreeMap::from([(digest.to_vec(), part(&key, &session, &digest, text, false))]);
        assert!(manifest
            .decode_as::<serde_json::Value>(&key, &session, &bodies, "test")
            .is_err());
    }

    #[test]
    #[ignore = "isolated recovery decoder comparison; CHECKPOINT_BENCH_BUFFERED=1 selects old decoding"]
    fn recovery_decode_memory_benchmark() {
        let key = SessionSecret::from_bytes([73; 32]);
        let session = SessionId::new_v7();
        let json = serde_json::to_string(&vec!["x".repeat(256 * 1024); 32]).unwrap();
        let mut bodies = BTreeMap::new();
        let manifest = split(
            &key,
            &session,
            serde_json::from_str(&json).unwrap(),
            &mut bodies,
            &BTreeSet::new(),
        )
        .unwrap();
        let bytes = json.len();
        drop(json);
        let bodies = bodies.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();
        let buffered = std::env::var_os("CHECKPOINT_BENCH_BUFFERED").is_some();
        let started = std::time::Instant::now();
        for _ in 0..3 {
            let result: Vec<String> = if buffered {
                let mut json = Zeroizing::new(Vec::new());
                manifest.append(&key, &session, &bodies, &mut json).unwrap();
                serde_json::from_slice(&json).unwrap()
            } else {
                manifest
                    .decode_as(&key, &session, &bodies, "benchmark")
                    .unwrap()
            };
            std::hint::black_box(result);
        }
        println!(
            "checkpoint_bytes={bytes} reads=3 buffered={buffered} elapsed_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        #[cfg(target_os = "linux")]
        for line in std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
        {
            if ["VmRSS:", "VmHWM:", "VmSwap:"]
                .iter()
                .any(|field| line.starts_with(field))
            {
                println!("{line}");
            }
        }
    }
}
