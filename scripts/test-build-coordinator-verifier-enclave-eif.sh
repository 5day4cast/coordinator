#!/usr/bin/env bash
# Test feature selection and measured runtime settings without Nix, Docker or Nitro builds.
set -euo pipefail
coordinator_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
coordinator_test_dir="$(mktemp -d)"
trap 'rm -rf -- "$coordinator_test_dir"' EXIT
mkdir "$coordinator_test_dir/bin"
export COORDINATOR_BUILD_TEST_DIR="$coordinator_test_dir"
export PATH="$coordinator_test_dir/bin:$PATH"
cat > "$coordinator_test_dir/bin/nix" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$COORDINATOR_BUILD_TEST_DIR/nix.log"
[[ "$1" == build && "$3" == --out-link && $# == 4 ]]
printf 'mock image archive' > "$4"
MOCK
cat > "$coordinator_test_dir/bin/docker" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  load) cat >/dev/null ;;
  build)
    printf '%s\n' "$@" > "$COORDINATOR_BUILD_TEST_DIR/docker.args"
    cp "${!#}/Dockerfile" "$COORDINATOR_BUILD_TEST_DIR/Dockerfile"
    ;;
  rmi) ;;
  *) exit 91 ;;
esac
MOCK
cat > "$coordinator_test_dir/bin/nitro-cli" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  build-enclave)
    [[ "$2" == --docker-uri && "$4" == --output-file ]]
    printf 'mock measured enclave' > "$5"
    ;;
  describe-eif) jq -n '{Measurements:{PCR0:("a"*96),PCR1:("b"*96),PCR2:("c"*96)}}' ;;
  *) exit 92 ;;
esac
MOCK
chmod +x "$coordinator_test_dir/bin/"*
export ENCLAVE_GATEWAY_PUBLIC_KEY="02$(printf '1%.0s' {1..64})"
export ENCLAVE_KMS_KEY_ID=arn:aws:kms:us-east-1:123456789012:key/11111111-2222-3333-4444-555555555555
export AWS_REGION=us-east-1 ENCLAVE_ID=7 VERSION=feature-test
unset COORDINATOR_ESCROW_VARIANT COORDINATOR_ESCROW_LNURL_ENABLED COORDINATOR_LNURL_RELAY_PORT
unset EIF_SIGNING_KEY EIF_SIGNING_CERTIFICATE EIF_NAME
cd -- "$coordinator_root"

run_case() {
  local feature="$1" enabled="$2" suffix="$3" port="$4"
  export COORDINATOR_ESCROW_VARIANT="$feature" COORDINATOR_ESCROW_LNURL_ENABLED="$enabled" COORDINATOR_LNURL_RELAY_PORT="$port"
  export OUTPUT_FILE="$coordinator_test_dir/$feature-$enabled.eif"
  bash scripts/build-coordinator-verifier-enclave-eif.sh > "$coordinator_test_dir/build.log"
  grep -Fq -- "build .#docker-coordinator-verifier-enclave$suffix --out-link " "$coordinator_test_dir/nix.log"
  grep -Fxq -- "BASE_IMAGE=coordinator-verifier-enclave$suffix:latest" "$coordinator_test_dir/docker.args"
  grep -Fxq -- "COORDINATOR_ESCROW_LNURL_ENABLED=$enabled" "$coordinator_test_dir/docker.args"
  grep -Fxq -- "COORDINATOR_LNURL_RELAY_PORT=$port" "$coordinator_test_dir/docker.args"
  grep -Fxq 'ENV COORDINATOR_ESCROW_LNURL_ENABLED=$COORDINATOR_ESCROW_LNURL_ENABLED' "$coordinator_test_dir/Dockerfile"
  grep -Fxq 'ENV COORDINATOR_LNURL_RELAY_PORT=$COORDINATOR_LNURL_RELAY_PORT' "$coordinator_test_dir/Dockerfile"
  grep -Fxq 'ENV TRANSPORT_MODE=vsock' "$coordinator_test_dir/Dockerfile"
  jq -e --arg feature "$feature" --argjson enabled "$enabled" --argjson port "$port" \
    '.escrow_variant == $feature and .escrow.lnurl.enabled == $enabled and .escrow.lnurl.relay_port == $port and .enclave_id == 7' \
    "$OUTPUT_FILE.manifest.json" >/dev/null
  : > "$coordinator_test_dir/nix.log"
}

# The feature selection never implies runtime consent.
run_case invoice false '' 8101
run_case lnurl false -lnurl 8101
run_case lnurl true -lnurl 8102
jq -e '.cargo_features == ["lnurl"] and .verifier.id == "coordinator.dlc" and .verifier.version == 1' "$OUTPUT_FILE.manifest.json" >/dev/null

reject_case() {
  export OUTPUT_FILE="$coordinator_test_dir/rejected.eif"
  if bash scripts/build-coordinator-verifier-enclave-eif.sh > "$coordinator_test_dir/rejected.log" 2>&1; then
    echo "Invalid feature/runtime combination was accepted" >&2; exit 1
  fi
  [[ ! -s "$coordinator_test_dir/nix.log" && ! -e "$OUTPUT_FILE" ]]
}
export COORDINATOR_ESCROW_VARIANT=invoice COORDINATOR_ESCROW_LNURL_ENABLED=true
reject_case
export COORDINATOR_ESCROW_VARIANT=unknown
reject_case
export COORDINATOR_ESCROW_VARIANT=lnurl COORDINATOR_ESCROW_LNURL_ENABLED=yes
reject_case
export COORDINATOR_ESCROW_LNURL_ENABLED=false COORDINATOR_LNURL_RELAY_PORT=0
reject_case
export COORDINATOR_LNURL_RELAY_PORT=4294967296
reject_case
export COORDINATOR_LNURL_RELAY_PORT=8101 COORDINATOR_ESCROW_VARIANT=unrecognized
reject_case

# Omitted settings produce a invoice-only image with measured LNURL disabled.
unset COORDINATOR_ESCROW_VARIANT COORDINATOR_ESCROW_LNURL_ENABLED COORDINATOR_LNURL_RELAY_PORT
export OUTPUT_FILE="$coordinator_test_dir/default.eif"
bash scripts/build-coordinator-verifier-enclave-eif.sh > "$coordinator_test_dir/default.log"
jq -e '.escrow_variant == "invoice" and .escrow.lnurl.enabled == false' "$OUTPUT_FILE.manifest.json" >/dev/null
printf '%s\n' 'EIF escrow feature/runtime build regressions passed (4 builds, 6 rejected configurations; all infrastructure mocked).'
