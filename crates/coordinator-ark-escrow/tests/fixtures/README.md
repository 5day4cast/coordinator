# Fixtures

These fixtures define what counts as valid.
Each one comes from the Arkade TypeScript SDK, so this crate must produce the same bytes as arkd and SDK clients.

| File | Source | Tests |
| --- | --- | --- |
| `vtxoscript.json` | `arkade-os/ts-sdk` `packages/ts-sdk/test/fixtures/vtxoscript.json` at `60d258f5dc9e1c7cc28440cc3eafce6b65f61667`. The taproot keys were generated with btcd's `AssembleTaprootScriptTree`. | `ts_sdk_tapscript.rs` |
| `vhtlc.json` | The same repository and commit, `test/fixtures/vhtlc.json` | `ts_sdk_vhtlc.rs` |
| `encoding.json` | The same repository and commit, `test/fixtures/encoding.json` | `ts_sdk_address.rs` |
| `escrow.json` | `generate-escrow-vectors.mjs`, run against `@arkade-os/sdk@0.4.74` | `ts_sdk_escrow.rs` |
| `arkd-rules.json` | `arkd-rules/escrow_rules_test.go`, run inside arkd at `f863e484719344edbe4a8d10cf5fe994b123f2c0`. It records arkd's `TapscriptsVtxoScript.Validate` verdict on each escrow in `escrow.json` under five rule sets. | `arkd_rules.rs` |

Copy the upstream files unchanged when you update them.
Regenerate `escrow.json` with the commands at the top of `generate-escrow-vectors.mjs`.
Then regenerate `arkd-rules.json` with the commands at the top of `arkd-rules/escrow_rules_test.go`.

The SDK accepts scripts that arkd rejects, so an SDK vector alone does not prove an escrow is spendable.
For example, the SDK encodes a condition containing `CHECKLOCKTIMEVERIFY`, but arkd refuses to decode it.
`arkd-rules.json` checks the escrows against the server itself.
