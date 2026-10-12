//! Format 3 keeps the manifest itself in content-addressed nodes. A write then stores a small
//! sealed root and the nodes on the paths to what changed, instead of a manifest that names
//! every part of the state, as a format-2 write does.
//!
//! An object with more fields than a chunk holds is split into chunks in key order, each
//! stored as a node. Field names place the cuts: a name whose keyed hash ends in enough zero
//! bits ends a chunk, so adding, changing or removing a field changes only its own chunk. An
//! array is cut every `fanout` elements. A list of chunks longer than a chunk is split again
//! the same way. Any other object or array whose packed text is longer than `node_bytes` is
//! stored as a node, so a parent names it with one address.
//!
//! A node is stored like a part, in the same table: its text is compressed, then sealed under
//! a session-bound HMAC address in a domain of its own. The row seals the root at its version.
//! The root names node addresses, and every node names the addresses below it, so a load
//! accepts only the tree a writer committed: a missing, altered or swapped node fails it.
use super::*;
use flate2::read::GzDecoder;
use keymeld_core::crypto::EncryptedData;
use std::{collections::btree_map, io::Read};

/// Nodes and nested values below a root, at most. A writer's trees stay far shallower.
pub(in crate::infra::keymeld) const MAX_DEPTH: usize = 128;

/// A content address, written as hex in roots and nodes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::infra::keymeld) struct Digest(pub [u8; 32]);

impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut digest = [0; 32];
        hex::decode_to_slice(text, &mut digest).map_err(serde::de::Error::custom)?;
        Ok(Self(digest))
    }
}

/// A format-3 manifest: a [`Manifest`] whose subtrees and chunks may be kept in nodes.
#[derive(Clone, Serialize, Deserialize)]
pub(in crate::infra::keymeld) enum Tree {
    /// A part, as in a [`Manifest`].
    Part(Digest),
    Object(BTreeMap<String, Tree>),
    Array(Vec<Tree>),
    /// An object, array or chunk list stored as a node.
    Node(Digest),
    /// One object's fields across nodes, in key order. Each node holds an `Object` or a further
    /// `Keys`, and each name sorts after every name before it.
    Keys(Vec<Digest>),
    /// One array's elements across nodes, in order. Each node holds an `Array` or a further
    /// `Items`.
    Items(Vec<Digest>),
}

impl Tree {
    /// The addresses of the nodes this tree names, without following them.
    fn references(&self, output: &mut BTreeSet<[u8; 32]>) {
        match self {
            Self::Part(_) => {}
            Self::Object(fields) => fields.values().for_each(|value| value.references(output)),
            Self::Array(values) => values.iter().for_each(|value| value.references(output)),
            Self::Node(digest) => {
                output.insert(digest.0);
            }
            Self::Keys(digests) | Self::Items(digests) => {
                output.extend(digests.iter().map(|digest| digest.0))
            }
        }
    }

    /// The nodes a load must fetch first: those the root names.
    pub fn wanted(&self) -> BTreeSet<[u8; 32]> {
        let mut wanted = BTreeSet::new();
        self.references(&mut wanted);
        wanted
    }

    /// As [`Manifest::select_fields`], before any node is fetched.
    pub fn select_fields(&self, fields: &[&str]) -> Self {
        match self {
            Self::Object(values) => Self::Object(
                values
                    .iter()
                    .filter(|(name, _)| fields.contains(&name.as_str()))
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
            ),
            _ => self.clone(),
        }
    }
}

/// How a writer splits a manifest. Readers accept any shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::infra::keymeld) struct Shape {
    /// A chunk holds 2^`fanout_bits` fields or elements on average, and four times that at most.
    pub fanout_bits: u32,
    /// A packed object or array whose text is longer than this is stored as a node.
    pub node_bytes: usize,
}

