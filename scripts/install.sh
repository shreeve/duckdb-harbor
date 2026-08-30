#!/bin/sh
# Installs DuckTable.app from the latest GitHub release — no signing, no
# ceremony. curl never sets macOS's quarantine attribute, so the app opens
# on first launch, and the binary already carries cargo's ad-hoc signature.
#
#   curl -fsSL https://raw.githubusercontent.com/shreeve/ducktable/main/scripts/install.sh | sh
set -e

[ "$(uname -s)" = "Darwin" ] || { echo "DuckTable is a macOS app." >&2; exit 1; }
[ "$(uname -m)" = "arm64" ] || {
    echo "The prebuilt DuckTable is Apple Silicon only (this Mac is $(uname -m))." >&2
    echo "Intel: clone the repo and run scripts/macos-app.sh release." >&2
    exit 1
}

# The release asset keeps one name, so "latest" is a stable URL forever.
url="https://github.com/shreeve/ducktable/releases/latest/download/DuckTable.zip"
dest="/Applications"
[ -w "$dest" ] || dest="$HOME/Applications"
mkdir -p "$dest"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading DuckTable..."
curl -fSL --progress-bar "$url" -o "$tmp/DuckTable.zip"
# ditto, not unzip: it preserves the bundle exactly as it was archived.
ditto -x -k "$tmp/DuckTable.zip" "$tmp"
rm -rf "$dest/DuckTable.app"
mv "$tmp/DuckTable.app" "$dest/"
# Belt and suspenders: if anything tagged the download, untag the install.
xattr -dr com.apple.quarantine "$dest/DuckTable.app" 2>/dev/null || true

echo "Installed $dest/DuckTable.app"
echo "DuckTable speaks to DuckDB Harbor: https://github.com/shreeve/duckdb-harbor"
