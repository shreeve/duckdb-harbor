#!/usr/bin/env bash
#
# fetch-duckdb.sh — put a DuckDB engine into ~/.duckdb/cli/2.0.0/
#
# harbor carries no engine (PLAN.md D1); this fetches one for it to link, along
# with the two headers (kept for reference — the crate ships pregenerated
# bindings, so the build never reads them) and the duckdb CLI that builds
# fixtures. Two modes:
#
#   make fetch-duckdb        DuckDB's official v2.0-dev nightly, from
#                            artifacts.duckdb.org — the current 2.0 line straight
#                            from upstream. The common case: a working engine,
#                            no fork, nothing to pin. `/latest/` is the v2.0-dev
#                            channel today (it reported v2.0.0-alpha38069 when
#                            this was written); if main ever rolls past 2.0, pin
#                            the v2.0 channel URL below.
#
#   make fetch-duckdb UI=1   the matched engine+UI pair, pinned, from our own
#                            releases. The UI extension loads ONLY against the
#                            exact engine it was built with, and the official
#                            nightly moves daily, so the two must travel together
#                            as a frozen pair until the UI factory publishes
#                            against the nightly.
#
# Override DEST to install elsewhere.

set -euo pipefail

dest=${DEST:-$HOME/.duckdb/cli/2.0.0}
want_ui=${UI:-0}

# The engine and the UI release spell platforms differently (osx vs osx_arm64,
# linux-amd64 vs linux_amd64); carry both spellings per host.
case "$(uname -s)/$(uname -m)" in
  Darwin/*)                  duck_plat=osx;         ui_plat=osx_arm64   ;;
  Linux/x86_64)              duck_plat=linux-amd64; ui_plat=linux_amd64 ;;
  Linux/aarch64|Linux/arm64) duck_plat=linux-arm64; ui_plat=linux_arm64 ;;
  *) echo "fetch-duckdb: unsupported platform $(uname -s)/$(uname -m)" >&2; exit 2 ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/fetch-duckdb.XXXXXX")
trap 'rm -rf "$work"' EXIT
say()   { printf '  %s\n' "$*"; }
grab()  { find "$work" -type f -name "$1" 2>/dev/null | head -1; }
place() { local s; s=$(grab "$1") || true; [ -n "$s" ] && install -m "$2" "$s" "$dest/$1" && say "-> $dest/$1"; return 0; }

# ---- the engine: one "binaries" zip, two sub-zips nested inside -------------
if [ "$want_ui" = 1 ]; then
  version=${DUCKDB_VERSION:-v2.0.0-alpha37626}       # the pinned matched pair
  engine_url="https://github.com/shreeve/duckdb-harbor/releases/download/duckdb-$version/duckdb-$version-binaries-$duck_plat.zip"
else
  engine_url=${ENGINE_URL:-https://artifacts.duckdb.org/latest/duckdb-binaries-$duck_plat.zip}
fi

say "fetching $engine_url"
curl -fsSL -o "$work/binaries.zip" "$engine_url"
( cd "$work" && unzip -oq binaries.zip )        # -> libduckdb-*.zip, duckdb_cli-*.zip
( cd "$work" && unzip -oq 'libduckdb-*.zip' )   # -> libduckdb.{dylib,so} (+ headers)
( cd "$work" && unzip -oq 'duckdb_cli-*.zip' )  # -> duckdb CLI

mkdir -p "$dest"
place libduckdb.dylib    0755
place libduckdb.so       0755
place duckdb             0755
place duckdb.h           0644
place duckdb_extension.h 0644

# ---- optional: the UI extension, from the same pinned release --------------
if [ "$want_ui" = 1 ]; then
  ui_asset="ui-duckdb-$version-$ui_plat.zip"
  say "fetching $ui_asset"
  curl -fsSL -o "$work/ui.zip" \
    "https://github.com/shreeve/duckdb-ui/releases/download/ui-$version/$ui_asset"
  ( cd "$work" && unzip -oq ui.zip )
  ext=$(grab 'ui.duckdb_extension') || true
  [ -n "$ext" ] && install -m 0644 "$ext" "$dest/ui.duckdb_extension" \
    && say "-> $dest/ui.duckdb_extension"
fi

# ---- point cli/latest at what we just refreshed ----------------------------
# Only when we filled the canonical dir — a throwaway DEST elsewhere (a scratch
# test, a one-off build root) has no business owning `latest`.
if [ "$dest" = "$HOME/.duckdb/cli/2.0.0" ]; then
  ln -sfn "$dest" "$HOME/.duckdb/cli/latest"
  say "cli/latest -> $dest"
fi
echo "fetch-duckdb: ready in $dest"
