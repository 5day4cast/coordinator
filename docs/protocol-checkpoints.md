# Confidential protocol checkpoints

The coordinator saves a durable checkpoint before sending a confidential command. Each checkpoint retains the complete signing journal and payment evidence.

## Storage formats

| Format | Stored state | Writer |
| --- | --- | --- |
| 1 | One encrypted JSON document, encoded as hex | Default |
| 2 | An encrypted manifest and compressed, encrypted binary parts | `COORDINATOR_PROTOCOL_PARTS=1` or `true` |
| 3 | An encrypted root, compressed, encrypted manifest nodes, and the parts of format 2 | `COORDINATOR_PROTOCOL_PARTS=tree` |

Leaving `COORDINATOR_PROTOCOL_PARTS` unset, or setting it to `0` or `false`, selects format 1. The coordinator reads the variable once at startup and refuses to start with any other value.

The reader supports every format. The additive migration leaves existing rows in format 1. Creating a session also writes format 1.

With format 2 enabled, the next checkpoint converts that session. Application fields and journal entries each have manifest nodes. Larger nested objects and structured arrays split further at field or element boundaries.

Scalar arrays remain one part, including serialized byte vectors. Each part uses a session-bound HMAC-SHA256 content address. Compression precedes authenticated encryption.

A transaction updates the version and manifest with compare-and-swap, inserts new parts, and removes unreferenced parts. Existing parts retain their ciphertext.

Concurrent readers load the manifest and parts from one database snapshot. Missing parts, failed authentication, and conflicting checkpoint versions fail closed.

The implementation limits reconstructed state to 512 MiB, including cached fields and JSON delimiters.

Encoding and decoding run on Tokio's blocking pool, not on async workers; database I/O stays async. A write moves the application state to the encoder and gets it back. A journal save passes a clone that shares the journal's entries instead of copying them. The partitioned writer retains the non-journal manifest between journal saves, without copying the complete state or journal. Each journal command and signing batch serializes separately, so no write buffer contains the complete journal.

The encoder holds the writer's lock. A write cancelled during encoding keeps the lock until the encoding ends, then releases the writer unchanged.

The SDK gives each unchanged journal entry an opaque runtime identity. The writer reuses its authenticated manifest and length without serializing or hashing that entry again. New or changed entries still receive content addresses. Reused nodes must reference parts stored at the writer's current version. The cache advances only after the transaction commits. A failed write retains the prior cache and version.

Application-state updates rebuild the non-journal manifest. Deserialization gives every journal entry a fresh identity. A load therefore records each entry's manifest node under the identity it gave that entry, and sizes the node from the authenticated plaintext lengths of its parts. A writer resumed from that load reuses an entry while the entry keeps that identity, without serializing it again. A collection that a manifest stores as one part seeds none of its entries.

A new writer also reuses the parts its load authenticated: loading decrypts every part the manifest references and checks its content address. The writer's first compare-and-swap names the loaded version, and each committed write keeps exactly the parts its manifest references. While that swap can succeed, the loaded parts are therefore still stored. Resuming serializes and hashes the non-journal fields again, but does not compress, encrypt, or insert them. A conflicting write makes the swap fail; the operation must reload. Reloads still decode the complete state, and individual large entries still need temporary buffers.

New base parts wait in memory until a write commits them. Each write shares them with the writer instead of copying them, and a failed write keeps them for the retry.

These changes retain format 2 compatibility. Older format-2 readers can reconstruct the new manifests. The default format-1 writer retains its existing allocation behavior.

`coordinator_checkpoint_plaintext_bytes` reports bytes actually serialized during each encode. Cached application fields and unchanged journal entries are omitted from subsequent journal-save samples. Decode samples still report the complete reconstructed document. `coordinator_checkpoint_encode_buffer_bytes` reports the largest plaintext buffer capacity during each partitioned write. Neither metric measures the total process peak.

## Format 3: the manifest in nodes

A format-2 manifest names every part of the state, so its size follows the whole state: about 120 bytes per part, doubled by the hex encoding. A session with thousands of parts rewrites close to a megabyte at every checkpoint, even when the step changed one journal entry. During a refund, which checkpoints before every enclave command, that is hundreds of megabytes a minute of database writes.

