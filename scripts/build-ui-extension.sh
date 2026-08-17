#!/usr/bin/env bash
#
# build-ui-extension.sh — build the DuckDB UI extension against the EXACT engine
# harbor runs, and install it into DuckDB's extension directory
# (~/.duckdb/extensions/<version>/<platform>/) so `LOAD ui` resolves it by name,
# in harbor and the plain duckdb CLI alike.
#
# This is the payoff of the dylib approach: harbor, libduckdb, and the UI
# extension all derive from ONE nightly, so the version lock the C++ ABI demands
# is satisfied by construction. No full-engine compile — we fetch the matching
# DuckDB *headers* at the engine's exact commit and compile only the ~11 UI
# sources, linking dynamically (symbols resolve from harbor's in-process
# libduckdb at load). Seconds, not the 20–40 min a from-source engine build costs.
#
#   make ui                     # build against ~/.duckdb/cli/2.0.0
#   DUCKDB_UI_DIR=... make ui    # point at a different UI checkout
#
# Requires: the UI source (a duckdb-ui checkout with the v2 fixes — our fork
# until duckdb/duckdb-ui#242 lands), a C++ compiler, git, gh, and OpenSSL.

set -euo pipefail

DUCKDB_LIB=${DUCKDB_LIB:-$HOME/.duckdb/cli/2.0.0}
UI_DIR=${DUCKDB_UI_DIR:-$HOME/Data/Code/duckdb-ui}
CACHE=${DUCKDB_SRC_CACHE:-$HOME/.cache/duckdb-src}
CXX=${CXX:-clang++}
say() { printf '  %s\n' "$*"; }

[ -x "$DUCKDB_LIB/duckdb" ] || { echo "build-ui: no duckdb CLI at $DUCKDB_LIB (run make fetch-duckdb)" >&2; exit 2; }
[ -d "$UI_DIR/src" ]        || { echo "build-ui: no UI source at $UI_DIR (set DUCKDB_UI_DIR)" >&2; exit 2; }

# 1. the engine's exact version + commit — everything synchronizes off this
read -r version source_id _ < <("$DUCKDB_LIB/duckdb" -no-init -csv -noheader -c "PRAGMA version" | tr ',' ' ')
say "engine: $version ($source_id)"
sha=$(gh api "repos/duckdb/duckdb/commits/$source_id" --jq .sha 2>/dev/null) \
  || { echo "build-ui: could not resolve $source_id to a full SHA" >&2; exit 3; }

