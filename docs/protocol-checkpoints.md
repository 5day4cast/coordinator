# Confidential protocol checkpoints

The coordinator saves a durable checkpoint before sending a confidential command. Each checkpoint retains the complete signing journal and payment evidence.

## Storage formats

| Format | Stored state | Writer |
| --- | --- | --- |
| 1 | One encrypted JSON document, encoded as hex | Default |
| 2 | An encrypted manifest and compressed, encrypted binary parts | `COORDINATOR_PROTOCOL_PARTS=1` or `true` |

Leaving `COORDINATOR_PROTOCOL_PARTS` unset, or setting it to `0` or `false`, selects format 1. The coordinator reads the variable once at startup and refuses to start with any other value.

The reader supports both formats. The additive migration leaves existing rows in format 1. Creating a session also writes format 1.

With format 2 enabled, the next checkpoint converts that session. Application fields and journal entries each have manifest nodes. Larger nested objects and structured arrays split further at field or element boundaries.

Scalar arrays remain one part, including serialized byte vectors. Each part uses a session-bound HMAC-SHA256 content address. Compression precedes authenticated encryption.

A transaction updates the version and manifest with compare-and-swap, inserts new parts, and removes unreferenced parts. Existing parts retain their ciphertext.

Concurrent readers load the manifest and parts from one database snapshot. Missing parts, failed authentication, and conflicting checkpoint versions fail closed.

The implementation limits reconstructed state to 512 MiB, including cached fields and JSON delimiters.

The partitioned writer borrows the application state. It retains the non-journal manifest between journal saves, without cloning the complete state or journal. Each journal command and signing batch serializes separately, so no write buffer contains the complete journal.

The SDK gives each unchanged journal entry an opaque runtime identity. The writer reuses its authenticated manifest and length without serializing or hashing that entry again. New or changed entries still receive content addresses. Reused nodes must reference parts stored at the writer's current version. The cache advances only after the transaction commits. A failed write retains the prior cache and version.

Application-state updates rebuild the non-journal manifest. Deserialization gives every journal entry a fresh identity, so a new writer's entry cache starts empty. A new writer does reuse the parts its load authenticated: loading decrypts every part the manifest references and checks its content address. The writer's first compare-and-swap names the loaded version, and each committed write keeps exactly the parts its manifest references. While that swap can succeed, the loaded parts are therefore still stored. Unchanged fields and entries are serialized and hashed again, but not compressed, encrypted, or inserted. A conflicting write makes the swap fail; the operation must reload. Reloads still decode the complete state, and individual large entries still need temporary buffers.

New base parts wait in memory until a write commits them. Each write shares them with the writer instead of copying them, and a failed write keeps them for the retry.

These changes retain format 2 compatibility. Older format-2 readers can reconstruct the new manifests. The default format-1 writer retains its existing allocation behavior.

`coordinator_checkpoint_plaintext_bytes` reports bytes actually serialized during each encode. Cached application fields and unchanged journal entries are omitted from subsequent journal-save samples. Decode samples still report the complete reconstructed document. `coordinator_checkpoint_encode_buffer_bytes` reports the largest plaintext buffer capacity during each partitioned write. Neither metric measures the total process peak.

## Deployment

1. Deploy a release that reads both formats. Leave `COORDINATOR_PROTOCOL_PARTS` unset.
2. Verify that every running process supports both formats.
3. Retain a tested rollback release that also reads both formats.
4. Verify a recent backup restore before changing the writer.
5. Set `COORDINATOR_PROTOCOL_PARTS=1` and restart through the normal deployment process.
6. Monitor checkpoint errors, database size, write-ahead log size, memory, and payout progress.

The prior reader cannot read format 2. Do not run it beside an enabled format-2 writer.

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
