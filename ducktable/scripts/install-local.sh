#!/bin/sh
# Builds DuckTable from this checkout and installs it over whatever
# DuckTable.app is already there — the local counterpart to install.sh,
# which fetches a published release instead.
#
# Usage: scripts/install-local.sh [debug|release]   (default: release)
#        DUCKTABLE_DEST=~/Applications scripts/install-local.sh
#
# Release is the default because an unoptimized GPUI build misrepresents
# how the app performs; install debug only to keep a bundle around for
# the element inspector.
#
# A copy already running from the destination is quit and relaunched, so
# what is on screen afterwards is the build just installed. Uncommitted
# staged edits in that window go with it, exactly as ⌘Q would take them.
set -e
cd "$(dirname "$0")/.."

[ "$(uname -s)" = "Darwin" ] || { echo "DuckTable is a macOS app." >&2; exit 1; }

profile="${1:-release}"
case "$profile" in
    debug | release) ;;
    *)
        echo "Unknown profile '$profile' (want: debug, release)." >&2
        exit 1
        ;;
esac

# A plain command, never a pipeline: a pipeline reports the last stage's
# status, which would hide a failed build from set -e and carry an empty
# path into the replace below — taking the installed app with it and
# putting nothing back. macos-app.sh writes this one fixed path; the
# check keeps that coupling honest if it ever moves.
scripts/macos-app.sh "$profile" >/dev/null
app="target/DuckTable.app"
[ -d "$app" ] || { echo "No bundle at $app after building." >&2; exit 1; }

# A destination given explicitly is honored or refused, never quietly
# swapped for another — only the default falls back, for Macs where
# /Applications belongs to someone else.
if [ -n "${DUCKTABLE_DEST:-}" ]; then
    dest="$DUCKTABLE_DEST"
else
    dest="/Applications"
    [ -w "$dest" ] || [ ! -d "$dest" ] || dest="$HOME/Applications"
fi
mkdir -p "$dest"
[ -w "$dest" ] || { echo "$dest is not writable." >&2; exit 1; }
installed="$dest/DuckTable.app"

# Only ever the copy being replaced: signalling by pid, never by name.
# Every build shares one bundle id, so anything name-addressed — an
# AppleScript quit, `killall DuckTable` — is as likely to reach a dev
# copy running from target/ as the app installed here. comm= carries the
# full executable path, so -F matches it literally and a destination
# holding regex characters cannot slip past.
running_pids() {
    ps -A -o pid=,comm= | grep -F "$installed/Contents/MacOS/ducktable" | awk '{print $1}'
}

# A process whose bundle is replaced underneath it runs on deleted files
# and misbehaves until relaunched, so the old copy goes down first and
# comes back up at the end — running this script means wanting the new
# build in front of you. TERM is what ⌘Q already amounts to here: the app
# keeps no unsaved state but staged edits, and it discards those on quit
# without asking either way.
relaunch=""
pids=$(running_pids)
if [ -n "$pids" ]; then
    echo "Quitting DuckTable (pid $(echo "$pids" | paste -sd' ' -))..."
    kill $pids 2>/dev/null || true
    n=0
    while [ -n "$(running_pids)" ]; do
        n=$((n + 1))
        [ "$n" -gt 20 ] && {
            echo "DuckTable did not exit; quit it and re-run." >&2
            exit 1
        }
        sleep 0.5
    done
    relaunch=1
fi

# Stage beside the destination, then swap: the installed app stands until
# the copy is whole, so a ditto that dies partway leaves the Mac with the
# version it already had rather than none at all.
staged="$dest/.DuckTable.app.incoming"
aside="$dest/.DuckTable.app.outgoing"
trap 'rm -rf "$staged"' EXIT
rm -rf "$staged"
ditto "$app" "$staged"

# A swap that died between its two renames left the only copy set aside;
# it goes back before anything else, so it is never mistaken for debris.
if [ -e "$aside" ]; then
    [ -e "$installed" ] || mv "$aside" "$installed"
    rm -rf "$aside"
fi

# Replace, never merge: copying onto a bundle leaves the old version's
# orphans inside the new one, and writing over an executable macOS has
# already run gets the next launch killed. ditto carries bundle metadata
# that cp drops. The swap is two renames: the installed app steps aside,
# the staged one takes its name, and only then is the old one removed, so
# a rename that fails puts back the app that was there.
if [ -e "$installed" ]; then
    mv "$installed" "$aside"
    if ! mv "$staged" "$installed"; then
        mv "$aside" "$installed"
        echo "Could not move the build into $dest; the installed DuckTable is untouched." >&2
        exit 1
    fi
    rm -rf "$aside"
else
    mv "$staged" "$installed"
fi

# Launch Services learns of the bundle at this path at once, so Finder,
# Spotlight and System Settings show its name and icon without waiting for
# a rescan. Best effort: the app runs the same without it.
lsregister="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
if [ -x "$lsregister" ]; then
    "$lsregister" -f "$installed" >/dev/null 2>&1 || true
fi

echo "Installed $installed ($profile)"

# Back to where it was: relaunch only what this script took down, so a
# scripted install onto a Mac where DuckTable sat closed leaves it closed.
if [ -n "$relaunch" ]; then
    open "$installed"
fi
