#!/bin/bash
#
# update-formula.sh — point the Homebrew formula at a published release.
#
#   scripts/update-formula.sh 0.43.5
#
# Rewrites the three archive URLs and their sha256s in Formula/duckdb-harbor.rb
# in the shreeve/homebrew-tap checkout (TAP names another; the default is the
# tap beside this repository), from the release's own checksums file. Commits
# on a branch duckdb-harbor-<version> from the tap's main, pushes, and opens
# the pull request. Merging it is the last step of a release:
# `brew upgrade duckdb-harbor` then installs the version.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
fail() { echo "error: $*" >&2; exit 1; }

version="${1:?usage: scripts/update-formula.sh <version>}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "version must look like 1.2.3, not $version"
repo="shreeve/duckdb-harbor"
tap="${TAP:-$root/../../homebrew-tap}"
formula="Formula/duckdb-harbor.rb"
[ -f "$tap/$formula" ] || fail "no formula at $tap/$formula (git clone https://github.com/shreeve/homebrew-tap there, or set TAP)"

# The sums as published, so each matches what Homebrew downloads.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
sums="$tmp/checksums.txt"
curl -fsSL -o "$sums" "https://github.com/$repo/releases/download/v$version/harbor-v$version-checksums.txt" \
  || fail "cannot download the checksums; is v$version published?"

# A fresh branch from the tap's main first, so the formula is written there and nowhere else.
cd "$tap"
[ -z "$(git status --porcelain --untracked-files=no)" ] || fail "the tap checkout has uncommitted changes"
git fetch -q origin main
branch="duckdb-harbor-$version"
git checkout -q -B "$branch" origin/main

for plat in osx-arm64 linux-arm64 linux-amd64; do
  sha=$(awk -v f="harbor-v$version-$plat.tar.gz" '$2 == f { print $1 }' "$sums")
  [ -n "$sha" ] || fail "no checksum published for $plat"
  # The url line for this platform, and the sha256 line that follows it.
  PLAT="$plat" VERSION="$version" SHA="$sha" perl -0pi -e '
    my ($p, $v, $s) = @ENV{qw(PLAT VERSION SHA)};
    s{(releases/download/)v[0-9.]+(/harbor-)v[0-9.]+(-\Q$p\E\.tar\.gz"\s*\n\s*sha256 ")[0-9a-f]{64}}{$1v$v$2v$v$3$s}
      or die "no url and sha256 for $p in the formula\n";
  ' "$formula"
done

if git diff --quiet -- "$formula"; then
  git checkout -q main 2>/dev/null || true
  echo "the formula already points at $version"
  exit 0
fi
git add "$formula"
git commit -q -m "Update duckdb-harbor to $version"
git push -q -u origin "$branch"
gh pr create --repo shreeve/homebrew-tap --base main --head "$branch" \
  --title "Update duckdb-harbor to $version" \
  --body "Points the formula at harbor $version: the three archive URLs and their sha256s, from the release's checksums file."
