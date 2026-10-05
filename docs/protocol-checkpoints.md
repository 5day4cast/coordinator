# Confidential protocol checkpoints

The coordinator saves a durable checkpoint before sending a confidential command. Each checkpoint retains the complete signing journal and payment evidence.

## Storage formats

| Format | Stored state | Writer |
| --- | --- | --- |
| 1 | One encrypted JSON document, encoded as hex | Default |
| 2 | An encrypted manifest and compressed, encrypted binary parts | `COORDINATOR_PROTOCOL_PARTS=1` or `true` |

The reader supports both formats. The additive migration leaves existing rows in format 1. Creating a session also writes format 1.

With format 2 enabled, the next checkpoint converts that session. Objects larger than 32 KiB split at field boundaries. Structured arrays split at element boundaries.

Scalar arrays remain one part, including serialized byte vectors. Each part uses a session-bound HMAC-SHA256 content address. Compression precedes authenticated encryption.

A transaction updates the version and manifest with compare-and-swap, inserts new parts, and removes unreferenced parts. Existing parts retain their ciphertext.

Concurrent readers load the manifest and parts from one database snapshot. Missing parts, failed authentication, and conflicting checkpoint versions fail closed.

The implementation limits reconstructed state to 512 MiB. It still serializes the complete state in memory and compresses each candidate part.

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
