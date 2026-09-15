#!/usr/bin/env bash
#
# fetch-duckdb.sh — put a DuckDB engine into ~/.duckdb/cli/2.0.0/
#
# harbor carries no engine; this fetches one for it to load, along
# with the two headers (kept for reference — the crate ships pregenerated
# bindings, so the build never reads them) and the duckdb CLI that builds
# fixtures.
#
# The source, until DuckDB 2.0 GA, is this repo's own shelf: the Engine
# workflow builds all five platforms at CI's pinned commit and shelves them
# on the engine-<pin> prerelease, in the shape DuckDB's official channel
# used to ship (duckdb-binaries-<plat>.zip wrapping libduckdb-<plat>.zip
# and duckdb_cli-<plat>.zip). No official artifact can serve harbor 0.21:
# the nightly channel was frozen pre-v2-API, and on 2026-09-14 DuckDB
# retired it outright — artifacts.duckdb.org/latest is gone, nightlies
# now live under branch-keyed paths (v2.0-cyanoptera/…) in a new tar.gz
# shape that still exports no v2 C API.
#
# <pin> is the first 10 chars of the commit in
# .github/actions/duckdb/action.yml — the ONE place the pin lives. This
# script reads it from there when run inside the checkout; elsewhere, or
# to fetch any other engine, set ENGINE_URL:
#
#   ENGINE_URL=https://github.com/shreeve/duckdb-harbor/releases/download/engine-<pin>/duckdb-binaries-<plat>.zip
#
# This script warns loudly when the fetched library cannot serve. At GA,
# point the default at the official channel and delete the warning below.
#
# Override DEST to install elsewhere.

set -euo pipefail

dest=${DEST:-$HOME/.duckdb/cli/2.0.0}

duck_plat=${DUCKDB_PLATFORM:-}
if [ -z "$duck_plat" ]; then
  case "$(uname -s)/$(uname -m)" in
    Darwin/*)                  duck_plat=osx         ;;
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

# ---- the engine: one "binaries" zip, two sub-zips nested inside -------------
engine_url=${ENGINE_URL:-}
if [ -z "$engine_url" ]; then
  # Default to the engine-<pin> shelf, pin read from the composite action so
  # a pin bump there flows here without a second edit.
  pin_file="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)/.github/actions/duckdb/action.yml"
  pin=$(grep -o 'sha=[0-9a-f]\{40\}' "$pin_file" 2>/dev/null | cut -d= -f2 || true)
  [ -n "$pin" ] || { echo "fetch-duckdb: no engine pin at $pin_file — set ENGINE_URL (see header)" >&2; exit 2; }
  engine_url="https://github.com/shreeve/duckdb-harbor/releases/download/engine-${pin:0:10}/duckdb-binaries-$duck_plat.zip"
fi
say "fetching $engine_url"
curl -fsSL -o "$work/binaries.zip" "$engine_url"
( cd "$work" && unzip -oq binaries.zip )        # -> libduckdb-*.zip, duckdb_cli-*.zip
( cd "$work" && unzip -oq 'libduckdb-*.zip' )   # -> libduckdb.{dylib,so} (+ headers)
( cd "$work" && unzip -oq 'duckdb_cli-*.zip' )  # -> duckdb CLI

mkdir -p "$dest"
place libduckdb.dylib    0755
place libduckdb.so       0755
place duckdb.dll         0755
place duckdb.lib         0644
place duckdb             0755
place duckdb.exe         0755
place duckdb.h           0644
place duckdb_extension.h 0644

# A fetch that placed no engine is a failure, not a quiet success — the
# same rule package-release.sh enforces. Without this, a malformed or
# empty archive would sail through to "ready" with nothing installed.
[ -f "$dest/libduckdb.dylib" ] || [ -f "$dest/libduckdb.so" ] || [ -f "$dest/duckdb.dll" ] \
  || { echo "fetch-duckdb: the archive contained no libduckdb" >&2; exit 1; }

# ---- point cli/latest at what we just refreshed ----------------------------
# Only when we filled the canonical dir — a throwaway DEST elsewhere (a scratch
# test, a one-off build root) has no business owning `latest`.
if [ "$dest" = "$HOME/.duckdb/cli/2.0.0" ]; then
  ln -sfn "$dest" "$HOME/.duckdb/cli/latest"
  say "cli/latest -> $dest"
fi

# ---- can this engine actually serve harbor 0.21? ---------------------------
# harbor 0.21 binds the v2 C API; no official artifact exports it. grep the
# dynamic symbol names straight out of the binary — present on every
# platform, no nm/objdump dependency. Delete this check at GA.
for f in "$dest"/libduckdb.dylib "$dest"/libduckdb.so "$dest"/duckdb.dll; do
  [ -f "$f" ] || continue
  if ! grep -q duckdb_v2_connect "$f" 2>/dev/null; then
    echo "" >&2
    echo "fetch-duckdb: WARNING — this libduckdb exports no v2 C API symbols." >&2
    echo "  harbor cannot serve with it (no official artifact will until" >&2
    echo "  DuckDB 2.0 GA). Re-run with ENGINE_URL pointed at this repo's" >&2
    echo "  engine-<pin> release — the exact line is in this script's header." >&2
  fi
  break
done
echo "fetch-duckdb: ready in $dest"