impl Shape {
    /// 64 fields per chunk: the SDK's limit of 4,096 journal commands fills about 64 chunks, so
    /// a change rewrites one chunk of about 64 names and one list of about 64 addresses.
    pub const WRITER: Self = Self {
        fanout_bits: 6,
        node_bytes: 1024,
    };

    /// The shape writers use. Test builds split even small states into chunks, chunk lists and
    /// nodes, so that every format-3 flow the tests drive stores and reads all of them.
    pub fn writer() -> Self {
        if cfg!(test) {
            Self {
                fanout_bits: 2,
                node_bytes: 256,
            }
        } else {
            Self::WRITER
        }
    }

    fn fanout(self) -> usize {
        1 << self.fanout_bits
    }

    fn max_chunk(self) -> usize {
        4 << self.fanout_bits
    }
}

fn domain_mac(key: &SessionSecret, session: &SessionId, domain: &[u8]) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts a session key");
    mac.update(domain);
    mac.update(session.to_string().as_bytes());
    mac
}

/// Node addresses: a domain apart from part addresses, so a part never answers for a node.
fn node_mac(key: &SessionSecret, session: &SessionId) -> Hmac<Sha256> {
    domain_mac(key, session, b"coordinator-protocol-node-v1/")
}

/// The keyed hash of a field name that decides where an object's chunks end.
fn cut_mac(key: &SessionSecret, session: &SessionId) -> Hmac<Sha256> {
    domain_mac(key, session, b"coordinator-protocol-cut-v1/")
}

fn node_context(session: &SessionId, digest: &[u8; 32]) -> String {
    format!(
        "coordinator-protocol-node-v1/{session}/{}",
        hex::encode(digest)
    )
}

/// The bytes a value's JSON takes, without keeping the text.
fn json_len(tree: &Tree) -> Result<usize, KeymeldError> {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, tree).map_err(|error| failure(error.to_string()))?;
    Ok(count.0)
}

/// A manifest packed for one write.
pub(in crate::infra::keymeld) struct Packed {
    /// The tree the row seals.
    pub root: Tree,
    /// Every node the root reaches.
    pub nodes: BTreeSet<[u8; 32]>,
    /// The sealed text of each node this pack made that `known` did not list.
    pub bodies: BTreeMap<[u8; 32], Vec<u8>>,
}

/// Pack `manifest` into a root and nodes. A node whose address `known` lists is stored already,
/// so it is not sealed again. Packing is deterministic: an unchanged subtree gets the same
/// nodes, and so the same addresses, at every write.
pub(in crate::infra::keymeld) fn pack(
    key: &SessionSecret,
    session: &SessionId,
    manifest: &Manifest,
    known: &BTreeSet<[u8; 32]>,
    shape: Shape,
) -> Result<Packed, KeymeldError> {
    if shape.fanout_bits == 0 || shape.fanout_bits > 16 {
        return Err(failure("Invalid checkpoint tree shape"));
    }
    let mut packer = Packer {
        key,
        session,
        known,
        shape,
        content: node_mac(key, session),
        cuts: cut_mac(key, session),
        nodes: BTreeSet::new(),
        bodies: BTreeMap::new(),
    };
    // The root keeps the state's own fields; the row seals it at its version.
    let root = match manifest {
        Manifest::Object(fields) => Tree::Object(
            fields
                .iter()
                .map(|(name, value)| Ok((name.clone(), packer.pack(value)?)))
                .collect::<Result<_, KeymeldError>>()?,
        ),
        other => packer.pack(other)?,
    };
    Ok(Packed {
        root,
        nodes: packer.nodes,
        bodies: packer.bodies,
    })
}

struct Packer<'a> {
    key: &'a SessionSecret,
    session: &'a SessionId,
    known: &'a BTreeSet<[u8; 32]>,
    shape: Shape,
    content: Hmac<Sha256>,
    cuts: Hmac<Sha256>,
    nodes: BTreeSet<[u8; 32]>,
    bodies: BTreeMap<[u8; 32], Vec<u8>>,
}

