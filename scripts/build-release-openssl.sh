#!/usr/bin/env bash
# Build the patched native dependency for portable coordinator release archives.
set -euo pipefail

fail() { printf 'OpenSSL release build: %s\n' "$*" >&2; exit 1; }
[[ $# -eq 2 ]] || fail 'usage: build-release-openssl.sh RUST_TARGET ABSOLUTE_PREFIX'
target=$1
prefix=$2
case "$target" in
  x86_64-unknown-linux-gnu) configure_target=linux-x86_64 ;;
  x86_64-apple-darwin) configure_target=darwin64-x86_64-cc ;;
  aarch64-apple-darwin) configure_target=darwin64-arm64-cc ;;
  *) fail "unsupported target: $target" ;;
esac

# Configure writes paths into Makefiles as well as shell scripts. Restrict paths
# before passing them through either format, including GitHub's environment file.
safe_path() {
  [[ $1 == /* && $1 != / && $1 != *[!A-Za-z0-9_./-]* ]] || return 1
  case "/${1#/}/" in */../*|*/./*) return 1 ;; esac
}
safe_path "$prefix" || fail 'prefix must be an absolute path containing only letters, digits, /, _, ., and -'
[[ ! -e "$prefix" && ! -L "$prefix" ]] || fail 'prefix already exists; use a fresh build directory'
temp_root=${TMPDIR:-/tmp}
safe_path "$temp_root" || fail 'unsafe temporary directory'
jobs=${OPENSSL_BUILD_JOBS:-2}
[[ $jobs =~ ^[1-9][0-9]?$ ]] || fail 'OPENSSL_BUILD_JOBS must be an integer from 1 to 99'

readonly version=3.6.4
# Published at https://openssl-library.org/source/ and the release's .sha256 asset.
readonly expected_sha256=9bffaa1ad1e07b354c21bd3324ec02fa15579f45a7d0494b3e74bc449b7333ef
readonly source_url="https://github.com/openssl/openssl/releases/download/openssl-$version/openssl-$version.tar.gz"
work_dir=$(mktemp -d "$temp_root/coordinator-openssl.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT
archive="$work_dir/openssl.tar.gz"
curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --connect-timeout 20 --max-time 300 --retry 3 --output "$archive" "$source_url"
if command -v sha256sum >/dev/null 2>&1; then
  actual_sha256=$(sha256sum "$archive")
elif command -v shasum >/dev/null 2>&1; then
  actual_sha256=$(shasum -a 256 "$archive")
else
  fail 'sha256sum or shasum is required'
fi
actual_sha256=${actual_sha256%% *}
[[ $actual_sha256 == "$expected_sha256" ]] || fail 'release archive SHA-256 mismatch'
tar -xzf "$archive" -C "$work_dir"
(
  cd "$work_dir/openssl-$version"
  # The fixed Configure targets supply the correct architecture, including
  # -arch x86_64 on an arm64 macOS runner. Do not inherit host build overrides.
  unset CC CXX AR AS RANLIB CFLAGS CXXFLAGS CPPFLAGS LDFLAGS
  unset MAKEFLAGS MFLAGS MAKEOVERRIDES PERL5OPT PERL5LIB
  ./Configure "$configure_target" no-shared no-module no-tests \
    "--prefix=$prefix" --openssldir=/etc/ssl --libdir=lib
  make -j"$jobs" build_libs
  make install_dev
)
[[ -s "$prefix/lib/libssl.a" && -s "$prefix/lib/libcrypto.a" ]] || fail 'static libraries were not installed'
[[ -s "$prefix/include/openssl/opensslv.h" ]] || fail 'OpenSSL headers were not installed'
for library in "$prefix"/lib/*.so* "$prefix"/lib/*.dylib; do
  [[ ! -e "$library" ]] || fail "unexpected shared library: $library"
done

target_env=$(printf '%s' "${target//-/_}" | tr '[:lower:]' '[:upper:]')
build_environment() {
  printf 'OPENSSL_DIR=%s\nOPENSSL_LIB_DIR=%s/lib\nOPENSSL_INCLUDE_DIR=%s/include\n' "$prefix" "$prefix" "$prefix"
  printf 'OPENSSL_STATIC=1\nOPENSSL_NO_VENDOR=1\nPKG_CONFIG_PATH=%s/lib/pkgconfig\n' "$prefix"
  # openssl-sys prefers target-specific variables over the generic variables.
  printf '%s_OPENSSL_DIR=%s\n%s_OPENSSL_LIB_DIR=%s/lib\n%s_OPENSSL_INCLUDE_DIR=%s/include\n' \
    "$target_env" "$prefix" "$target_env" "$prefix" "$target_env" "$prefix"
  printf '%s_OPENSSL_STATIC=1\n%s_OPENSSL_NO_VENDOR=1\n' "$target_env" "$target_env"
}
if [[ -n ${GITHUB_ENV:-} ]]; then
  build_environment >> "$GITHUB_ENV"
else
  build_environment
fi
printf 'Built static OpenSSL %s for %s at %s\n' "$version" "$target" "$prefix"
