#!/usr/bin/env bash
#
# package-release.sh — assemble one platform's release tarball.
#
# A release archive is self-contained and version-locked by construction:
# harbor + pilot (built with a RELATIVE rpath: @loader_path/../lib on macOS,
# $ORIGIN/../lib on Linux), the exact libduckdb they were built against, and
# the ui extension compiled against that same engine. Extract anywhere and
# bin/harbor runs in place; install.sh copies the pieces into their homes.
#
#   TAG=v0.11.0 PLAT=osx-arm64 UI_EXT=path/to/ui.duckdb_extension \
#     scripts/package-release.sh
#
# Produces $OUT/harbor-$TAG-$PLAT.tar.gz (OUT defaults to dist/).

set -euo pipefail

TAG=${TAG:?package-release: set TAG (e.g. v0.11.0)}
PLAT=${PLAT:?package-release: set PLAT (osx-arm64 | linux-amd64 | linux-arm64)}
UI_EXT=${UI_EXT:?package-release: set UI_EXT (path to built ui.duckdb_extension)}
DUCKDB_LIB=${DUCKDB_LIB:-$HOME/.duckdb/cli/2.0.0}
OUT=${OUT:-dist}
say() { printf '  %s\n' "$*"; }

[ -f "$UI_EXT" ] || { echo "package-release: no ui extension at $UI_EXT" >&2; exit 2; }
[ -f target/release/harbor ] && [ -f target/release/pilot ] \
  || { echo "package-release: build harbor + pilot first" >&2; exit 2; }

# The engine's identity, recorded in the archive so install.sh knows which
# extensions/<version>/<platform>/ directory the ui extension belongs in.
version=$("$DUCKDB_LIB/duckdb" -no-init -csv -noheader -c "PRAGMA version" | cut -d, -f1)
ext_plat=$("$DUCKDB_LIB/duckdb" -no-init -csv -noheader -c "PRAGMA platform")

name="harbor-$TAG-$PLAT"
root="$OUT/$name"
rm -rf "$root"; mkdir -p "$root/bin" "$root/lib" "$root/extensions"

install -m 0755 target/release/harbor "$root/bin/harbor"
install -m 0755 target/release/pilot  "$root/bin/pilot"
for lib in libduckdb.dylib libduckdb.so; do
  [ -f "$DUCKDB_LIB/$lib" ] && install -m 0755 "$DUCKDB_LIB/$lib" "$root/lib/$lib"
done
install -m 0644 "$UI_EXT" "$root/extensions/ui.duckdb_extension"
printf '%s %s\n' "$version" "$ext_plat" > "$root/ENGINE"
install -m 0755 scripts/release-install.sh "$root/install.sh"

tar -C "$OUT" -czf "$OUT/$name.tar.gz" "$name"
say "engine $version ($ext_plat)"
say "-> $OUT/$name.tar.gz ($(du -h "$OUT/$name.tar.gz" | cut -f1))"