impl Packer<'_> {
    /// `manifest` as a tree: wide objects and arrays split into chunks, and the result stored as
    /// a node when its text is longer than the shape allows inline.
    fn pack(&mut self, manifest: &Manifest) -> Result<Tree, KeymeldError> {
        let fanout = self.shape.fanout();
        let tree = match manifest {
            Manifest::Part(digest) => return Ok(Tree::Part(Digest(*digest))),
            Manifest::Object(fields) if fields.len() > fanout => self.keys(fields)?,
            Manifest::Object(fields) => Tree::Object(
                fields
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), self.pack(value)?)))
                    .collect::<Result<_, KeymeldError>>()?,
            ),
            Manifest::Array(values) if values.len() > fanout => self.items(values)?,
            Manifest::Array(values) => Tree::Array(
                values
                    .iter()
                    .map(|value| self.pack(value))
                    .collect::<Result<_, _>>()?,
            ),
        };
        if json_len(&tree)? <= self.shape.node_bytes {
            return Ok(tree);
        }
        Ok(Tree::Node(self.store(&tree)?))
    }

    /// A wide object's fields in chunks. A name whose rank is at least one ends a chunk, and so
    /// does a chunk's last allowed field.
    fn keys(&mut self, fields: &BTreeMap<String, Manifest>) -> Result<Tree, KeymeldError> {
        let mut chunks = Vec::new();
        let mut chunk = BTreeMap::new();
        for (name, value) in fields {
            let packed = self.pack(value)?;
            chunk.insert(name.clone(), packed);
            let rank = self.rank(name.as_bytes());
            if rank > 0 || chunk.len() == self.shape.max_chunk() {
                let digest = self.store(&Tree::Object(std::mem::take(&mut chunk)))?;
                chunks.push((digest, rank));
            }
        }
        if !chunk.is_empty() {
            chunks.push((self.store(&Tree::Object(chunk))?, 0));
        }
        Ok(Tree::Keys(self.lists(chunks, Tree::Keys)?))
    }

    /// A wide array's elements, `fanout` to a chunk. Position alone places the cuts, so chunk
    /// `k`'s rank counts how many times the fanout divides `k + 1`, plus one.
    fn items(&mut self, values: &[Manifest]) -> Result<Tree, KeymeldError> {
        let mut chunks = Vec::new();
        for (index, slice) in values.chunks(self.shape.fanout()).enumerate() {
            let packed = slice
                .iter()
                .map(|value| self.pack(value))
                .collect::<Result<_, _>>()?;
            let rank = 1 + (index + 1).trailing_zeros() / self.shape.fanout_bits;
            chunks.push((self.store(&Tree::Array(packed))?, rank));
        }
        Ok(Tree::Items(self.lists(chunks, Tree::Items)?))
    }

    /// Group chunk addresses into list nodes, level by level, until at most `fanout` remain.
    /// At level `level` an address ends a group when its rank reaches the level, so the groups
    /// also follow the content. Ranks are bounded, so from some level on only the size limit
    /// ends groups, and every level then shortens the list.
    fn lists(
        &mut self,
        mut refs: Vec<(Digest, u32)>,
        list: fn(Vec<Digest>) -> Tree,
    ) -> Result<Vec<Digest>, KeymeldError> {
        let mut level = 1;
        while refs.len() > self.shape.fanout() {
            level += 1;
            let mut grouped = Vec::new();
            let mut group = Vec::new();
            for (digest, rank) in refs {
                group.push(digest);
                if rank >= level || group.len() == self.shape.max_chunk() {
                    grouped.push((self.store(&list(std::mem::take(&mut group)))?, rank));
                }
            }
            if !group.is_empty() {
                grouped.push((self.store(&list(group))?, 0));
            }
            refs = grouped;
        }
        Ok(refs.into_iter().map(|(digest, _)| digest).collect())
    }

    /// How many chunk levels a field name ends: the zero low bits of its keyed hash, counted in
    /// whole fanouts.
    fn rank(&self, name: &[u8]) -> u32 {
        let mut mac = self.cuts.clone();
        mac.update(name);
        let hash = mac.finalize().into_bytes();
        let low = u64::from_le_bytes(hash[..8].try_into().expect("eight bytes"));
        low.trailing_zeros() / self.shape.fanout_bits
    }

    /// Store `tree` as a node, unless it is stored already, and return its address.
    fn store(&mut self, tree: &Tree) -> Result<Digest, KeymeldError> {
        let text =
            Zeroizing::new(serde_json::to_vec(tree).map_err(|error| failure(error.to_string()))?);
        let mut mac = self.content.clone();
        mac.update(&text);
        let digest: [u8; 32] = mac.finalize().into_bytes().into();
        self.nodes.insert(digest);
        if self.known.contains(&digest) {
            return Ok(Digest(digest));
        }
        if let btree_map::Entry::Vacant(entry) = self.bodies.entry(digest) {
            entry.insert(seal_compressed(
                self.key,
                &node_context(self.session, &digest),
                &text,
            )?);
        }
        Ok(Digest(digest))
    }
}

