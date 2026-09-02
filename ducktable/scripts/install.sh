#!/bin/sh
# Installs DuckTable.app from the newest ducktable-v* GitHub release — no
# signing, no ceremony. curl never sets macOS's quarantine attribute, so the
# app opens on first launch, and the binary already carries cargo's ad-hoc
# signature.
#
#   curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/ducktable/scripts/install.sh | bash
set -e

# Color only when stdout is a terminal, and never against NO_COLOR.
Color_Off='' Red='' Green='' Dim='' Bold_Green='' Bold_White=''
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    Color_Off='\033[0m'
    Red='\033[0;31m' Green='\033[0;32m' Dim='\033[0;2m'
    Bold_Green='\033[1;32m' Bold_White='\033[1m'
fi

info() { printf "${Dim}%s${Color_Off}\n" "$*"; }
fail() { printf "${Red}error${Color_Off}: %s\n" "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || fail "DuckTable is a macOS app."
[ "$(uname -m)" = "arm64" ] || {
    printf "${Red}error${Color_Off}: %s\n" "The prebuilt DuckTable is Apple Silicon only (this Mac is $(uname -m))." >&2
    printf "${Dim}%s${Color_Off}\n" "Intel: clone the repo and run scripts/macos-app.sh release." >&2
    exit 1
}

# DuckTable shares its repo with harbor, and the repo's "latest" release is
# harbor's — so resolve the newest ducktable-v* tag by name. The asset keeps
# one name, so the tag is all that varies.
tag=$(curl -fsSL "https://api.github.com/repos/shreeve/duckdb-harbor/releases?per_page=50" \
      | grep -o '"tag_name": *"ducktable-v[^"]*"' | head -1 | cut -d'"' -f4)
[ -n "$tag" ] || fail "could not find a ducktable-v* release"
url="https://github.com/shreeve/duckdb-harbor/releases/download/$tag/DuckTable.zip"
dest="/Applications"
[ -w "$dest" ] || dest="$HOME/Applications"
mkdir -p "$dest"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
info "DuckTable ($tag, Apple Silicon)"
curl -fSL --progress-bar "$url" -o "$tmp/DuckTable.zip"
# ditto, not unzip: it preserves the bundle exactly as it was archived.
ditto -x -k "$tmp/DuckTable.zip" "$tmp"
rm -rf "$dest/DuckTable.app"
mv "$tmp/DuckTable.app" "$dest/"
# Belt and suspenders: if anything tagged the download, untag the install.
xattr -dr com.apple.quarantine "$dest/DuckTable.app" 2>/dev/null || true

case "$dest" in
    "$HOME"/*) shown="~${dest#"$HOME"}/DuckTable.app" ;;
    *)         shown="$dest/DuckTable.app" ;;
esac
printf "${Green}DuckTable was installed successfully to ${Bold_Green}%s${Color_Off}\n" "$shown"
info "Run 'open -a DuckTable' to get started"
info "DuckTable speaks to DuckDB Harbor: https://github.com/shreeve/duckdb-harbor"
