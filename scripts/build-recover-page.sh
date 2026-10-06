#!/usr/bin/env bash
# Build the recovery page as static files, for hosting anywhere once the coordinator is gone:
# index.html, recover.js, recover.css, Bulma, and the WASM package in pkg/.
#
# Usage: scripts/build-recover-page.sh <out-dir> [<wasm-pkg-dir>]
#
# With a WASM package already built (scripts/build-wasm.sh), it is copied; otherwise this builds
# one, with the same tools scripts/build-wasm.sh needs. Serve the directory from any static web
# server (GitHub Pages, IPFS, `python3 -m http.server`); opened as a file the browser will not
# load the module.
set -euo pipefail

out=${1:?usage: scripts/build-recover-page.sh <out-dir> [<wasm-pkg-dir>]}
root=$(cd "$(dirname "$0")/.." && pwd)
pages="$root/crates/coordinator/src/templates"

mkdir -p "$out/pkg"
if [ -n "${2:-}" ]; then
  cp "$2/coordinator_wasm.js" "$2/coordinator_wasm_bg.wasm" "$out/pkg/"
else
  "$root/scripts/build-wasm.sh" "$out/pkg"
fi
cp "$pages/pages/recover/standalone.html" "$out/index.html"
cp "$pages/static/recover.js" "$out/recover.js"
cp "$pages/pages/recover/recover.css" "$out/recover.css"
cp "$root/vendor/bulma/1.0.2/bulma.min.css" "$out/bulma.min.css"
# The package's own extras (TypeScript types, package.json) are not needed by the page.
find "$out/pkg" -type f ! -name 'coordinator_wasm.js' ! -name 'coordinator_wasm_bg.wasm' -delete
(cd "$out" && sha256sum index.html recover.js recover.css bulma.min.css pkg/* > SHA256SUMS)
echo "Recovery page in $out"
