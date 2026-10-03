#!/bin/bash
#
# update-cask.sh — point the Homebrew cask at a published release.
#
#   scripts/update-cask.sh 0.22.8
#
# Writes Casks/ducktable.rb in the shreeve/homebrew-tap checkout (TAP names another; the default
# is the tap beside this repository) with the release's archive and its sha256, commits it on a
# branch ducktable-<version> from the tap's main, pushes, and opens the pull request. Merging it
# is the last step of a release; `brew install --cask shreeve/tap/ducktable` then installs the
# version.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
fail() { echo "error: $*" >&2; exit 1; }

version="${1:?usage: scripts/update-cask.sh <version>}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "version must look like 1.2.3, not $version"
repo="shreeve/duckdb-harbor"
tap="${TAP:-$root/../../homebrew-tap}"
[ -d "$tap/Casks" ] || fail "no tap checkout at $tap (git clone https://github.com/shreeve/homebrew-tap there, or set TAP)"
url="https://github.com/$repo/releases/download/ducktable-v$version/DuckTable-$version.zip"

# The archive as published, so the sum matches what Homebrew downloads.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/DuckTable.zip" "$url" || fail "cannot download $url; is ducktable-v$version published?"
sha=$(shasum -a 256 "$tmp/DuckTable.zip" | cut -d' ' -f1)

# A fresh branch from the tap's main first, so the cask is written there and nowhere else.
cd "$tap"
[ -z "$(git status --porcelain --untracked-files=no)" ] || fail "the tap checkout has uncommitted changes"
git fetch -q origin main
branch="ducktable-$version"
git checkout -q -B "$branch" origin/main

cask="Casks/ducktable.rb"
new=1; [ ! -f "$cask" ] || new=0
cat > "$cask" <<CASK
cask "ducktable" do
  version "$version"
  sha256 "$sha"

  url "https://github.com/$repo/releases/download/ducktable-v#{version}/DuckTable-#{version}.zip"
  name "DuckTable"
  desc "Fast, minimal desktop client for DuckDB Harbor servers"
  homepage "https://github.com/$repo/tree/main/ducktable"

  livecheck do
    url "https://github.com/$repo/releases/download/ducktable-updates/appcast.xml"
    strategy :sparkle
  end

  auto_updates true
  depends_on arch: :arm64
  depends_on macos: :monterey

  app "DuckTable.app"

  zap trash: [
    "~/.config/ducktable",
    "~/Library/Caches/com.shreeve.ducktable",
    "~/Library/HTTPStorages/com.shreeve.ducktable",
    "~/Library/Preferences/com.shreeve.ducktable.plist",
  ]

  caveats <<~EOS
    DuckTable speaks to DuckDB Harbor servers. Install harbor with:
      brew install shreeve/tap/duckdb-harbor
    or with its one-line installer:
      curl -fsSL https://raw.githubusercontent.com/$repo/main/install.sh | bash
  EOS
end
CASK

git add "$cask"
if [ "$new" = 1 ]; then title="Add ducktable $version"; else title="Update ducktable to $version"; fi
git commit -q -m "$title"
git push -q -u origin "$branch"
gh pr create --title "$title" --body "Points the cask at https://github.com/$repo/releases/tag/ducktable-v$version." | tail -1
git checkout -q main