/// The nodes a load fetched for one root, and the text they took.
#[derive(Default)]
pub(in crate::infra::keymeld) struct Nodes {
    trees: BTreeMap<[u8; 32], Tree>,
    bytes: usize,
}

impl Nodes {
    /// Authenticate the stored bodies of `wanted`, and return the addresses of the nodes they
    /// name that are not fetched yet. A body that is missing, sealed for another address or
    /// session, or whose text does not match its address fails the load.
    pub fn authenticate(
        &mut self,
        key: &SessionSecret,
        session: &SessionId,
        wanted: &BTreeSet<[u8; 32]>,
        bodies: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<BTreeSet<[u8; 32]>, KeymeldError> {
        let content = node_mac(key, session);
        let mut next = BTreeSet::new();
        for digest in wanted {
            let body = bodies
                .get(digest.as_slice())
                .ok_or_else(|| failure("Confidential checkpoint node is missing"))?;
            let limit = MAX_STATE_BYTES.saturating_sub(self.bytes);
            let (tree, len) = open(key, session, digest, body, &content, limit)?;
            self.bytes += len;
            tree.references(&mut next);
            self.trees.insert(*digest, tree);
        }
        next.retain(|digest| !self.trees.contains_key(digest));
        Ok(next)
    }

    /// Every node fetched.
    pub fn digests(&self) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.trees.keys().copied()
    }

    /// The manifest `root` describes, from the fetched nodes.
    pub fn unpack(&self, root: &Tree) -> Result<Manifest, KeymeldError> {
        self.unpack_at(root, 0)
    }

    fn node(&self, digest: &Digest) -> Result<&Tree, KeymeldError> {
        self.trees
            .get(&digest.0)
            .ok_or_else(|| failure("Confidential checkpoint node is missing"))
    }

