#!/usr/bin/env bash
#
# install.sh — install DuckTable.app with one command (macOS, Apple Silicon):
#
#   curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/ducktable/scripts/install.sh | bash
#
# Installs from the newest ducktable-v* GitHub release — no signing, no
# ceremony. curl never sets macOS's quarantine attribute, so the app opens on
# first launch, and the bundle is ad-hoc signed as com.shreeve.ducktable.
#
# The app lands in /Applications, or ~/Applications where that is not
# writable; DUCKTABLE_DEST names another directory (... | DUCKTABLE_DEST=dir bash).
# An installed copy is replaced by rename, so a failed install leaves it be.
#
# Uninstall the same way — the app goes; your settings (~/.config/ducktable)
# and your databases stay:
#
#   curl -fsSL .../install.sh | bash -s -- --uninstall

set -euo pipefail

# Color only when stdout is a terminal, and never against NO_COLOR.
Color_Off='' Red='' Green='' Dim='' Bold_Green='' Bold_White=''
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    Color_Off='\033[0m'
    Red='\033[0;31m' Green='\033[0;32m' Dim='\033[0;2m'
    Bold_Green='\033[1;32m' Bold_White='\033[1m'
fi

info() { printf "${Dim}%s${Color_Off}\n" "$*"; }
fail() { printf "${Red}error${Color_Off}: %s\n" "$*" >&2; exit 1; }
tildify() { case "$1" in "$HOME"/*) printf '~%s' "${1#"$HOME"}" ;; *) printf '%s' "$1" ;; esac; }

# Remove only what install put down — the app bundle, wherever it landed.
# Settings and databases belong to the user, and an uninstaller that reaches
# for those is malware with a manual. No network: the filesystem answers
# what's installed.
uninstall() {
    removed=false
    # The same places install chooses from: the directory DUCKTABLE_DEST
    # names and no other, or else the two defaults.
    if [ -n "${DUCKTABLE_DEST:-}" ]; then
        set -- "$DUCKTABLE_DEST/DuckTable.app"
    else
        set -- "/Applications/DuckTable.app" "$HOME/Applications/DuckTable.app"
    fi
    for app in "$@"; do
        [ -d "$app" ] || continue
        rm -rf "$app" || fail "cannot remove $(tildify "$app")"
        printf "${Green}DuckTable was removed from ${Bold_Green}%s${Color_Off}\n" "$(tildify "$(dirname "$app")")"
        removed=true
    done
    $removed || fail "DuckTable is not installed ($(tildify "$(dirname "$1")")${2:+ or $(tildify "$(dirname "$2")")})"
    info "your settings ($(tildify "$HOME/.config/ducktable")) and your databases are untouched"
}

# Everything lives in main() so a truncated `curl | bash` download can
# never execute a half-delivered script.
main() {
    case "${1:-}" in
        --uninstall) uninstall; return ;;
    esac

    [ "$(uname -s)" = "Darwin" ] || fail "DuckTable is a macOS app."
    [ "$(uname -m)" = "arm64" ] || {
        printf "${Red}error${Color_Off}: %s\n" "The prebuilt DuckTable is Apple Silicon only (this Mac is $(uname -m))." >&2
        printf "${Dim}%s${Color_Off}\n" "Intel: clone the repo and run scripts/macos-app.sh release." >&2
        exit 1
    }

    # DuckTable shares its repo with harbor, and the repo's "latest" release is
    # harbor's — so resolve the highest ducktable-v* tag by version. sort -V,
    # not creation order: a re-released hotfix of an old line must not win.
    # (This script only runs on macOS, whose sort has -V.) The asset keeps one
    # name, so the tag is all that varies.
    tag=$(curl -fsSL --retry 3 --retry-delay 1 "https://api.github.com/repos/shreeve/duckdb-harbor/releases?per_page=100" \
          | grep -o '"tag_name": *"ducktable-v[0-9][0-9.]*"' | cut -d'"' -f4 | sort -V | tail -1)
    [ -n "$tag" ] || fail "could not find a ducktable-v* release"
    url="https://github.com/shreeve/duckdb-harbor/releases/download/$tag/DuckTable.zip"
    # DUCKTABLE_DEST, when given, is honored or refused, never quietly
    # swapped for another; only the default falls back, for Macs where
    # /Applications belongs to someone else.
    if [ -n "${DUCKTABLE_DEST:-}" ]; then
        dest="$DUCKTABLE_DEST"
    else
        dest="/Applications"
        [ -w "$dest" ] || dest="$HOME/Applications"
    fi
    mkdir -p "$dest"
    [ -w "$dest" ] || fail "$(tildify "$dest") is not writable"
    installed="$dest/DuckTable.app"
    staged="$dest/.DuckTable.app.incoming"
    aside="$dest/.DuckTable.app.outgoing"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp" "$staged"' EXIT
    info "DuckTable ($tag, Apple Silicon)"
    curl -fSL --retry 3 --retry-delay 1 --progress-bar "$url" -o "$tmp/DuckTable.zip"
    # ditto, not unzip: it preserves the bundle exactly as it was archived.
    ditto -x -k "$tmp/DuckTable.zip" "$tmp"

    # Stage beside the destination, so the swap below is two renames within
    # one directory. Everything that touches the bundle happens to the staged
    # copy: nothing is written into an app once it is in place.
    rm -rf "$staged"
    mv "$tmp/DuckTable.app" "$staged"
    # Belt and suspenders: if anything tagged the download, untag it. This
    # script only ever installs the zip it fetched itself, and curl sets no
    # quarantine, so there is no browser-downloaded bundle to refuse here.
    xattr -dr com.apple.quarantine "$staged" 2>/dev/null || true
    # A damaged download stops here, with the installed app still standing.
    codesign --verify --deep --strict "$staged" 2>/dev/null \
        || fail "the downloaded DuckTable.app does not verify; nothing was changed"

    # A swap that died between its two renames left the only copy set
    # aside; it goes back before anything else.
    if [ -e "$aside" ]; then
        [ -e "$installed" ] || mv "$aside" "$installed"
        rm -rf "$aside"
    fi

    # Replace, never merge, and never by deleting first: the installed app
    # steps aside, the staged one takes its name, and only then is the old
    # one removed. A rename that fails puts back the app that was there.
    if [ -e "$installed" ]; then
        mv "$installed" "$aside" || fail "cannot replace $(tildify "$installed")"
        if ! mv "$staged" "$installed"; then
            mv "$aside" "$installed"
            fail "cannot move DuckTable into $(tildify "$dest"); the installed copy is untouched"
        fi
        rm -rf "$aside"
    else
        mv "$staged" "$installed"
    fi

    # Launch Services learns of the bundle at this path at once, so Finder,
    # Spotlight and System Settings show its name and icon without waiting
    # for a rescan. Best effort: the app runs the same without it.
    lsregister="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
    if [ -x "$lsregister" ]; then
        "$lsregister" -f "$installed" >/dev/null 2>&1 || true
    fi

    printf "${Green}DuckTable was installed to ${Bold_Green}%s${Color_Off}\n" "$(tildify "$installed")"
    info "Run 'open -a DuckTable' to get started"
    info "DuckTable speaks to DuckDB Harbor: https://github.com/shreeve/duckdb-harbor"
}

main "$@"
