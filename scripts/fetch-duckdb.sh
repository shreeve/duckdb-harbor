#!/usr/bin/env bash
#
# fetch-duckdb.sh — put a DuckDB engine into ~/.duckdb/cli/2.0.0/
#
# harbor carries no engine (PLAN.md D1); this fetches one for it to link, along
# with the two headers (kept for reference — the crate ships pregenerated
# bindings, so the build never reads them) and the duckdb CLI that builds
# fixtures. The source is DuckDB's official v2.0-dev nightly, from
# artifacts.duckdb.org — the current 2.0 line straight from upstream, no fork,
# nothing to pin. `/latest/` is the v2.0-dev channel today (it reported
# v2.0.0-alpha38069 when this was written); if main ever rolls past 2.0, pin the
# v2.0 channel URL below. The matched UI extension is built separately against
# whatever this installs — see scripts/build-ui-extension.sh (`make ui`).
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
say()   { printf '  %s\n' "$*"; }
grab()  { find "$work" -type f -name "$1" 2>/dev/null | head -1; }
place() { local s; s=$(grab "$1") || true; [ -n "$s" ] && install -m "$2" "$s" "$dest/$1" && say "-> $dest/$1"; return 0; }

# ---- the engine: one "binaries" zip, two sub-zips nested inside -------------
engine_url=${ENGINE_URL:-https://artifacts.duckdb.org/latest/duckdb-binaries-$duck_plat.zip}
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

# ---- point cli/latest at what we just refreshed ----------------------------
# Only when we filled the canonical dir — a throwaway DEST elsewhere (a scratch
# test, a one-off build root) has no business owning `latest`.
if [ "$dest" = "$HOME/.duckdb/cli/2.0.0" ]; then
  ln -sfn "$dest" "$HOME/.duckdb/cli/latest"
  say "cli/latest -> $dest"
fi
echo "fetch-duckdb: ready in $dest"
