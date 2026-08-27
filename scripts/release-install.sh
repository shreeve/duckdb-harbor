#!/usr/bin/env bash
#
# install.sh — install this harbor release from the extracted archive.
#
#   bin/harbor, bin/pilot -> /usr/local/bin   (override: BIN=...)
#   lib/libduckdb.*       -> /usr/local/lib   (override: LIB=...)
#   ui.duckdb_extension   -> ~/.duckdb/extensions/<version>/<platform>/
#
# The binaries carry a relative rpath (../lib), so bin + lib travel as a pair —
# they also run straight out of this directory without installing. The ui
# extension goes where `LOAD ui` resolves it by name; ENGINE (written at
# package time) names the exact <version>/<platform> it was built for.
# sudo is used only if the system dirs are root-owned.

set -euo pipefail
cd "$(dirname "$0")"

BIN=${BIN:-/usr/local/bin}
LIB=${LIB:-/usr/local/lib}
read -r version ext_plat < ENGINE

as_owner() { if [ -w "$(dirname "$1")" ] || [ -w "$1" ]; then "${@:2}"; else sudo "${@:2}"; fi; }
as_owner "$BIN" install -d -m 0755 "$BIN"
as_owner "$LIB" install -d -m 0755 "$LIB"
# rm first, install second: macOS caches a binary's code signature per inode,
# and overwriting in place leaves every later exec SIGKILL'd against the stale
# cache. A fresh inode gets a fresh verdict; upgrades stay safe.
as_owner "$BIN" rm -f "$BIN/harbor" "$BIN/pilot"
as_owner "$LIB" rm -f "$LIB"/libduckdb.dylib "$LIB"/libduckdb.so
as_owner "$BIN" install -m 0755 bin/harbor bin/pilot "$BIN"
as_owner "$LIB" install -m 0755 lib/libduckdb.* "$LIB"

# The extension belongs to the *invoking user's* ~/.duckdb even when the whole
# script is run under sudo — a root-owned ~/.duckdb breaks the ui extension,
# whose init mkdirs extension_data there as the user.
user=${SUDO_USER:-$(id -un)}
home=$(eval echo "~$user")
ext_dir="$home/.duckdb/extensions/$version/$ext_plat"
mkdir -p "$ext_dir"
install -m 0644 extensions/ui.duckdb_extension "$ext_dir/ui.duckdb_extension"
if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ]; then chown -R "$user" "$home/.duckdb"; fi

# Heal the runtime dir's permissions: sockets and tokens live in
# $HARBOR_HOME/runtime, and a dir made earlier by hand (or a sloppy
# umask) must not stay world-listable. harbor also heals this on every
# run; doing it here covers a fleet that is stopped.
hh="${HARBOR_HOME:-$home/.config/harbor}/runtime"
if [ -d "$hh" ]; then chmod 700 "$hh" 2>/dev/null || true; fi

echo "installed: harbor + pilot -> $BIN, libduckdb -> $LIB"
echo "           ui extension   -> $ext_dir  (engine $version, owner $user)"
echo "try: harbor add mydata.duckdb --unsigned --init 'LOAD ui; FROM start_ui_server();'"
