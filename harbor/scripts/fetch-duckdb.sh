#!/usr/bin/env bash
#
# fetch-duckdb.sh — put a DuckDB engine and its CLI into ~/.duckdb/cli/2.0.0/
#
# harbor carries no engine; this fetches one for it to load, beside the
# duckdb CLI of the same build, which the test suites build fixtures and read
# oracles with. Override DUCKDB_PLATFORM to fetch for another machine, DEST
# to install elsewhere.
#
# The engine is the DuckDB release harbor binds, taken from DuckDB's own
# GitHub release: `duckdb-shared-libs-<plat>.tar.gz` (the library and its
# headers) and `duckdb-cli-<plat>.tar.gz`, each checked against the SHA-256
# recorded below, so the library and the CLI are one build and an archive
# that changed under its name is refused. Moving to another DuckDB release
# is a change to `version` and the table, together with the bindings
# (scripts/gen-v2-ffi.rb) and the loader's gate.

set -euo pipefail

version=v2.0.0
dest=${DEST:-$HOME/.duckdb/cli/2.0.0}

# SHA-256 of each archive of $version, as GitHub reports the release's assets.
checksum() {
  case "$1" in
    duckdb-shared-libs-osx-arm64.tar.gz)      echo c35d60080b31ba5d11a3ce505a6831be784f003b4c575d32a4c2badb3b41cdf0 ;;
    duckdb-shared-libs-osx-universal.tar.gz)  echo 14cd63180614cfd961f7959146ecd9087527ff20b67311efdc3fa1c29c18204d ;;
    duckdb-shared-libs-linux-amd64.tar.gz)    echo 993ad89e7dfca66cd6da994c2cad090c2fc160ddf9f550498a098ab4bb13cfb1 ;;
    duckdb-shared-libs-linux-arm64.tar.gz)    echo c60eecf91143ca6bbac55c4f4d40dc22520488b0ac257c86e0fe9a2b451cf2b7 ;;
    duckdb-shared-libs-windows-amd64.tar.gz)  echo 2b83169128239de5f45fe21de80f99bf436e53aa4bb24020dd951a1ba8d4b95f ;;
    duckdb-shared-libs-windows-arm64.tar.gz)  echo 159ce60eb587112b8c5f290b8beea56af5f13be4d13679de127ad9bfaafb7447 ;;
    duckdb-cli-osx-arm64.tar.gz)              echo 188a36fe8883ef52e69165a0cbb6031ef679c8e6c448a1ce22951964149b7f96 ;;
    duckdb-cli-osx-universal.tar.gz)          echo 8a8f67e7c7f7d1392fa63ba3971948391392fe2a25fca63e7008e33c50beb4c2 ;;
    duckdb-cli-linux-amd64.tar.gz)            echo c6507206baf7793c6013dbd82368469a45f20dc0e17a6f92f204602f652ed2bc ;;
    duckdb-cli-linux-arm64.tar.gz)            echo 859e1875d2a9515fbb4bf8871061437a14af82320da68c753f094c4b776348fe ;;
    duckdb-cli-windows-amd64.tar.gz)          echo 91663c2f36c2a334b0e5e639006d8c29c29e013af0c477d80bb931df68870312 ;;
    duckdb-cli-windows-arm64.tar.gz)          echo 4b5d1e4611debf676a693a8a6896dc29b352a9c3b11b706e7421d1d6082e05fd ;;
  esac
}

