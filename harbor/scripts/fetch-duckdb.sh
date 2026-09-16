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
# got exports the v2 C API, because for a month in 2026 the channel shipped
# one that did not, and harbor refuses such an engine at dlopen.
#
# Override DEST to install elsewhere.

set -euo pipefail

dest=${DEST:-$HOME/.duckdb/cli/2.0.0}
channel=${DUCKDB_CHANNEL:-v2.0-cyanoptera}

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

# ---- can this engine actually serve harbor? --------------------------------
# harbor binds the v2 C API. grep the dynamic symbol names straight out of
# the binary — present on every platform, no nm/objdump dependency — and
# refuse a library without them: harbor would refuse it at dlopen anyway,
# later and less clearly.
for f in "$dest"/libduckdb.dylib "$dest"/libduckdb.so "$dest"/duckdb.dll; do
  [ -f "$f" ] || continue
  if ! grep -q duckdb_v2_connect "$f" 2>/dev/null; then
    echo "fetch-duckdb: $f exports no v2 C API symbols — harbor cannot serve with it." >&2
    echo "  The $channel channel shipped a pre-v2 build; try again later, or another DUCKDB_CHANNEL." >&2
    exit 1
  fi
  break
done
echo "fetch-duckdb: ready in $dest"
