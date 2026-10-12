use super::*;

/// Deterministic xorshift numbers for generated manifests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn digest(&mut self) -> [u8; 32] {
        let mut digest = [0; 32];
        for chunk in digest.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_le_bytes());
        }
        digest
    }
}

/// A random manifest of objects and arrays with up to `width` children, `depth` levels deep.
fn generated(rng: &mut Rng, depth: u32, width: u64) -> Manifest {
    let kind = if depth == 0 { 0 } else { rng.below(3) };
    match kind {
        0 => Manifest::Part(rng.digest()),
        1 => {
            let count = rng.below(width + 1);
            let mut fields = BTreeMap::new();
            for _ in 0..count {
                let name = format!("field \"{:x}\"", rng.next());
                let value = generated(rng, depth - 1, width);
                fields.insert(name, value);
            }
            Manifest::Object(fields)
        }
        _ => {
            let count = rng.below(width + 1);
            let mut values = Vec::new();
            for _ in 0..count {
                values.push(generated(rng, depth - 1, width));
            }
            Manifest::Array(values)
        }
    }
}

/// An object of `count` fields, each one part, named as journal commands are.
fn commands(rng: &mut Rng, count: usize) -> BTreeMap<String, Manifest> {
    (0..count)
        .map(|index| {
            (
                format!("admit/{:016x}/{}", rng.next(), index % 3 + 1),
                Manifest::Part(rng.digest()),
            )
        })
        .collect()
}

/// A state-shaped manifest: a wide journal command map, a wide outcome map, and small fields.
fn state_manifest(rng: &mut Rng, command_count: usize, outcomes: usize) -> Manifest {
    let outcome_ids = (0..outcomes)
        .map(|index| (index.to_string(), Manifest::Part(rng.digest())))
        .collect();
    let journal = BTreeMap::from([
        (
            "aborted_signing_sessions".to_string(),
            Manifest::Part(rng.digest()),
        ),
        (
            "commands".to_string(),
            Manifest::Object(commands(rng, command_count)),
        ),
        ("opaque_route_id".to_string(), Manifest::Part(rng.digest())),
        ("signing_batches".to_string(), Manifest::Part(rng.digest())),
    ]);
    let session = BTreeMap::from([
        ("session_id".to_string(), Manifest::Part(rng.digest())),
        (
            "outcome_subset_ids".to_string(),
            Manifest::Object(outcome_ids),
        ),
    ]);
    Manifest::Object(BTreeMap::from([
        ("schema_version".to_string(), Manifest::Part(rng.digest())),
        ("session".to_string(), Manifest::Object(session)),
        ("journal".to_string(), Manifest::Object(journal)),
        ("settlements".to_string(), Manifest::Part(rng.digest())),
    ]))
}

/// A fixed session, so cuts and addresses, and with them every bound below, are the same at
/// every run.
fn session() -> SessionId {
    SessionId::parse("01a12338-6b2e-7d3c-9f10-1c2b3a4d5e6f").unwrap()
}

fn json(manifest: &Manifest) -> Vec<u8> {
    serde_json::to_vec(manifest).unwrap()
}

fn stored(packed: &Packed, into: &mut BTreeMap<Vec<u8>, Vec<u8>>) {
    for (digest, body) in &packed.bodies {
        into.insert(digest.to_vec(), body.clone());
    }
}

