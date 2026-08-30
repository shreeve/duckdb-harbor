#!/usr/bin/env bash
#
# install.sh — install this harbor release from the extracted archive.
#
#   bin/harbor, bin/pilot -> ~/.local/bin   (override: BIN=...)
#   lib/libduckdb.*       -> ~/.local/lib   (override: LIB=...)
#
# Into your own home, so nothing here needs root. `~/.local/bin` is where the
# XDG base directory spec puts user executables, and Debian and Fedora already
# add it to PATH when it exists; macOS does not, so this says so rather than
# installing somewhere you cannot see.
#
# The binaries carry a relative rpath (../lib), so bin and lib travel as a
# pair — they also run straight out of this directory without installing.
#
# A system-wide install is BIN=/usr/local/bin LIB=/usr/local/lib with sudo in
# front of the whole command. This script never escalates on its own: a script
# that silently acquires root to write somewhere you did not ask for is a
# script you cannot reason about.

set -euo pipefail
cd "$(dirname "$0")"

BIN=${BIN:-$HOME/.local/bin}
LIB=${LIB:-$HOME/.local/lib}

fail() { printf 'install: %s\n' "$*" >&2; exit 1; }

for d in "$BIN" "$LIB"; do
  mkdir -p "$d" 2>/dev/null || fail "cannot create $d"
  [ -w "$d" ] || fail "$d is not writable by you — pick another BIN=/LIB=, or re-run the whole command under sudo"
done

# rm first, install second: macOS caches a binary's code signature per inode,
# and overwriting in place leaves every later exec SIGKILL'd against the stale
# cache. A fresh inode gets a fresh verdict; upgrades stay safe.
rm -f "$BIN/harbor" "$BIN/pilot"
rm -f "$LIB"/libduckdb.dylib "$LIB"/libduckdb.so
install -m 0755 bin/harbor bin/pilot "$BIN"
install -m 0755 lib/libduckdb.* "$LIB"

# Sockets and tokens live in the runtime dir. harbor heals these permissions on
# every run; doing it here covers a fleet that is currently stopped.
state="${HARBOR_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}/harbor}"
[ -d "$state/runtime" ] && chmod 700 "$state" "$state/runtime" 2>/dev/null || true

echo "installed: harbor + pilot -> $BIN"
echo "           libduckdb      -> $LIB"

case ":${PATH:-}:" in
  *":$BIN:"*) ;;
  *)
    echo
    echo "$BIN is not on your PATH. Add it:"
    echo "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc   # or ~/.bashrc"
    ;;
esac

echo
echo "try: harbor start mydata.duckdb --create && pilot mydata"