# Platform also picks the link model: macOS bundles leave DuckDB symbols
# unresolved via -undefined dynamic_lookup; Linux shared objects allow
# unresolved symbols by default. Both resolve from the host's libduckdb at load.
case "$(uname -s)/$(uname -m)" in
  Darwin/*)                  plat=osx_arm64;   ldflags=(-bundle -undefined dynamic_lookup)
                             OPENSSL=${OPENSSL:-$(brew --prefix openssl@3 2>/dev/null || echo /opt/homebrew/opt/openssl@3)} ;;
  Linux/x86_64)              plat=linux_amd64; ldflags=(-shared); OPENSSL=${OPENSSL:-} ;;
  Linux/aarch64|Linux/arm64) plat=linux_arm64; ldflags=(-shared); OPENSSL=${OPENSSL:-} ;;
  *) echo "build-ui: unsupported platform" >&2; exit 2 ;;
esac

# DuckDB resolves `LOAD ui` by name from extensions/<version>/<platform>/, so
# install there. Keyed by the exact nightly, matching the engine it was built
# against; a version bump makes a new dir (old ones are safe to delete).
OUT=${OUT:-$HOME/.duckdb/extensions/$version/$plat/ui.duckdb_extension}
mkdir -p "$(dirname "$OUT")"

# 2. DuckDB headers at that exact commit (cached by SHA, fetched once)
ddb="$CACHE/$sha"
if [ ! -d "$ddb/src/include" ]; then
  say "fetching DuckDB headers @ ${sha:0:10} (one-time)"
  rm -rf "$ddb"; mkdir -p "$ddb"
  ( cd "$ddb" && git init -q && git remote add origin https://github.com/duckdb/duckdb.git \
      && git fetch -q --depth 1 origin "$sha" && git checkout -q FETCH_HEAD )
else
  say "headers cached: $ddb"
fi

# 3. include dirs — DuckDB's curated extension include set, exactly what its
#    build exports (NOT all of third_party: globbing pulls in brotli, whose
#    `include/version` file shadows the C++ standard <version> header). This set
#    is stable across nightlies; add here only if a future engine needs it.
tp="$ddb/third_party"
incs=(
  -I"$ddb/src/include"
  -I"$tp/fsst" -I"$tp/fmt/include" -I"$tp/hyperloglog" -I"$tp/fastpforlib"
  -I"$tp/skiplist" -I"$tp/ska_sort" -I"$tp/fast_float" -I"$tp/re2"
  -I"$tp/miniz" -I"$tp/utf8proc/include" -I"$tp/concurrentqueue" -I"$tp/pcg"
  -I"$tp/pdqsort" -I"$tp/tdigest" -I"$tp/mbedtls/include" -I"$tp/httplib"
  -I"$tp/jaro_winkler" -I"$tp/vergesort" -I"$tp/yyjson/include" -I"$tp/zstd/include"
  -I"$UI_DIR/src/include" -I"$UI_DIR/third_party/httplib"
)
# OpenSSL powers the UI's HTTPS *client* (it proxies the front-end from
# ui.duckdb.org). macOS points at brew's; Linux finds the system one.
[ -n "$OPENSSL" ] && incs+=(-isystem "$OPENSSL/include") && sslflags=(-L"$OPENSSL/lib") || sslflags=()

# 4. version stamps the source expects (cosmetic — reported by ui_version())
IFS=. read -r maj min pat <<<"${version#v}"; pat=${pat%%-*}
seq=$(git -C "$UI_DIR" rev-list --count HEAD 2>/dev/null || echo 0)
gsha=$(git -C "$UI_DIR" rev-parse --short=10 HEAD 2>/dev/null || echo unknown)
defs=(-DDUCKDB_BUILD_LOADABLE_EXTENSION
      -DDUCKDB_MAJOR_VERSION="$maj" -DDUCKDB_MINOR_VERSION="$min" -DDUCKDB_PATCH_VERSION="${pat:-0}"
      -DUI_EXTENSION_GIT_SHA="\"$gsha\"" -DUI_EXTENSION_SEQ_NUM="\"$seq\"" -DEXT_VERSION_UI="\"$gsha\"" -DNDEBUG)

# 5. compile the ~11 UI sources in parallel, link dynamically
work=$(mktemp -d "${TMPDIR:-/tmp}/build-ui.XXXXXX"); trap 'rm -rf "$work"' EXIT
# bash 3.2 (macOS default) has no mapfile — read the list the portable way.
srcs=(); while IFS= read -r f; do srcs+=("$f"); done < <(find "$UI_DIR/src" -name '*.cpp' | sort)
say "compiling ${#srcs[@]} sources"
pids=(); objs=()
for src in "${srcs[@]}"; do
  obj="$work/$(basename "$src").o"; objs+=("$obj")
  "$CXX" -std=c++17 -O2 -fPIC -fvisibility=hidden "${defs[@]}" "${incs[@]}" -c "$src" -o "$obj" &
  pids+=($!)
done
fail=0; for p in "${pids[@]}"; do wait "$p" || fail=1; done
[ "$fail" = 0 ] || { echo "build-ui: a source failed to compile against $version" >&2; exit 4; }

raw="$work/ui.raw"
# bash 3.2 + set -u: an empty sslflags array can't expand bare, hence the idiom.
"$CXX" "${ldflags[@]}" -o "$raw" "${objs[@]}" ${sslflags[@]+"${sslflags[@]}"} -lssl -lcrypto
say "linked ($(du -h "$raw" | cut -f1)) — links no engine"

# 6. stamp DuckDB extension metadata, matched to the running engine
python3 "$UI_DIR/extension-ci-tools/scripts/append_extension_metadata.py" \
  -l "$raw" -n ui -dv "$version" -p "$plat" --abi-type CPP -ev "$gsha" -o "$OUT" >/dev/null
say "-> $OUT"
echo "build-ui: ui.duckdb_extension ready for $version"
