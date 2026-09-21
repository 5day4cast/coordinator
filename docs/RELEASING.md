# Release procedure

The release workflow builds the version committed in Git. It does not change versions or push commits.
Use a dry run to inspect packages before publishing.

## Prepare the source

1. Set `workspace.package.version` in `Cargo.toml` and update the four workspace package versions in `Cargo.lock`.
2. Set both Helm charts' `version`, `appVersion`, and default image tags to that version.
3. Add `docs/releases/v<VERSION>.md` with changes, required upgrade steps, and validation limits.
4. Run the release helper tests, chart tests, and normal CI checks.
5. Commit and push the release branch with signed Conventional Commits.

A release's database migrations may only add tables, columns, and indexes.
During a deploy the previous version keeps running against the migrated database, so dropping or renaming waits for a later release.
See [Competition runners](COMPETITION_RUNNERS.md#running-two-coordinators).

Nix reads the workspace version from `Cargo.toml`.
The release version must use `X.Y.Z`, without a `v` prefix or leading zeros.

```bash
python3 -m unittest discover -s scripts -p 'test_release.py' -v
python3 deploy/helm/tests/test_admin_routing.py
python3 scripts/release.py validate --version 2.0.0
```

## Build review artifacts

Run the workflow against the pushed release branch:

```bash
gh workflow run release.yml --repo 5day4cast/coordinator \
  --ref release/v2.0.0 -f version=2.0.0 -f dry_run=true
gh run list --repo 5day4cast/coordinator --workflow release.yml --branch release/v2.0.0
```

Use the run ID to inspect completion and download artifacts:

```bash
gh run view <RUN_ID> --repo 5day4cast/coordinator
gh run download <RUN_ID> --repo 5day4cast/coordinator --dir release-review
```

The dry run builds native packages, browser WASM, Helm charts, and both container architectures.
It saves artifacts for 14 days without publishing a GitHub release or pushing container images.
Each native package includes the UI and the shared browser WASM build.
`RELEASE.json` records the source commit and hashes of the dependency locks and browser attestation pins.
Verify archive checksums and the contained `SHA256SUMS` files before installation.

## Publish the reviewed version

Merge the approved release PR into `master`.
Run the dry run again against `master` and verify its source commit, required CI checks, and artifacts.
Create the release tag on that exact commit only after the release is approved:

```bash
git fetch origin master
git tag -s v2.0.0 <VERIFIED_MASTER_COMMIT> -m 'Release v2.0.0'
git push origin refs/tags/v2.0.0
```

The tag push starts publication automatically.
The workflow builds from the tag's commit, publishes versioned architecture images, then publishes the multi-architecture version and `latest` tags after all builds pass.
The GitHub release follows container publication and includes the upgrade notes and package checksums.
A manual run with `dry_run=false` also requires an existing version tag at the selected commit.

Verify the completed workflow, release assets, and both container manifests before rollout.
Use the versioned image tags for deployment and follow the release's maintenance and recovery instructions.
Do not move a published version tag to a different commit.