Format 3 keeps the same parts and keeps the manifest itself in content-addressed nodes:

- An object with more than 64 fields is split into chunks in key order. A field name ends a chunk when the low 6 bits of its session-keyed hash are zero, and a chunk ends at 256 fields regardless. Adding, changing or removing a journal entry therefore changes the chunk that holds it, not its neighbours.
- An array with more than 64 elements is split every 64 elements.
- A list of more than 64 chunks is grouped the same way, level by level.
- Any other object or array whose packed text exceeds 1 KiB is stored as a node, and its parent names it by address.
- The row seals a root that holds the state's top-level fields. It is a few hundred bytes to a few KiB.

A node is stored in `keymeld_protocol_parts` beside the parts. Its text is compressed, then sealed under the address `HMAC-SHA256(session key, "coordinator-protocol-node-v1/" ‖ session ‖ text)`, a domain apart from part addresses. The authenticated root names node addresses, and each node names the addresses below it. A load fetches the nodes level by level in the row's read transaction, decrypts each one, checks its address, and only then reads the parts the rebuilt manifest names. A missing, altered or swapped node, a chunk out of key order, or nesting past 128 levels fails the load. A metadata read projects the root first and fetches no journal node.

Packing is deterministic, so an unchanged subtree keeps its nodes and their addresses. A write seals and inserts only the nodes on the paths to what changed, and the root. A resumed writer reuses the nodes its load authenticated, as it reuses parts.

Each write deletes the parts and nodes that the replaced version stored and the new one does not reference. A writer's first write after loading reads the table to find them, as every write did before. Later writes use what the writer's own previous write stored: the compare-and-swap proves that no other write came between. Format-1 and format-2 writers follow the same rules, so the table never holds an unreferenced part after a commit.

`coordinator_checkpoint_written_bytes_total{kind="row|part|node"}` counts the bytes committed writes store: the sealed row (the whole document in format 1, the manifest in format 2, the root in format 3), new parts, and new nodes.

## Deployment

1. Deploy a release that reads every format. Leave `COORDINATOR_PROTOCOL_PARTS` unset.
2. Verify that every running process supports both formats.
3. Retain a tested rollback release that also reads both formats.
4. Verify a recent backup restore before changing the writer.
5. Set `COORDINATOR_PROTOCOL_PARTS=1` and restart through the normal deployment process.
6. Monitor checkpoint errors, database size, write-ahead log size, memory, and payout progress.

The prior reader cannot read format 2. Do not run it beside an enabled format-2 writer.

Format 3 rolls out the same way. Deploy a release that reads it while the writer stays on format 2, keep a rollback release that also reads it, and then set `COORDINATOR_PROTOCOL_PARTS=tree`. Each session converts at its next checkpoint, which stores all of its nodes once. Earlier releases refuse a format-3 row, so do not run them beside a format-3 writer. Setting the variable back to `1` makes the next checkpoint of each session write format 2 again and delete its nodes.

Disabling the flag changes future writes to format 1. Existing format-2 rows remain until a compatible writer saves them again.

An old binary requires conversion of **every** format-2 row, including inactive sessions. There is no bulk conversion command in this release.

The down migration refuses to proceed while any row uses another format. Prefer rollback to a compatible release without reversing migrations.

## Retention and expected savings

Only parts outside the current manifest are deleted. The current journal remains available even after settlement. No signing or payment evidence expires automatically.

On the measured backup, 90 checkpoint rows contained 476,018,100 encrypted bytes. Their mean size was 5,289,090 bytes; the largest was 24,613,478 bytes.

Format 1 rewrites that session's full encrypted document at each changed checkpoint. Format 2 writes the new manifest and changed compressed parts.

Database pages, indexes, and transaction records add write overhead. Unchanged parts do not add new ciphertext writes. Exact savings depend on which fields change.

The regression fixture checks compression, stable ciphertext, binary arrays, round-trip recovery, missing parts, and rejected concurrent writes. Fixture savings are not production measurements.

Existing rows convert only when written. Therefore, deployment alone does not shrink historical data or establish a daily growth rate.

SQLite can reuse freed pages. Do not run a live `VACUUM` during active signing or payouts to reclaim file space.

Terminal-journal retention needs a separate policy that preserves retry and audit evidence. This release does not delete complete terminal checkpoints.
