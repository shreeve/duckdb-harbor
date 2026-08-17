#!/usr/bin/env bash
#
# fetch-duckdb.sh — pull one synchronized DuckDB engine into ~/.duckdb/cli/<ver>/
#
# harbor carries no engine: it is dynamically linked and resolves whichever
# libduckdb sits beside it at runtime (PLAN.md D1). This fetches that engine —
# the library harbor links, the two headers (kept for reference; the crate ships
# pregenerated bindings, so the build never reads them), and the matching duckdb
# CLI that builds test fixtures — from ONE release, so the three are the same
# build and cannot drift.
#
# Called by `make fetch-duckdb`. Run it directly to override any of it:
#   DUCKDB_VERSION=v2.0.0-alpha37626 DEST=~/.duckdb/cli/2.0.0 scripts/fetch-duckdb.sh
#   UI=1 scripts/fetch-duckdb.sh        # also drop the matching ui.duckdb_extension
#
# The only coupling to the release is the asset naming, spelled out in the two
# case arms below — change the names on the release and change them here together.
# Source of truth is currently our own 2.0 build on the duckdb-harbor fork; when
# the duckdb-ui factory pipeline emits the same libduckdb, point RELEASE_BASE at
# it — the sub-zip layout is identical, so nothing else here moves.

set -euo pipefail

version=${DUCKDB_VERSION:-v2.0.0-alpha37626}
# ~/.duckdb/cli/2.0.0 by default: the tag's marketing version with the leading
# v and the -alpha<seq> suffix stripped, which is how the cli/<ver> dirs already
# read. Override DEST to put it anywhere.
short=$(printf '%s' "$version" | sed -E 's/^v//; s/-alpha[0-9]+$//')
dest=${DEST:-$HOME/.duckdb/cli/$short}
release_base=${RELEASE_BASE:-https://github.com/shreeve/duckdb-harbor/releases/download}
ui_base=${UI_RELEASE_BASE:-https://github.com/shreeve/duckdb-ui/releases/download}
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
say()  { printf '  %s\n' "$*"; }
grab() { find "$work" -type f -name "$1" 2>/dev/null | head -1; }
place() { # place <name-in-archive> <mode>
  local src; src=$(grab "$1")
  [ -n "$src" ] || return 0
  install -m "$2" "$src" "$dest/$1"; say "-> $dest/$1"
}

# ---- the engine: one binaries zip, two sub-zips nested inside it -----------
tag="duckdb-$version"
asset="duckdb-$version-binaries-$duck_plat.zip"
say "fetching $asset"
curl -fsSL -o "$work/binaries.zip" "$release_base/$tag/$asset"
( cd "$work" && unzip -oq binaries.zip )        # -> libduckdb-*.zip, duckdb_cli-*.zip
( cd "$work" && unzip -oq 'libduckdb-*.zip' )   # -> libduckdb.{dylib,so} (+ headers)
( cd "$work" && unzip -oq 'duckdb_cli-*.zip' )  # -> duckdb CLI

mkdir -p "$dest"
place libduckdb.dylib      0755
place libduckdb.so         0755
place duckdb               0755
place duckdb.h             0644
place duckdb_extension.h   0644

# ---- optional: the UI extension, from its own (same-build) release ---------
if [ "$want_ui" = 1 ]; then
  ui_asset="ui-duckdb-$version-$ui_plat.zip"
  say "fetching $ui_asset"
  curl -fsSL -o "$work/ui.zip" "$ui_base/ui-$version/$ui_asset"
  ( cd "$work" && unzip -oq ui.zip )
  ext=$(grab 'ui.duckdb_extension')
  [ -n "$ext" ] && install -m 0644 "$ext" "$dest/ui.duckdb_extension" \
    && say "-> $dest/ui.duckdb_extension"
fi

# ---- point cli/latest at what we just refreshed ----------------------------
# Only when we filled a dir under ~/.duckdb/cli — a throwaway DEST elsewhere
# (a scratch test, a one-off build root) has no business owning `latest`.
if [ "$dest" = "$HOME/.duckdb/cli/$short" ]; then
  ln -sfn "$dest" "$HOME/.duckdb/cli/latest"
  say "cli/latest -> $dest"
fi
echo "fetch-duckdb: $version ready in $dest"