duck_plat=${DUCKDB_PLATFORM:-}
if [ -z "$duck_plat" ]; then
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64)              duck_plat=osx-arm64   ;;
    Darwin/*)                  duck_plat=osx-universal ;;
    Linux/x86_64)              duck_plat=linux-amd64 ;;
    Linux/aarch64|Linux/arm64) duck_plat=linux-arm64 ;;
    MINGW*/*86_64|MSYS*/*86_64) duck_plat=windows-amd64 ;;
    MINGW*/aarch64|MSYS*/aarch64|MINGW*/arm64|MSYS*/arm64) duck_plat=windows-arm64 ;;
    *) echo "fetch-duckdb: unsupported platform $(uname -s)/$(uname -m)" >&2; exit 2 ;;
  esac
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/fetch-duckdb.XXXXXX")
trap 'rm -rf "$work"' EXIT
say()  { printf '  %s\n' "$*"; }
fail() { echo "fetch-duckdb: $*" >&2; exit 1; }
# --retry: a transient 5xx or timeout from a CDN is not worth a failed job.
# A 404 is not retried, and -f makes it a failure.
get()  { say "fetching $1"; curl -fsSL --retry 3 -o "$2" "$1"; }
sha256() { { sha256sum "$1" 2>/dev/null || shasum -a 256 "$1"; } | cut -d' ' -f1; }
# /usr/bin/find + -print -quit: GNU find by absolute path (Git Bash can
# shadow bare `find` with the DOS one), and no `| head` for pipefail to
# turn into a silent death. Empty result is status 0 by design.
grab() { /usr/bin/find "$work" -type f -name "$1" -print -quit 2>/dev/null; }
place() {
  # A file the archive doesn't carry is skipped (each platform ships its
  # own subset); a failed install is a real error and propagates.
  local s; s=$(grab "$1")
  if [ -n "$s" ]; then
    install -m "$2" "$s" "$dest/$1"
    say "-> $dest/$1"
  fi
}

base="https://github.com/duckdb/duckdb/releases/download/$version"
for asset in "duckdb-shared-libs-$duck_plat.tar.gz" "duckdb-cli-$duck_plat.tar.gz"; do
  want=$(checksum "$asset")
  [ -n "$want" ] || fail "DuckDB $version has no $asset harbor knows the checksum of"
  get "$base/$asset" "$work/$asset" || fail "DuckDB $version has no $asset"
  [ "$(sha256 "$work/$asset")" = "$want" ] || fail "$asset does not match its recorded checksum"
  tar -xzf "$work/$asset" -C "$work"
done

# ---- is this the engine asked for, and can it serve harbor? ----------------
# Both questions are put to the binaries in the work tree, so one that fails
# either never lands in $dest. grep reads the names straight out of the
# binary — present on every platform, no nm/objdump dependency.
lib=
for name in libduckdb.dylib libduckdb.so duckdb.dll; do
  lib=$(grab "$name")
  [ -z "$lib" ] || break
done
# A fetch that carried no engine is a failure, not a quiet success — the
# same rule package-release.sh enforces.
[ -n "$lib" ] || fail "the archives contained no libduckdb"

# harbor binds the v2 C API and refuses a library without it at dlopen,
# later and less clearly than here. The name asked for is the one harbor's
# loader gates on (engine/mod.rs, `boot`), whole: it is no prefix of another
# symbol, so a library with a different v2 surface does not pass by accident.
grep -q duckdb_v2_instance_get_factory "$lib" 2>/dev/null \
  || fail "this libduckdb lacks the v2 C API harbor binds — harbor cannot serve with it."

# Each binary carries its version string.
cli=$(grab duckdb); [ -n "$cli" ] || cli=$(grab duckdb.exe)
[ -n "$cli" ] || fail "duckdb-cli-$duck_plat.tar.gz holds no duckdb CLI"
for bin in "$lib" "$cli"; do
  grep -q -- "$version" "$bin" 2>/dev/null || fail "$(basename "$bin") does not say it is DuckDB $version"
done

mkdir -p "$dest"
place libduckdb.dylib    0755
place libduckdb.so       0755
place duckdb.dll         0755
place duckdb.lib         0644
place duckdb             0755
place duckdb.exe         0755
place duckdb.h           0644
place duckdb_v2.h        0644
place duckdb_extension.h 0644
place duckdb_extension_v2.h 0644

echo "fetch-duckdb: DuckDB $version ready in $dest"
