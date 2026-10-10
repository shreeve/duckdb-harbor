#!/usr/bin/env bash
#
# fetch-duckdb.sh — put a DuckDB engine and its CLI into ~/.duckdb/cli/2.0.0/
#
# harbor carries no engine; this fetches one for it to load, beside the
# duckdb CLI of the same build, which the test suites build fixtures and read
# oracles with. Override DUCKDB_PLATFORM to fetch for another machine, DEST
# to install elsewhere.
#
# DUCKDB_LIB_BUILD names the build. `latest`, the default, is DuckDB's
# nightly channel at artifacts.duckdb.org, keyed by branch (DUCKDB_CHANNEL,
# `v2.0-cyanoptera` for the 2.0 line): `duckdb-shared-libs-<plat>.tar.gz`
# (the library and headers) and `duckdb-cli-<plat>.tar.gz`. The channel is a
# moving pointer, the latest green build of the branch with no way to ask
# for an older one, so a fetch from it is deliberately current.
#
# Any other value is a DuckDB build by name (`alpha42289`, the tail of
# `v2.0.0-alpha42289`), fetched whole from the `engine-<build>` release of
# this repository: `libduckdb-<plat>.tar.gz` and `duckdb-cli-<plat>.tar.gz`,
# each checked against `engine-<build>-checksums.txt`. Nothing comes from the
# channel, so a channel outage cannot stop a pinned build, and the library
# and the CLI are one build, which each binary's version string must say.
# The headers are reference only (the crate ships pregenerated bindings), so
# a named build carries none. CI and Release.yml pass the repository
# variable of the same name.
#
# Either way, the library must export the v2 C API harbor binds: the channel
# has shipped ones that do not, and harbor refuses such an engine at dlopen.

set -euo pipefail

dest=${DEST:-$HOME/.duckdb/cli/2.0.0}
channel=${DUCKDB_CHANNEL:-v2.0-cyanoptera}
build=${DUCKDB_LIB_BUILD:-latest}

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

if [ "$build" = latest ]; then
  for kind in shared-libs cli; do
    get "https://artifacts.duckdb.org/$channel/duckdb-$kind-$duck_plat.tar.gz" "$work/$kind.tar.gz"
    tar -xzf "$work/$kind.tar.gz" -C "$work"
  done
else
  case "$duck_plat" in
    osx-arm64|linux-amd64|linux-arm64|windows-amd64|windows-arm64) ;;
    *) echo "fetch-duckdb: no engine release carries $duck_plat — use DUCKDB_LIB_BUILD=latest" >&2; exit 2 ;;
  esac
  base="https://github.com/shreeve/duckdb-harbor/releases/download/engine-$build"
  sums="engine-$build-checksums.txt"
  get "$base/$sums" "$work/$sums" || fail "no engine-$build release (no $sums)"
  for asset in "libduckdb-$duck_plat.tar.gz" "duckdb-cli-$duck_plat.tar.gz"; do
    get "$base/$asset" "$work/$asset" || fail "engine-$build has no $asset"
    want=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$work/$sums")
    [ -n "$want" ] || fail "$sums does not list $asset"
    [ "$(sha256 "$work/$asset")" = "$want" ] || fail "$asset does not match its checksum in $sums"
    tar -xzf "$work/$asset" -C "$work"
  done
fi

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
if ! grep -q duckdb_v2_create_environment "$lib" 2>/dev/null; then
  echo "fetch-duckdb: this libduckdb lacks the v2 C API harbor binds — harbor cannot serve with it." >&2
  if [ "$build" = latest ]; then
    echo "  The $channel channel has moved past this harbor; name a build it loads with DUCKDB_LIB_BUILD." >&2
  fi
  exit 1
fi

# Each binary carries its version string, `v2.0.0-<build>`.
if [ "$build" != latest ]; then
  cli=$(grab duckdb); [ -n "$cli" ] || cli=$(grab duckdb.exe)
  [ -n "$cli" ] || fail "duckdb-cli-$duck_plat.tar.gz holds no duckdb CLI"
  for bin in "$lib" "$cli"; do
    grep -q -- "-$build" "$bin" 2>/dev/null || fail "$(basename "$bin") in engine-$build does not say it is build $build"
  done
fi

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

echo "fetch-duckdb: ready in $dest (build: $build)"