/// Read a root back as a load does: fetch the nodes it names, round by round, then unpack.
fn read_back(
    key: &SessionSecret,
    session: &SessionId,
    root: &Tree,
    bodies: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<(Manifest, Nodes), KeymeldError> {
    let mut nodes = Nodes::default();
    let mut wanted = root.wanted();
    let mut rounds = 0;
    while !wanted.is_empty() {
        rounds += 1;
        assert!(rounds <= MAX_DEPTH, "a written tree is never this deep");
        wanted = nodes.authenticate(key, session, &wanted, bodies)?;
    }
    Ok((nodes.unpack(root)?, nodes))
}

const SHAPES: [Shape; 6] = [
    Shape {
        fanout_bits: 1,
        node_bytes: 0,
    },
    Shape {
        fanout_bits: 1,
        node_bytes: 96,
    },
    Shape {
        fanout_bits: 2,
        node_bytes: 256,
    },
    Shape {
        fanout_bits: 3,
        node_bytes: 1024,
    },
    Shape::WRITER,
    Shape {
        fanout_bits: 6,
        node_bytes: usize::MAX,
    },
];

#[test]
fn generated_manifests_unpack_exactly_in_every_shape() {
    let key = SessionSecret::from_bytes([81; 32]);
    let session = session();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut manifests: Vec<Manifest> = (0..40)
        .map(|_| Manifest::Object(BTreeMap::from([("root".into(), generated(&mut rng, 4, 9))])))
        .collect();
    // Wide enough for several list levels at every fanout, and empty containers.
    manifests.push(state_manifest(&mut rng, 2000, 600));
    manifests.push(Manifest::Object(BTreeMap::from([
        (
            "items".to_string(),
            Manifest::Array((0..3000).map(|_| Manifest::Part(rng.digest())).collect()),
        ),
        (
            "empty object".to_string(),
            Manifest::Object(BTreeMap::new()),
        ),
        ("empty array".to_string(), Manifest::Array(Vec::new())),
    ])));
    manifests.push(Manifest::Part(rng.digest()));
    manifests.push(Manifest::Array(
        (0..300).map(|_| Manifest::Part(rng.digest())).collect(),
    ));
    for shape in SHAPES {
        for manifest in &manifests {
            let packed = pack(&key, &session, manifest, &BTreeSet::new(), shape).unwrap();
            assert_eq!(
                packed.nodes,
                packed.bodies.keys().copied().collect::<BTreeSet<_>>(),
                "a pack that knows no node seals every node it reaches"
            );
            let mut bodies = BTreeMap::new();
            stored(&packed, &mut bodies);
            let (unpacked, nodes) = read_back(&key, &session, &packed.root, &bodies).unwrap();
            assert!(json(&unpacked) == json(manifest), "{shape:?}");
            assert_eq!(nodes.digests().collect::<BTreeSet<_>>(), packed.nodes);
            let (mut expected, mut found) = (BTreeSet::new(), BTreeSet::new());
            manifest.digests(&mut expected);
            unpacked.digests(&mut found);
            assert_eq!(found, expected);
        }
    }
}

#[test]
fn wide_containers_become_chunk_lists_and_long_ones_nodes() {
    let key = SessionSecret::from_bytes([82; 32]);
    let session = session();
    let mut rng = Rng(7);
    let manifest = state_manifest(&mut rng, 4096, 1000);
    let packed = pack(&key, &session, &manifest, &BTreeSet::new(), Shape::WRITER).unwrap();
    let Tree::Object(root) = &packed.root else {
        panic!("the root keeps the state's fields");
    };
    assert_eq!(
        root.keys().collect::<Vec<_>>(),
        ["journal", "schema_version", "session", "settlements"]
    );
    let root_len = serde_json::to_vec(&packed.root).unwrap().len();
    assert!(
        root_len < 2048,
        "the root names nodes, not parts: {root_len}"
    );
    // 4,096 commands make about 64 chunks; with the outcomes and the lists above them, a few
    // more. Never one node per part.
    assert!(
        (50..300).contains(&packed.nodes.len()),
        "{} nodes",
        packed.nodes.len()
    );
    let largest = packed.bodies.values().map(Vec::len).max().unwrap();
    assert!(largest < 32 * 1024, "the largest node is {largest} bytes");
}

#[test]
fn packing_is_deterministic_and_known_nodes_are_not_sealed_again() {
    let key = SessionSecret::from_bytes([83; 32]);
    let session = session();
    let mut rng = Rng(11);
    let manifest = state_manifest(&mut rng, 700, 300);
    for shape in SHAPES {
        let first = pack(&key, &session, &manifest, &BTreeSet::new(), shape).unwrap();
        let again = pack(&key, &session, &manifest, &first.nodes, shape).unwrap();
        assert!(again.bodies.is_empty(), "{shape:?}");
        assert_eq!(again.nodes, first.nodes);
        assert_eq!(
            serde_json::to_vec(&again.root).unwrap(),
            serde_json::to_vec(&first.root).unwrap()
        );
    }
    // Addresses and cuts are bound to the session's key: another key packs other nodes.
    let first = pack(&key, &session, &manifest, &BTreeSet::new(), Shape::WRITER).unwrap();
    let other = pack(
        &SessionSecret::from_bytes([84; 32]),
        &session,
        &manifest,
        &BTreeSet::new(),
        Shape::WRITER,
    )
    .unwrap();
    assert!(first.nodes.is_disjoint(&other.nodes));
}

/// What one change costs: the nodes a pack seals that the previous one did not store, the
/// root, and the nodes the previous version referenced that this one drops.
struct Change {
    sealed: usize,
    sealed_bytes: usize,
    dropped: usize,
    root_bytes: usize,
}

fn change(
    key: &SessionSecret,
    session: &SessionId,
    previous: &Packed,
    manifest: &Manifest,
    shape: Shape,
) -> (Packed, Change) {
    let packed = pack(key, session, manifest, &previous.nodes, shape).unwrap();
    let change = Change {
        sealed: packed.bodies.len(),
        sealed_bytes: packed.bodies.values().map(Vec::len).sum(),
        dropped: previous.nodes.difference(&packed.nodes).count(),
        root_bytes: serde_json::to_vec(&packed.root).unwrap().len(),
    };
    (packed, change)
}

fn commands_of(manifest: &mut Manifest) -> &mut BTreeMap<String, Manifest> {
    let Manifest::Object(root) = manifest else {
        unreachable!()
    };
    let Some(Manifest::Object(journal)) = root.get_mut("journal") else {
        unreachable!()
    };
    let Some(Manifest::Object(commands)) = journal.get_mut("commands") else {
        unreachable!()
    };
    commands
}

#[test]
fn a_changed_journal_command_rewrites_one_path_of_nodes() {
    let key = SessionSecret::from_bytes([85; 32]);
    let session = session();
    let mut rng = Rng(13);
    let mut manifest = state_manifest(&mut rng, 4096, 1000);
    let format_2 = key
        .encrypt(&json(&manifest), "format-2 manifest")
        .unwrap()
        .to_hex()
        .unwrap()
        .len();
    let mut previous = pack(&key, &session, &manifest, &BTreeSet::new(), Shape::WRITER).unwrap();
    let mut worst = 0;
    for step in 0..60 {
        let commands = commands_of(&mut manifest);
        match step % 3 {
            // A new command lands anywhere in key order.
            0 => {
                commands.insert(
                    format!("admit/{:016x}/1", rng.next()),
                    Manifest::Part(rng.digest()),
                );
            }
            // An outcome changes a command.
            1 => {
                let index = rng.below(commands.len() as u64) as usize;
                let name = commands.keys().nth(index).unwrap().clone();
                commands.insert(name, Manifest::Part(rng.digest()));
            }
            // A rejected command is cleared.
            _ => {
                let index = rng.below(commands.len() as u64) as usize;
                let name = commands.keys().nth(index).unwrap().clone();
                commands.remove(&name);
            }
        }
        let (packed, cost) = change(&key, &session, &previous, &manifest, Shape::WRITER);
        // A chunk (two or three when the change moves a cut next to a full chunk), and the
        // lists above it.
        assert!(
            (1..=6).contains(&cost.sealed),
            "step {step}: {} nodes",
            cost.sealed
        );
        assert!(cost.dropped <= 6, "step {step}: {} dropped", cost.dropped);
        assert!(cost.root_bytes < 2048);
        worst = worst.max(cost.sealed_bytes + cost.root_bytes * 2);
        previous = packed;
    }
    // A full chunk holds 256 names; a typical one 64.
    assert!(worst < 64 * 1024, "a change stored {worst} bytes");
    assert!(
        format_2 > 20 * worst,
        "format 2 seals {format_2} bytes per write, format 3 at most {worst}"
    );
}

#[test]
fn appending_to_a_wide_array_rewrites_only_its_last_chunk_path() {
    let key = SessionSecret::from_bytes([86; 32]);
    let session = session();
    let mut rng = Rng(17);
    let mut values: Vec<Manifest> = (0..9000).map(|_| Manifest::Part(rng.digest())).collect();
    let wrap = |values: &Vec<Manifest>| {
        Manifest::Object(BTreeMap::from([(
            "items".to_string(),
            Manifest::Array(values.clone()),
        )]))
    };
    let shape = Shape {
        fanout_bits: 3,
        node_bytes: 512,
    };
    let mut previous = pack(&key, &session, &wrap(&values), &BTreeSet::new(), shape).unwrap();
    for _ in 0..20 {
        values.push(Manifest::Part(rng.digest()));
        let (packed, cost) = change(&key, &session, &previous, &wrap(&values), shape);
        // 9,000 elements at eight to a chunk: the chunk and three list levels above it, and one
        // more of each where the new element starts a chunk or a group.
        assert!(cost.sealed <= 6, "{} nodes", cost.sealed);
        assert!(cost.dropped <= 6);
        previous = packed;
    }
}

fn nodes(trees: Vec<([u8; 32], Tree)>) -> Nodes {
    Nodes {
        trees: trees.into_iter().collect(),
        bytes: 0,
    }
}

/// An object chunk whose fields are each one part.
fn object(names: &[&str]) -> Tree {
    Tree::Object(
        names
            .iter()
            .map(|name| (name.to_string(), Tree::Part(Digest([9; 32]))))
            .collect(),
    )
}

#[test]
fn unpacking_refuses_trees_no_writer_makes() {
    let (a, b, c, part): ([u8; 32], [u8; 32], [u8; 32], _) =
        ([1; 32], [2; 32], [3; 32], Digest([9; 32]));
    let refused = |trees: Vec<([u8; 32], Tree)>, root: Tree| nodes(trees).unpack(&root).is_err();
    let accepted = nodes(vec![(a, object(&["a", "c"])), (b, object(&["d"]))])
        .unpack(&Tree::Keys(vec![Digest(a), Digest(b)]))
        .unwrap();
    assert!(matches!(&accepted, Manifest::Object(fields) if fields.len() == 3));

    // Chunks out of order, or naming a field twice.
    assert!(refused(
        vec![(a, object(&["a", "c"])), (b, object(&["b"]))],
        Tree::Keys(vec![Digest(a), Digest(b)])
    ));
    assert!(refused(
        vec![(a, object(&["a"])), (b, object(&["a"]))],
        Tree::Keys(vec![Digest(a), Digest(b)])
    ));
    // Empty lists and empty chunks.
    assert!(refused(vec![], Tree::Keys(vec![])));
    assert!(refused(vec![], Tree::Items(vec![])));
    assert!(refused(
        vec![(a, Tree::Object(BTreeMap::new()))],
        Tree::Keys(vec![Digest(a)])
    ));
    assert!(refused(
        vec![(a, Tree::Array(vec![]))],
        Tree::Items(vec![Digest(a)])
    ));
    // A chunk of the other kind.
    assert!(refused(
        vec![(a, Tree::Array(vec![Tree::Part(part)]))],
        Tree::Keys(vec![Digest(a)])
    ));
    assert!(refused(
        vec![(a, object(&["a"]))],
        Tree::Items(vec![Digest(a)])
    ));
    // A node that holds only a reference.
    assert!(refused(vec![(a, Tree::Part(part))], Tree::Node(Digest(a))));
    assert!(refused(
        vec![(a, Tree::Node(Digest(b))), (b, object(&["a"]))],
        Tree::Node(Digest(a))
    ));
    // A node no load fetched.
    assert!(refused(vec![(a, object(&["a"]))], Tree::Node(Digest(c))));
    // Nesting past the limit, in a chain that is otherwise complete.
    let links = MAX_DEPTH as u32;
    let chain: Vec<_> = (0..=links)
        .map(|index| {
            let mut own = [0; 32];
            own[..4].copy_from_slice(&index.to_le_bytes());
            let mut next = [0; 32];
            next[..4].copy_from_slice(&(index + 1).to_le_bytes());
            let inner = if index == links {
                BTreeMap::new()
            } else {
                BTreeMap::from([("x".to_string(), Tree::Node(Digest(next)))])
            };
            (own, Tree::Object(inner))
        })
        .collect();
    assert!(refused(chain, Tree::Node(Digest([0; 32]))));
    // A shorter chain of the same kind unpacks.
    let short: Vec<_> = (0..4u32)
        .map(|index| {
            let mut own = [0; 32];
            own[..4].copy_from_slice(&index.to_le_bytes());
            let mut next = [0; 32];
            next[..4].copy_from_slice(&(index + 1).to_le_bytes());
            let inner = if index == 3 {
                BTreeMap::new()
            } else {
                BTreeMap::from([("x".to_string(), Tree::Node(Digest(next)))])
            };
            (own, Tree::Object(inner))
        })
        .collect();
    assert!(!refused(short, Tree::Node(Digest([0; 32]))));
}

#[test]
fn nodes_open_only_under_their_own_address_key_and_session() {
    let key = SessionSecret::from_bytes([87; 32]);
    let session = session();
    let mut rng = Rng(19);
    let manifest = state_manifest(&mut rng, 300, 100);
    let shape = Shape {
        fanout_bits: 2,
        node_bytes: 128,
    };
    let packed = pack(&key, &session, &manifest, &BTreeSet::new(), shape).unwrap();
    let mut bodies = BTreeMap::new();
    stored(&packed, &mut bodies);
    assert!(read_back(&key, &session, &packed.root, &bodies).is_ok());
    let wanted = packed.root.wanted();
    let first = *wanted.iter().next().unwrap();
    let opens = |key: &SessionSecret, session: &SessionId, bodies: &BTreeMap<Vec<u8>, Vec<u8>>| {
        Nodes::default()
            .authenticate(key, session, &wanted, bodies)
            .is_ok()
    };
    assert!(opens(&key, &session, &bodies));
    assert!(!opens(
        &SessionSecret::from_bytes([88; 32]),
        &session,
        &bodies
    ));
    assert!(!opens(&key, &SessionId::new_v7(), &bodies));
    // Missing.
    let mut missing = bodies.clone();
    missing.remove(first.as_slice());
    assert!(!opens(&key, &session, &missing));
    // Altered.
    let mut altered = bodies.clone();
    let body = altered.get_mut(first.as_slice()).unwrap();
    let last = body.len() - 1;
    body[last] ^= 1;
    assert!(!opens(&key, &session, &altered));
    // Another node's body under this address.
    let other = packed
        .nodes
        .iter()
        .find(|digest| **digest != first)
        .unwrap();
    let mut swapped = bodies.clone();
    swapped.insert(first.to_vec(), bodies[other.as_slice()].clone());
    assert!(!opens(&key, &session, &swapped));
    // A part sealed the way parts are, under this node's address.
    let mut part = bodies.clone();
    part.insert(
        first.to_vec(),
        seal_compressed(&key, &context(&session, &first), &bodies[first.as_slice()]).unwrap(),
    );
    assert!(!opens(&key, &session, &part));
    // Text that is not what the address names, sealed under it.
    let mut forged = bodies.clone();
    forged.insert(
        first.to_vec(),
        seal_compressed(&key, &node_context(&session, &first), b"{\"Object\":{}}").unwrap(),
    );
    assert!(!opens(&key, &session, &forged));
    // The reconstructed size limit counts node text.
    let mut full = Nodes {
        trees: BTreeMap::new(),
        bytes: MAX_STATE_BYTES - 8,
    };
    assert!(full.authenticate(&key, &session, &wanted, &bodies).is_err());
}

#[test]
fn digests_are_written_as_hex() {
    let digest = Digest([0xab; 32]);
    let text = serde_json::to_string(&Tree::Part(digest)).unwrap();
    assert_eq!(text, format!("{{\"Part\":\"{}\"}}", "ab".repeat(32)));
    assert!(matches!(
        serde_json::from_str::<Tree>(&text).unwrap(),
        Tree::Part(read) if read == digest
    ));
    for bad in ["ab".to_string(), "ab".repeat(33), "zz".repeat(32)] {
        assert!(serde_json::from_str::<Tree>(&format!("{{\"Part\":\"{bad}\"}}")).is_err());
    }
}

#[test]
fn a_shape_without_a_fanout_is_refused() {
    let key = SessionSecret::from_bytes([89; 32]);
    let session = session();
    let shape = Shape {
        fanout_bits: 0,
        node_bytes: 0,
    };
    let manifest = Manifest::Object(BTreeMap::new());
    assert!(pack(&key, &session, &manifest, &BTreeSet::new(), shape).is_err());
}
