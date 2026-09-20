# Dependency advisory applicability

Reviewed on 2026-09-20 against the current Coordinator and Keymeld workspaces.
The RustSec scan reports one vulnerable package in Coordinator's lockfile.
The resolved workspace graphs do not activate that package.
This distinction does not remove or suppress the advisory.

## RustCrypto RSA

[RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) affects `rsa 0.9.10` through observable private-key timing.
RustSec lists no patched version, including the available release candidates.
A version upgrade alone cannot resolve this advisory.

The audit database commit was `d5c17953a895cf19e8d3ce66eaa42b6fcfe1fb16`, updated on 2026-09-19.
The scan found `rsa 0.9.10` in a lockfile containing 629 packages.
Its advisory ignore list was empty.

| Checked component | Result |
| --- | --- |
| Coordinator lockfile | The only direct parent of `rsa` is `sqlx-mysql 0.8.6`. |
| Keymeld lockfile | The only direct parent of `rsa` is `sqlx-mysql 0.8.6`. |
| Coordinator workspace, all features and targets | Neither `rsa` nor `sqlx-mysql` has an active dependency path. |
| Keymeld workspace, all features and targets | `rsa` has no active dependency path. |
| Coordinator and synth database code | Both use `SqlitePool` and SQLite connection options. |
| Keymeld KMS recipient decryption | Uses the `openssl` crate, not RustCrypto `rsa`. |

SQLx and SQLx macros declare MySQL as an optional dependency.
Their locked metadata includes `sqlx-mysql`, which depends on `rsa`.
The selected SQLx features activate SQLite without activating MySQL.
The default `any` feature forwards settings only to database drivers already enabled.

Source evidence:

- [Workspace SQLx features](../Cargo.toml).
- [Coordinator SQLite pools](../crates/coordinator/src/infra/db.rs).
- [Synth SQLite pool](../crates/synth/src/db/mod.rs).
- SQLx 0.8.6 registry manifests: `sqlx/Cargo.toml`, `sqlx-macros-core/Cargo.toml`, and `sqlx-mysql/Cargo.toml`.
- Keymeld source: `crates/keymeld-enclave/src/operations/kms_recipient.rs` and `nix/openssl.nix`.

The Keymeld KMS path creates a fresh RSA-2048 recipient through OpenSSL for each operation.
It accepts the required RSA-OAEP-SHA256 CMS envelope and reports one generic invalid-ciphertext error.
The Nix overlay selects OpenSSL 3.6.4 or a newer package version.
These facts identify the implementation; they do not establish immunity to every native-library vulnerability.

## Reproduce the graph checks

From the Coordinator workspace, run:

```sh
env RUSTC_WRAPPER= cargo tree --offline --locked --workspace --all-features --target all -i rsa@0.9.10
env RUSTC_WRAPPER= cargo tree --offline --locked --workspace --all-features --target all -i sqlx-mysql@0.8.6
env RUSTC_WRAPPER= cargo tree --offline --locked --workspace --all-features --target all -i sqlx-sqlite@0.8.6 --depth 2
env RUSTC_WRAPPER= cargo tree --offline --locked --workspace --all-features --target all -e features -i sqlx@0.8.6 --depth 2
```

The first two commands exit successfully and report `warning: nothing to print.`
The third shows `sqlx-sqlite` through `sqlx` and `sqlx-macros-core`.
The fourth shows `sqlite`, `any`, macros, runtime, and data-type features, without `mysql`.

From the Keymeld workspace, run:

```sh
env RUSTC_WRAPPER= cargo tree --offline --locked --workspace --all-features --target all -i rsa
```

This command also exits successfully and reports `warning: nothing to print.`
These commands resolve dependencies without compiling the workspaces.

If manifests, database features, or direct cryptography dependencies change, repeat this review.
Enabling SQLx MySQL invalidates the current reachability conclusion.
Do not delete valid lockfile entries or add a global advisory ignore to obtain a passing scan.

## Other scan results and limits

The scan also reports maintenance warnings for `bincode 1.3.3`, `instant 0.1.13`, `proc-macro-error 1.0.4`, and `serde_cbor 0.11.2`.
They are maintenance advisories, not additional vulnerability findings in this scan.
Replacing a serialization dependency requires separate wire-format and signature compatibility review.

The root review queried GitHub Dependabot on the same date.
Coordinator returned no open alerts.
Keymeld returned HTTP 403 because Dependabot alerts are disabled for that repository.
The review did not enable alerts or change authentication scopes.
This review does not replace release tests, native-library scanning, browser dependency scanning, or review of application security findings.
