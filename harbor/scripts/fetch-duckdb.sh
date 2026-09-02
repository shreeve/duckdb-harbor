#!/usr/bin/env bash
#
# fetch-duckdb.sh — put a DuckDB engine into ~/.duckdb/cli/2.0.0/
#
# harbor carries no engine; this fetches one for it to load, along
# with the two headers (kept for reference — the crate ships pregenerated
# bindings, so the build never reads them) and the duckdb CLI that builds
# fixtures. The source is DuckDB's official artifact channel at
# artifacts.duckdb.org.
#
# CAVEAT, until DuckDB 2.0 GA: that channel is frozen at alpha38195
# (2026-08-18), which predates the v2 C API landing on DuckDB main — the
# libduckdb it delivers exports ZERO v2 symbols, and harbor 0.21 (which
# binds the v2 C API) will refuse it with "engine has no v2 C API". Until
# GA publishes v2-capable binaries, set ENGINE_URL to our own shelf — the
# Engine workflow builds all five platforms at CI's pinned commit and
# shelves them on this repo's engine-<pin> prerelease, in the official
# channel's exact zip shape:
#
#   ENGINE_URL=https://github.com/shreeve/duckdb-harbor/releases/download/engine-1582849bf9/duckdb-binaries-<plat>.zip
#
# (or build from source yourself — recipe in .github/actions/duckdb/
# action.yml). The CLI from the frozen zip is still fine: it only builds
# fixtures and needs no v2 API. This script warns loudly when the fetched
# library cannot serve. At GA, delete the caveat and the warning below.
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
# harbor 0.21 binds the v2 C API; the frozen artifact channel predates it.
# grep the dynamic symbol names straight out of the binary — present on every
# platform, no nm/objdump dependency. Delete this check at GA.
for f in "$dest"/libduckdb.dylib "$dest"/libduckdb.so "$dest"/duckdb.dll; do
  [ -f "$f" ] || continue
  if ! grep -q duckdb_v2_connect "$f" 2>/dev/null; then
    echo "" >&2
    echo "fetch-duckdb: WARNING — this libduckdb exports no v2 C API symbols." >&2
    echo "  harbor cannot serve with it (the official channel is frozen" >&2
    echo "  pre-v2-API until DuckDB 2.0 GA). Re-run with ENGINE_URL pointed" >&2
    echo "  at this repo's engine-<pin> release — the exact line is in this" >&2
    echo "  script's header." >&2
  fi
  break
done
echo "fetch-duckdb: ready in $dest"