    fn unpack_at(&self, tree: &Tree, depth: usize) -> Result<Manifest, KeymeldError> {
        let depth = depth + 1;
        if depth > MAX_DEPTH {
            return Err(failure("Confidential checkpoint nests too deeply"));
        }
        Ok(match tree {
            Tree::Part(digest) => Manifest::Part(digest.0),
            Tree::Object(fields) => Manifest::Object(
                fields
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), self.unpack_at(value, depth)?)))
                    .collect::<Result<_, KeymeldError>>()?,
            ),
            Tree::Array(values) => Manifest::Array(
                values
                    .iter()
                    .map(|value| self.unpack_at(value, depth))
                    .collect::<Result<_, _>>()?,
            ),
            // A writer stores only objects, arrays and chunk lists as nodes.
            Tree::Node(digest) => match self.node(digest)? {
                Tree::Part(_) | Tree::Node(_) => {
                    return Err(failure("Invalid confidential checkpoint node"))
                }
                stored => self.unpack_at(stored, depth)?,
            },
            Tree::Keys(chunks) => {
                let mut fields = BTreeMap::new();
                self.keys(chunks, depth, &mut fields)?;
                Manifest::Object(fields)
            }
            Tree::Items(chunks) => {
                let mut values = Vec::new();
                self.items(chunks, depth, &mut values)?;
                Manifest::Array(values)
            }
        })
    }

    fn keys(
        &self,
        chunks: &[Digest],
        depth: usize,
        fields: &mut BTreeMap<String, Manifest>,
    ) -> Result<(), KeymeldError> {
        if depth > MAX_DEPTH {
            return Err(failure("Confidential checkpoint nests too deeply"));
        }
        if chunks.is_empty() {
            return Err(failure("Invalid confidential checkpoint chunk list"));
        }
        for chunk in chunks {
            match self.node(chunk)? {
                Tree::Object(entries) if !entries.is_empty() => {
                    for (name, value) in entries {
                        if fields
                            .last_key_value()
                            .is_some_and(|(last, _)| last >= name)
                        {
                            return Err(failure("Confidential checkpoint chunks overlap"));
                        }
                        let value = self.unpack_at(value, depth)?;
                        fields.insert(name.clone(), value);
                    }
                }
                Tree::Keys(inner) => self.keys(inner, depth + 1, fields)?,
                _ => return Err(failure("Invalid confidential checkpoint chunk")),
            }
        }
        Ok(())
    }

    fn items(
        &self,
        chunks: &[Digest],
        depth: usize,
        values: &mut Vec<Manifest>,
    ) -> Result<(), KeymeldError> {
        if depth > MAX_DEPTH {
            return Err(failure("Confidential checkpoint nests too deeply"));
        }
        if chunks.is_empty() {
            return Err(failure("Invalid confidential checkpoint chunk list"));
        }
        for chunk in chunks {
            match self.node(chunk)? {
                Tree::Array(elements) if !elements.is_empty() => {
                    for value in elements {
                        values.push(self.unpack_at(value, depth)?);
                    }
                }
                Tree::Items(inner) => self.items(inner, depth + 1, values)?,
                _ => return Err(failure("Invalid confidential checkpoint chunk")),
            }
        }
        Ok(())
    }
}

/// Authenticate and parse the node stored at `digest`; its text may take at most `limit` bytes.
fn open(
    key: &SessionSecret,
    session: &SessionId,
    digest: &[u8; 32],
    body: &[u8],
    content: &Hmac<Sha256>,
    limit: usize,
) -> Result<(Tree, usize), KeymeldError> {
    let encrypted =
        EncryptedData::from_bytes(body).map_err(|_| failure("Invalid checkpoint node encoding"))?;
    let compressed = Zeroizing::new(
        key.decrypt(&encrypted, &node_context(session, digest))
            .map_err(|_| failure("Confidential checkpoint node authentication failed"))?,
    );
    let mut text = Zeroizing::new(Vec::new());
    GzDecoder::new(compressed.as_slice())
        .take(limit as u64 + 1)
        .read_to_end(&mut text)
        .map_err(|_| failure("Invalid checkpoint node compression"))?;
    if text.len() > limit {
        return Err(failure("Confidential checkpoint exceeds size limit"));
    }
    let mut mac = content.clone();
    mac.update(&text);
    mac.verify_slice(digest)
        .map_err(|_| failure("Confidential checkpoint node digest differs"))?;
    let tree = serde_json::from_slice(&text)
        .map_err(|_| failure("Invalid confidential checkpoint node"))?;
    Ok((tree, text.len()))
}

#[cfg(test)]
#[path = "checkpoint_tree_tests.rs"]
mod tests;
