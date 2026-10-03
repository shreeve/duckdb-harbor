#!/bin/bash
#
# release.sh — build DuckTable <version>, notarize it, sign its update feed, and publish it as a
# GitHub release.
#
#   scripts/release.sh 0.22.8 --notes     # print the release notes it would publish; nothing else
#   scripts/release.sh 0.22.8 --dry-run   # build everything under target/release-0.22.8; publish nothing
#   scripts/release.sh 0.22.8             # also commit the version, tag ducktable-v0.22.8, push, and publish
#
# The versioned release carries DuckTable-<version>.zip, which the Homebrew cask downloads, and
# DuckTable.zip, which scripts/install.sh fetches. The bundle is signed with the Developer ID and
# notarized, with the ticket stapled, so Gatekeeper accepts it however it was downloaded. The
# update feed lives on one more release that never changes name, ducktable-updates
# (docs/UPDATES.md): the run adds the archive there and rewrites appcast.xml, signed with the
# EdDSA key the login keychain holds under the account "ducktable", the private half of
# assets/sparkle-public-key.txt.
#
# The notes are the version's section of CHANGELOG.md, which a release must have: the GitHub
# release shows them, and the feed embeds them for Sparkle's update dialog.
#
# The release commit, "DuckTable <version>", sets the workspace version and the lockfile that
# follows it; the changelog section and the Sparkle pin land before, on main. A failed run leaves
# the repo as it found it: Cargo.toml and Cargo.lock are put back, and until the push, the local
# commit, the tag, and the draft release are undone. Past the push, the script prints the command
# that finishes the step that failed. A real release refuses to start when a tag or release for
# the version already exists, or when Sparkle has a newer stable release than the pin.
#
# The versioned release is never --latest: the repository's /releases/latest belongs to harbor.
# Release harbor first when both ship, so the lockfile records harbor's new version.
#
# After publishing, scripts/update-cask.sh <version> opens the Homebrew tap's pull request.
#
# SIGN names another signing identity and NOTARY_PROFILE another notarytool keychain profile.

set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

fail() { echo "error: $*" >&2; exit 1; }
warn() { echo "warning: $*" >&2; }

[ $# -ge 1 ] || fail "usage: scripts/release.sh <version> [--dry-run | --notes]"
version="$1"
mode="${2:-publish}"
case "$mode" in publish | --dry-run | --notes) ;; *) fail "unknown option $mode" ;; esac
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "version must look like 1.2.3, not $version"
repo="shreeve/duckdb-harbor"
tag="ducktable-v$version"
feed_release="ducktable-updates"
identity="${SIGN:-Developer ID Application: Steve Shreeve (SD6N7Z8P9P)}"
profile="${NOTARY_PROFILE:-notary-tool}"
key_account=ducktable
out="$root/target/release-$version"

