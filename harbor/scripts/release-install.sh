#!/usr/bin/env bash
#
# install.sh — install this harbor release from the extracted archive.
#
#   bin/harbor      -> ~/.local/bin   (override: BIN=...)
#   lib/libduckdb.* -> ~/.local/lib   (override: LIB=...)
#
# Into your own home, so nothing here needs root. `~/.local/bin` is where the
# XDG base directory spec puts user executables, and Debian and Fedora already
# add it to PATH when it exists; macOS does not, so this says so rather than
# installing somewhere you cannot see.
#
# Nothing is linked — the engine loads on demand, and bin/../lib is the first
# place harbor looks — so bin and lib travel as a pair, and run straight out
# of this directory without installing.
#
# A system-wide install is BIN=/usr/local/bin LIB=/usr/local/lib with sudo in
# front of the whole command. This script never escalates on its own: a script
# that silently acquires root to write somewhere you did not ask for is a
# script you cannot reason about.

set -euo pipefail
cd "$(dirname "$0")"

BIN=${BIN:-$HOME/.local/bin}
LIB=${LIB:-$HOME/.local/lib}

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

for d in "$BIN" "$LIB"; do
  mkdir -p "$d" 2>/dev/null || fail "cannot create $d"
  [ -w "$d" ] || fail "$d is not writable by you — pick another BIN=/LIB=, or re-run the whole command under sudo"
done

# rm first, install second: macOS caches a binary's code signature per inode,
# and overwriting in place leaves every later exec SIGKILL'd against the stale
# cache. A fresh inode gets a fresh verdict; upgrades stay safe.
rm -f "$BIN/harbor"
rm -f "$LIB"/libduckdb.dylib "$LIB"/libduckdb.so
install -m 0755 bin/harbor "$BIN"
install -m 0755 lib/libduckdb.* "$LIB"

# Sockets and tokens live in the runtime dir. harbor heals these permissions on
# every run; doing it here covers a fleet that is currently stopped.
state="${HARBOR_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}/harbor}"
[ -d "$state/runtime" ] && chmod 700 "$state" "$state/runtime" 2>/dev/null || true

printf "${Green}harbor was installed successfully to ${Bold_Green}%s${Color_Off}\n" "$(tildify "$BIN")"
info "libduckdb -> $(tildify "$LIB")"

case ":${PATH:-}:" in
  *":$BIN:"*) ;;
  *)
    printf '\n'
    info "$(tildify "$BIN") is not on your PATH. Add it:"
    printf "  ${Bold_White}echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc${Color_Off}${Dim}   # or ~/.bashrc${Color_Off}\n"
    ;;
esac

printf '\n'
info "Run 'harbor mydata.duckdb' to get started"
