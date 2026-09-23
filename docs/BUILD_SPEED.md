# Faster builds: plan

Agreed on 2026-09-23. A dry-run release took 33 minutes, most of it rebuilding the same third-party
code on every run. [Status](#status) records what is in place.

## Where the time goes

Release run 35809194846:

| Job | Time | What it does |
|---|---|---|
| Docker images (amd64) | 32m36s | `nix build` of the images: 28m41s, every dependency from scratch |
| Docker images (arm64) | 26m16s | The same, on the ARM runner |
| Native package (Linux) | 11m07s | Starts 3 minutes in, after the WASM job |
| Native packages (macOS) | 8–9m | Not on the critical path |
| Browser WASM | 2m50s | Includes `cargo install wasm-bindgen-cli` from source |

In the Linux job:

- About 650 of the lockfile's 688 crates are compiled from cold. About 40 of them are ours.
- The second `cargo build` (enclave, relay, ark-swapd, synth) recompiles about 220 crates the first
  had already built. They resolve with different features, so this takes 3m31s.
- OpenSSL takes 1m34s, built with 2 jobs on a 4-core runner.

67 crates are pulled in at more than one version, adding 97 extra builds. Among them:

- `reqwest` 0.11, 0.12 and 0.13, each with its own hyper and http stack;
- `secp256k1` 0.29, 0.31 and 0.33, each compiling the C library;
- `rand` 0.8, 0.9 and 0.10;
- `syn` 1, 2 and 3;
- two `axum-core`.

## Decisions

1. **A builder image for the Linux build.**
   - Push `ghcr.io/5day4cast/coordinator-builder:<Cargo.lock hash>` to GHCR, holding the toolchain,
     the static OpenSSL, and every dependency precompiled with `cargo chef cook` for the release
     target and features.
   - The native Linux job runs inside it, compiling only our crates. Expected: about 11 minutes down
     to about 3.
   - Rebuild the image only when `Cargo.lock`, the toolchain, or the OpenSSL script changes.
   - **Published releases build from it too**, not only dry runs. A release then depends on an image
     this repository's CI built earlier from the same lockfile. Record the image digest in
     `RELEASE.json`.
   - The macOS builds can't use a container. They stay as they are, or get `Swatinem/rust-cache`.
2. **A Nix binary cache on the lab, served by attic.**
   - The flake already builds dependencies separately (`workspaceDeps` in `flake.nix`), so the image
     jobs need only a cache that persists between runs. Expected: 28 minutes down to about 5.
   - Run it on lab-apps (little CPU, some disk), published through the hub like the other public
     names, so GitHub's runners can reach it.
     - CI pushes with a write token kept as a repository secret.
     - Reads can be public, or use a read token.
   - The laptop and the lab nodes use it as a substituter too, so the lab can pull prebuilt packages.
   - nixos_setup work: an `apps/attic` module, a public route on the hub, a DNS name, the signing
     key's public half in each host's `trusted-public-keys`, and a garbage-collection policy.
3. **Fewer dependencies:**
   - Build both Linux binary sets with the same features, so the second build reuses the first.
     Either line the features up, or build them in one `cargo build` after checking that the
     coordinator binary's features don't change.
   - Align our own crates, including keymeld (`~/repos/keymeld-bip340`), on one `reqwest`, one
     `secp256k1` and one `rand`.
   - Leave the duplicates that come from ark-client, nostr and the AWS SDK for later.

## Smaller wins to fold in

- OpenSSL: `OPENSSL_BUILD_JOBS=$(getconf _NPROCESSORS_ONLN)`.
- Install `wasm-bindgen-cli` prebuilt, or from the cache, instead of compiling it on every run.
- The Linux job waits about 3 minutes for the WASM. Only the final assembly step needs the WASM, so
  that step can move to its own small job.

## Status

- **Builder image: in place.** `.github/builder/Dockerfile` pins Rust, the static OpenSSL (built on
  every core), `wasm-bindgen-cli`, and the Linux and WASM dependencies cooked with cargo-chef.
  `scripts/builder_image.py tag` names it after the lockfile, the manifests, and the image's own
  inputs, leaving out the workspace's version so a release bump reuses the image.
  `builder-image.yml` publishes it when those inputs change on `master`, and the release calls
  the same workflow, which reuses the published image or builds it once. The WASM and Linux jobs
  run inside it. Every Linux and WASM package records the image digest as `builder_image` in
  `RELEASE.json`.
- **One Linux build: in place.** `scripts/release-cargo.sh` builds all five Linux binaries in one
  `cargo build`, and the image cooks with the same arguments. The coordinator's dependencies gain
  only additive features (Keymeld's `enclave` and `networking` modules, `pem`, `serde`, tonic's
  router, rustls-native-certs on the AWS client); reqwest still selects webpki roots.
- **Packaging split: in place.** The Linux build no longer waits for the WASM. It hands its binaries
  and UI bundles to `package-linux`, the only Linux step that needs the WASM.
- **Nix cache: waiting on the lab.** The image jobs substitute from and push to an attic cache when
  the `ATTIC_ENDPOINT`, `ATTIC_CACHE` and `ATTIC_PUBLIC_KEY` repository variables and the
  `ATTIC_TOKEN` secret are set. The flake's image builds already match `workspaceDeps`'s features,
  so a cache hit skips the dependency build. The server is `apps/attic` in nixos_setup.
- **Dependency alignment: not started.**
- **macOS:** unchanged; dry runs keep `Swatinem/rust-cache`.