# The version's section of CHANGELOG.md, from under its "## <version> — <date>" heading to the
# next one.
section=$(awk -v v="$version" '
    /^## / { if (on) exit; on = ($2 == v); next }
    on { line[++n] = $0 }
    END {
        first = 1
        while (first <= n && line[first] == "") first++
        while (n >= first && line[n] == "") n--
        for (i = first; i <= n; i++) print line[i]
    }' CHANGELOG.md)
notes="$section

Install with Homebrew:

    brew install --cask shreeve/tap/ducktable

or with one command:

    curl -fsSL https://raw.githubusercontent.com/$repo/main/ducktable/scripts/install.sh | bash

Installed copies update themselves: choose DuckTable → Check for Updates…"

if [ "$mode" = --notes ]; then
    [ -n "$section" ] || fail "CHANGELOG.md has no section for $version"
    printf '%s\n' "$notes"
    exit 0
fi

# Whether version $1 is higher than version $2.
higher() {
    local IFS=. i
    local -a a=($1) b=($2)
    for i in 0 1 2; do
        ((10#${a[i]} > 10#${b[i]})) && return 0
        ((10#${a[i]} < 10#${b[i]})) && return 1
    done
    return 1
}

if [ "$mode" = publish ]; then
    [ "$(git branch --show-current)" = "main" ] || fail "release from main"
    [ -z "$(git status --porcelain)" ] || fail "the working tree is not clean"
    git fetch -q origin main --tags
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || fail "main is not in step with origin/main"
    ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || fail "$tag already exists"
    ! git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null || fail "$tag already exists on origin"
    gh auth status >/dev/null 2>&1 || fail "gh is not signed in to GitHub"
    ! gh release view "$tag" --repo "$repo" >/dev/null 2>&1 \
        || fail "a release for $tag already exists; if a failed run left a draft, delete it: gh release delete $tag --repo $repo"
    # The cask, the installer and the feed download release files without signing in.
    [ "$(gh repo view "$repo" --json visibility --jq .visibility)" = "PUBLIC" ] \
        || fail "$repo is not public; Homebrew, the installer and Sparkle download releases anonymously"
fi
# Sparkle offers an update only when its CFBundleVersion is higher than the installed one's.
latest=""
for existing in $(git tag -l 'ducktable-v[0-9]*'); do
    existing="${existing#ducktable-v}"
    [[ "$existing" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || continue
    if [ -z "$latest" ] || higher "$existing" "$latest"; then latest="$existing"; fi
done
. scripts/sparkle.sh
stable=$(gh api repos/sparkle-project/Sparkle/releases/latest --jq .tag_name 2>/dev/null || true)
problems=()
[ -z "$latest" ] || higher "$version" "$latest" || problems+=("$version is not higher than the latest release, $latest")
[ -n "$section" ] || problems+=("CHANGELOG.md has no \"## $version\" section for the release notes")
[ -z "$stable" ] || [ "$stable" = "$sparkle_version" ] \
    || problems+=("Sparkle $stable is the latest stable; set sparkle_version and sparkle_sha256 in scripts/sparkle.sh and the path in docs/UPDATES.md")
for problem in ${problems[@]+"${problems[@]}"}; do
    if [ "$mode" = publish ]; then fail "$problem"; else warn "$problem"; fi
done

security find-identity -v -p codesigning | grep -qF "\"$identity\"" \
    || fail "the keychain has no signing identity \"$identity\""
xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1 \
    || fail "notarytool cannot sign in with keychain profile \"$profile\"; see docs/UPDATES.md"

# The feed must be signed with the private half of the key the app trusts.
ensure_sparkle
public_key=$(tr -d '[:space:]' < assets/sparkle-public-key.txt 2>/dev/null || true)
[ -n "$public_key" ] || fail "assets/sparkle-public-key.txt is missing, so no copy could ever update"
keychain_key=$("$sparkle_dir/bin/generate_keys" --account "$key_account" -p 2>/dev/null || true)
[ -n "$keychain_key" ] || fail "the login keychain has no Sparkle key under the account \"$key_account\"; see docs/UPDATES.md"
[ "$keychain_key" = "$public_key" ] || fail "the keychain's \"$key_account\" update key does not match assets/sparkle-public-key.txt"

# From here a failure undoes what the run did, as far as `stage` says it got.
start=$(git rev-parse HEAD)
backup=$(mktemp -d)
cp Cargo.toml Cargo.lock "$backup/"
stage=stamped
undo() {
    local status=$?
    case "$stage" in
        stamped | drafted | committed)
            if [ "$stage" != stamped ]; then
                gh release delete "$tag" --repo "$repo" --yes >/dev/null 2>&1 \
                    || echo "error: if a draft release $tag exists, delete it: gh release delete $tag --repo $repo" >&2
            fi
            if [ "$stage" = committed ]; then
                git tag -d "$tag" >/dev/null 2>&1 || true
                git reset -q --keep "$start" || echo "error: could not undo the commit \"DuckTable $version\"" >&2
            fi
            cp "$backup/Cargo.toml" "$backup/Cargo.lock" .
            ;;
        pushed)
            echo "error: $tag is pushed, but its release is still a draft; publish it, then the feed, with:" >&2
            echo "  gh release edit $tag --repo $repo --draft=false --latest=false --verify-tag" >&2
            echo "  find $out/feed -maxdepth 1 -type f -exec gh release upload $feed_release --repo $repo --clobber {} +" >&2
            ;;
        published)
            echo "error: $tag is published, but the update feed is not; finish it with:" >&2
            echo "  find $out/feed -maxdepth 1 -type f -exec gh release upload $feed_release --repo $repo --clobber {} +" >&2
            ;;
    esac
    rm -rf "$backup"
    exit "$status"
}
trap undo EXIT
trap 'exit 130' INT TERM HUP

# The workspace version is the bundle's both ways (scripts/macos-app.sh): Sparkle orders updates
# by CFBundleVersion. The lockfile follows it, with harbor's protocol crates at their version.
sed -i '' "s/^version = \".*\"/version = \"$version\"/" Cargo.toml
grep -q "^version = \"$version\"$" Cargo.toml || fail "could not set the version in Cargo.toml"
cargo update -q -w

app=$(SIGN="$identity" scripts/macos-app.sh release | tail -1)
plist="$app/Contents/Info.plist"
[ "$(plutil -extract CFBundleVersion raw "$plist")" = "$version" ] || fail "the bundle does not say $version"
[ "$(plutil -extract SUPublicEDKey raw "$plist" 2>/dev/null)" = "$public_key" ] \
    || fail "the bundle's SUPublicEDKey is not assets/sparkle-public-key.txt"
plutil -extract NSLocalNetworkUsageDescription raw "$plist" >/dev/null 2>&1 \
    || fail "the bundle has no NSLocalNetworkUsageDescription"

rm -rf "$out"
mkdir -p "$out/feed"

# Apple scans the app and issues a ticket; stapling puts the ticket in the bundle, so Gatekeeper
# can check it offline. The zip sent to Apple is only for the submission: the release zips are
# made from the stapled app below.
echo "Notarizing (usually a few minutes)…"
ditto -c -k --keepParent "$app" "$out/notarize.zip"
result=$(xcrun notarytool submit "$out/notarize.zip" --keychain-profile "$profile" --wait --output-format json || true)
rm "$out/notarize.zip"
status=$(plutil -extract status raw -o - - <<<"$result" 2>/dev/null || true)
if [ "$status" != "Accepted" ]; then
    id=$(plutil -extract id raw -o - - <<<"$result" 2>/dev/null || true)
    [ -z "$id" ] || xcrun notarytool log "$id" --keychain-profile "$profile" >&2 || true
    fail "notarization came back ${status:-without a status}"
fi
xcrun stapler staple -q "$app"
assessment=$(spctl --assess --type execute -vv "$app" 2>&1) && grep -qx "source=Notarized Developer ID" <<<"$assessment" \
    || fail "Gatekeeper does not accept the stapled app"

# ditto --keepParent preserves the bundle exactly; Homebrew, the installer and Sparkle unpack it
# the same way. Extended attributes stay behind: the signature and the stapled ticket live in
# files, and what macOS attaches on the build machine (provenance, quarantine) would be unpacked
# as the user's.
archive="DuckTable-$version.zip"
ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent "$app" "$out/$archive"
cp "$out/$archive" "$out/DuckTable.zip"
printf '%s\n' "$notes" > "$out/notes.md"

# The feed keeps its earlier items and builds binary deltas against them, so it starts from what
# the feed release holds. Notes named like the archive are the update's notes; embedded, the
# feed carries them itself.
if gh release view "$feed_release" --repo "$repo" >/dev/null 2>&1; then
    gh release download "$feed_release" --repo "$repo" --dir "$out/feed" --clobber
fi
cp "$out/$archive" "$out/feed/$archive"
[ -z "$section" ] || printf '%s\n' "$section" > "$out/feed/DuckTable-$version.md"
scripts/appcast.sh "$out/feed" >/dev/null
grep -q "<sparkle:version>$version</sparkle:version>" "$out/feed/appcast.xml" || fail "the feed does not offer $version"
[ -z "$section" ] || grep -q '<description' "$out/feed/appcast.xml" || fail "the feed carries no release notes"

if [ "$mode" = --dry-run ]; then
    echo "Dry run: $out"
    ls -la "$out" "$out/feed"
    exit 0
fi

# A draft is invisible until published, so it can be deleted if anything below fails. The tag
# does not exist on GitHub yet; it arrives with the push, and publishing uses it.
stage=drafted
gh release create "$tag" "$out/$archive" "$out/DuckTable.zip" \
    --repo "$repo" --title "DuckTable $version" --notes-file "$out/notes.md" --draft --latest=false >/dev/null
stage=committed
git commit -q -m "DuckTable $version" -- Cargo.toml Cargo.lock
git tag -a "$tag" -m "DuckTable $version"
git push -q --atomic origin main "$tag"
stage=pushed
gh release edit "$tag" --repo "$repo" --draft=false --latest=false --verify-tag >/dev/null
stage=published
# The feed release is a prerelease and never --latest, so /releases/latest stays harbor's. Files
# only: generate_appcast sets the archives it prunes aside in old_updates/, and a directory is not
# an asset; what it set aside stays on the feed release, unlisted.
gh release view "$feed_release" --repo "$repo" >/dev/null 2>&1 \
    || gh release create "$feed_release" --repo "$repo" --title "DuckTable update feed" --prerelease --latest=false \
        --notes "The Sparkle feed DuckTable checks for updates. Not a release: install from the newest DuckTable release." >/dev/null
find "$out/feed" -maxdepth 1 -type f -exec gh release upload "$feed_release" --repo "$repo" --clobber {} +
stage=fed
echo "Published $tag"
echo "Next: scripts/update-cask.sh $version opens the Homebrew tap's pull request."
