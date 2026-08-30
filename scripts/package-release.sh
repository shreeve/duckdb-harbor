#!/usr/bin/env bash
#
# package-release.sh — assemble one platform's release tarball.
#
# A release archive is self-contained and version-locked by construction:
# harbor + pilot (built with a RELATIVE rpath: @loader_path/../lib on macOS,
# $ORIGIN/../lib on Linux) and the exact libduckdb they were built against.
# Windows puts duckdb.dll beside the executables. Extract anywhere and run
# in place;
# install.sh copies the Unix pieces into their homes.
#
#   TAG=v0.15.0 PLAT=osx-arm64 scripts/package-release.sh
#
# Produces $OUT/harbor-$TAG-$PLAT.tar.gz (OUT defaults to dist/).

set -euo pipefail

TAG=${TAG:?package-release: set TAG (e.g. v0.11.0)}
PLAT=${PLAT:?package-release: set PLAT (osx-arm64 | linux-amd64 | linux-arm64 | windows-amd64 | windows-arm64)}
DUCKDB_LIB=${DUCKDB_LIB:-$HOME/.duckdb/cli/2.0.0}
OUT=${OUT:-dist}
say() { printf '  %s\n' "$*"; }

if [[ "$PLAT" == windows-* ]]; then
  [ -f target/release/harbor.exe ] && [ -f target/release/pilot.exe ] \
    || { echo "package-release: build harbor + pilot first" >&2; exit 2; }
  [ -f "$DUCKDB_LIB/duckdb.dll" ] \
    || { echo "package-release: no duckdb.dll in $DUCKDB_LIB" >&2; exit 2; }

  name="harbor-$TAG-$PLAT"
  root="$OUT/$name"
  rm -rf "$root"; mkdir -p "$root/bin"
  install -m 0755 target/release/harbor.exe "$root/bin/harbor.exe"
  install -m 0755 target/release/pilot.exe  "$root/bin/pilot.exe"
  install -m 0755 "$DUCKDB_LIB/duckdb.dll" "$root/bin/duckdb.dll"
  install -m 0644 scripts/release-install.ps1 "$root/install.ps1"
  install -m 0644 config.toml.example "$root/config.toml.example"
  ( cd "$OUT" && 7z a -tzip "$name.zip" "$name" >/dev/null )
  say "-> $OUT/$name.zip ($(du -h "$OUT/$name.zip" | cut -f1))"
  exit 0
fi

[ -f target/release/harbor ] && [ -f target/release/pilot ] \
  || { echo "package-release: build harbor + pilot first" >&2; exit 2; }

name="harbor-$TAG-$PLAT"
root="$OUT/$name"
rm -rf "$root"; mkdir -p "$root/bin" "$root/lib"

install -m 0755 target/release/harbor "$root/bin/harbor"
install -m 0755 target/release/pilot  "$root/bin/pilot"
for lib in libduckdb.dylib libduckdb.so; do
  [ -f "$DUCKDB_LIB/$lib" ] && install -m 0755 "$DUCKDB_LIB/$lib" "$root/lib/$lib"
done
install -m 0755 scripts/release-install.sh "$root/install.sh"
# Reference only — never copied over an operator's live config.
install -m 0644 config.toml.example "$root/config.toml.example"

tar -C "$OUT" -czf "$OUT/$name.tar.gz" "$name"
say "-> $OUT/$name.tar.gz ($(du -h "$OUT/$name.tar.gz" | cut -f1))"
