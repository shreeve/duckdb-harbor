#!/usr/bin/env bash
#
# fetch-duckdb.sh — put a DuckDB engine into ~/.duckdb/cli/2.0.0/
#
# harbor carries no engine; this fetches one for it to load, along
# with the headers (kept for reference — the crate ships pregenerated
# bindings, so the build never reads them) and the duckdb CLI that builds
# fixtures. The source is DuckDB's official nightly channel at
# artifacts.duckdb.org: two tarballs per platform, `duckdb-shared-libs-
# <plat>.tar.gz` (the library and headers) and `duckdb-cli-<plat>.tar.gz`
# (the CLI), keyed by branch. The 2.0 line is `v2.0-cyanoptera`; override
# DUCKDB_CHANNEL to fetch another branch, DUCKDB_PLATFORM to fetch for
# another machine, DEST to install elsewhere.
#
# The channel is a moving pointer — the latest green build of the branch,
# with no way to ask for an older one — so what this fetches today is not
# what it fetched yesterday. The release archives bundle the engine they
# were built with, which is what makes a release reproducible; a local
# fetch is deliberately current. The script checks that the library it
# got exports the v2 C API harbor binds, because the channel has shipped
# ones that did not, and harbor refuses such an engine at dlopen.
#
# DUCKDB_LIB_BUILD names the build of the library: `latest`, the default,
# is the channel's; anything else is a DuckDB build by name (`alpha42289`,
# the tail of `v2.0.0-alpha42289`), fetched from the `engine-<build>`
# release of this repository, which holds one `libduckdb-<plat>.tar.gz` per
# platform. The channel cannot serve a build by name, so an engine that has
# to stay fixed is published there. The library says which build it is, and
# the script refuses one that is not the build it was asked for. The CLI
# and the headers still come from the channel, so they may run ahead of a
# named library — the headers are reference only. CI and Release.yml read
# the same name from the repository variable of that name; `latest`, or
# clearing it, returns to the channel.
#
# Override DEST to install elsewhere.

set -euo pipefail

dest=${DEST:-$HOME/.duckdb/cli/2.0.0}
channel=${DUCKDB_CHANNEL:-v2.0-cyanoptera}
build=${DUCKDB_LIB_BUILD:-latest}
[ -n "$build" ] || build=latest

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

# ---- the engine: two tarballs, the library (with headers) and the CLI -----
base="https://artifacts.duckdb.org/$channel"
for kind in shared-libs cli; do
  url="$base/duckdb-$kind-$duck_plat.tar.gz"
  say "fetching $url"
  curl -fsSL -o "$work/$kind.tar.gz" "$url"
  tar -xzf "$work/$kind.tar.gz" -C "$work"
done

# ---- a named build: the library from this repository's engine release -----
# The channel's library is discarded and the named one takes its place in
# the work tree, so the placing and the checks below see one library.
if [ "$build" != latest ]; then
  case "$duck_plat" in
    osx-arm64|linux-amd64|linux-arm64|windows-amd64|windows-arm64) ;;
    *) echo "fetch-duckdb: no engine release carries $duck_plat — use DUCKDB_LIB_BUILD=latest" >&2; exit 2 ;;
  esac
  url="https://github.com/shreeve/duckdb-harbor/releases/download/engine-$build/libduckdb-$duck_plat.tar.gz"
  say "build $build: fetching $url"
  /usr/bin/find "$work" -type f \( -name libduckdb.dylib -o -name libduckdb.so \
    -o -name duckdb.dll -o -name duckdb.lib \) -delete
  curl -fsSL -o "$work/libduckdb.tar.gz" "$url" \
    || { echo "fetch-duckdb: no engine-$build release, or none for $duck_plat" >&2; exit 1; }
  tar -xzf "$work/libduckdb.tar.gz" -C "$work"
fi

# ---- is this the engine asked for, and can it serve harbor? ----------------
# Both questions are put to the library in the work tree, so one that fails
# either never lands in $dest. grep reads the names straight out of the
# binary — present on every platform, no nm/objdump dependency.
lib=
for name in libduckdb.dylib libduckdb.so duckdb.dll; do
  lib=$(grab "$name")
  [ -z "$lib" ] || break
done
# A fetch that carried no engine is a failure, not a quiet success — the
# same rule package-release.sh enforces.
[ -n "$lib" ] || { echo "fetch-duckdb: the archives contained no libduckdb" >&2; exit 1; }

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

# A library carries its version string, `v2.0.0-<build>`.
if [ "$build" != latest ] && ! grep -q -- "-$build" "$lib" 2>/dev/null; then
  echo "fetch-duckdb: the library in engine-$build does not say it is build $build." >&2
  exit 1
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

# ---- point cli/latest at what we just refreshed ----------------------------
# Only when we filled the canonical dir — a throwaway DEST elsewhere (a scratch
# test, a one-off build root) has no business owning `latest`.
if [ "$dest" = "$HOME/.duckdb/cli/2.0.0" ]; then
  ln -sfn "$dest" "$HOME/.duckdb/cli/latest"
  say "cli/latest -> $dest"
fi

echo "fetch-duckdb: ready in $dest (libduckdb build: $build)"
