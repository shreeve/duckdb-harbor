#!/usr/bin/env bash
#
# appcast.sh — sign the update archives in <updates-dir> and (re)write its
# appcast.xml, the Sparkle feed the app reads from the `ducktable-updates`
# GitHub release (docs/UPDATES.md).
#
#   scripts/appcast.sh <updates-dir>
#
# <updates-dir> holds DuckTable-<version>.zip for the new release plus any
# older archives already on the feed release, so Sparkle can build binary
# deltas against them and keep their items. Every enclosure URL is the feed
# release's download URL for that file name.
#
# The private EdDSA key comes from SPARKLE_PRIVATE_KEY (CI, fed on stdin so
# it never lands on disk), otherwise from the login keychain account
# "ducktable" that `generate_keys --account ducktable` created.

set -euo pipefail
cd "$(dirname "$0")/.."

updates_dir="${1:?usage: scripts/appcast.sh <updates-dir>}"
[ -d "$updates_dir" ] || { echo "error: $updates_dir is not a directory" >&2; exit 1; }

. scripts/sparkle.sh
ensure_sparkle
generator="$sparkle_dir/bin/generate_appcast"

feed_prefix="https://github.com/shreeve/duckdb-harbor/releases/download/ducktable-updates/"
args=(
    --download-url-prefix "$feed_prefix"
    --link "https://github.com/shreeve/duckdb-harbor"
    --maximum-deltas 2
)

if [ -n "${SPARKLE_PRIVATE_KEY:-}" ]; then
    printf '%s\n' "$SPARKLE_PRIVATE_KEY" | "$generator" "${args[@]}" --ed-key-file - "$updates_dir"
else
    "$generator" "${args[@]}" --account ducktable "$updates_dir"
fi

# generate_appcast exits 0 after writing an *unsigned* feed when its key does
# not match the bundle's SUPublicEDKey, and Sparkle rejects an unsigned
# enclosure — so a silent mismatch would ship a dead update feed.
appcast="$updates_dir/appcast.xml"
unsigned=$(grep -o '<enclosure[^>]*>' "$appcast" | grep -v 'sparkle:edSignature=' || true)
if [ -n "$unsigned" ]; then
    echo "error: appcast has unsigned enclosures; the signing key does not match SUPublicEDKey:" >&2
    echo "$unsigned" >&2
    exit 1
fi
echo "Wrote $appcast"
